//! Full-text: an analyzer, a term dictionary that carries document frequency, and postings
//! whose impact is an **exact** term frequency.
//!
//! ⚠️ **The text stays in the document's attributes**, and that is what makes compaction
//! possible. Postings cannot be inverted back into text — order, duplicates and dropped
//! tokens are gone — so a merge that had only the postings would rewrite every document as a
//! bag of words. `Segment::scan` already returns attributes, so a compaction re-analyzes the
//! original string and nothing in the read path had to change.
//!
//! ⚠️ **No `Fields` row.** That table describes *vector* fields, whose layout a reader must
//! know to decode them. A text field's presence is answered by whether its section exists,
//! and giving it a row would hand the postings to `decode_field`, which reads them as `f32`
//! and returns a dense field of noise.

use crate::sparse::{Entry, ImpactEncoding};
use crate::{Document, Value};

/// The attribute a text index is built over.
///
/// ⚠️ A placeholder for a schema, exactly as [`crate::DEFAULT_FIELD`] was the placeholder for
/// named vector fields — and replaced by the same thing: a schema in HEAD saying which fields
/// are indexed, which is M6's catalog. Until then a segment has one text field, so its
/// fieldnorm is unambiguous and there is no second field to lose silently.
pub const DEFAULT_TEXT_FIELD: &str = "text";

/// Tokens, lowercased, split on anything that is not alphanumeric.
///
/// ⚠️ Deliberately this small. Stemming, stopwords and language rules are quality knobs, and
/// a knob chosen without an eval set is noise. What analysis **must** be is a pure function of
/// the text: changing it changes every posting, so it is a reindex rather than a setting.
#[must_use]
pub fn analyze(s: &str) -> Vec<String> {
    s.split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(str::to_lowercase)
        .collect()
}

/// The three byte strings a text field becomes.
#[derive(Debug, Clone, Default)]
pub struct Built {
    /// The `TextPostings` section.
    pub postings: Vec<u8>,
    /// The term dictionary sidecar.
    pub dictionary: Vec<u8>,
    /// One token count per **segment row**, including rows with no text.
    pub fieldnorms: Vec<u32>,
}

/// Fieldnorms as the `Fieldnorms` section's bytes.
#[must_use]
pub fn encode_norms(norms: &[u32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(norms.len() * 4);
    for n in norms {
        out.extend_from_slice(&n.to_le_bytes());
    }
    out
}

/// One row's token count, from the `Fieldnorms` section's bytes.
#[must_use]
pub fn norm_at(raw: &[u8], row: usize) -> Option<u32> {
    raw.get(row * 4..row * 4 + 4)
        .and_then(|b| b.try_into().ok())
        .map(u32::from_le_bytes)
}

const MAGIC: &[u8; 8] = b"PSTORETD";
const VERSION: u16 = 1;
/// term_at(4) term_len(2) offset(4) bytes(4) count(4) df(4).
const ENTRY: usize = 22;
/// MAGIC(8) VERSION(2) terms(4) blob_len(4) rows(4) total_tokens(8).
///
/// ⚠️ `rows` and `total_tokens` are the **corpus summary** D-30 needs, and they live here
/// rather than being derived from the fieldnorms section on purpose: the sidecar is fetched
/// beside the footer, so a query that needs global statistics gets this segment's
/// contribution in the round that was happening anyway. Deriving them from the fieldnorms
/// would put a fetch between the open and the score, and two-pass IDF would cost a round
/// trip — which is exactly what D-30 says it does not.
const HEADER: usize = 8 + 2 + 4 + 4 + 4 + 8;

/// Where a segment's term dictionary lives, **derived** from the segment's own key.
///
/// ⚠️ Derived, never discovered — the same rule as the sparse sidecar, for the same reason: a
/// compaction has to reach the dictionary of every segment it merges, and a LIST to find one
/// is priced like a PUT and caps at 1,000 keys.
#[must_use]
pub fn dict_key(segment: &pstore_blob::Key) -> pstore_blob::Key {
    pstore_blob::Key::new(format!("{}.tdict", segment.as_str()))
}

/// Builds the postings, the dictionary and the fieldnorms for one text attribute.
///
/// ⚠️ `docs` is taken in **segment row order**, and a document with no text still occupies a
/// row. Skipping it would move every later row by one, and every hit afterwards would name the
/// wrong document — with scores that are internally consistent and a top-k that looks fine.
#[must_use]
pub fn build(docs: &[Document], field: &str) -> Built {
    let mut by_term: std::collections::BTreeMap<String, Vec<(u32, u32)>> =
        std::collections::BTreeMap::new();
    let mut fieldnorms = Vec::with_capacity(docs.len());
    for (row, d) in docs.iter().enumerate() {
        let tokens = match d.attrs.get(field) {
            Some(Value::Str(s)) => analyze(s),
            _ => Vec::new(),
        };
        fieldnorms.push(tokens.len() as u32);
        let mut tf: std::collections::BTreeMap<&str, u32> = std::collections::BTreeMap::new();
        for t in &tokens {
            *tf.entry(t.as_str()).or_insert(0) += 1;
        }
        for (term, n) in tf {
            by_term
                .entry(term.to_owned())
                .or_default()
                .push((row as u32, n));
        }
    }

    let mut postings: Vec<u8> = Vec::new();
    let mut terms: Vec<u8> = Vec::new();
    let mut entries: Vec<(u32, u16, Entry, u32)> = Vec::with_capacity(by_term.len());
    for (term, list) in by_term {
        let offset = postings.len() as u64;
        // ⚠️ `Varint`, not `U8`. A term frequency is an integer, and `U8`'s per-term scale is
        // a quantization: a tf of 3 in a list whose maximum is 4 decodes as 2.99, BM25
        // saturates it differently, and the oracle disagrees for a reason no test names.
        #[expect(
            clippy::cast_precision_loss,
            reason = "a term frequency is exact in f32 to 2^24, which the encoding promises"
        )]
        let as_impacts: Vec<(u32, f32)> = list.iter().map(|(r, tf)| (*r, *tf as f32)).collect();
        crate::sparse::write_list(&mut postings, &as_impacts, ImpactEncoding::Varint, 0.0);
        let term_at = terms.len() as u32;
        terms.extend_from_slice(term.as_bytes());
        entries.push((
            term_at,
            term.len() as u16,
            Entry {
                dim: 0,
                offset,
                bytes: (postings.len() as u64 - offset) as u32,
                count: list.len() as u32,
                max_impact: 0.0,
            },
            // ⚠️ Document frequency, not posting count -- which happen to be equal here only
            // because a row contributes at most one posting per term. Stated because a change
            // that made postings per-position would silently make `df` mean something else.
            list.len() as u32,
        ));
    }

    let mut dictionary = Vec::with_capacity(HEADER + entries.len() * ENTRY + terms.len());
    dictionary.extend_from_slice(MAGIC);
    dictionary.extend_from_slice(&VERSION.to_le_bytes());
    dictionary.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    dictionary.extend_from_slice(&(terms.len() as u32).to_le_bytes());
    dictionary.extend_from_slice(&(fieldnorms.len() as u32).to_le_bytes());
    dictionary.extend_from_slice(
        &fieldnorms
            .iter()
            .map(|n| u64::from(*n))
            .sum::<u64>()
            .to_le_bytes(),
    );
    for (term_at, term_len, e, df) in &entries {
        dictionary.extend_from_slice(&term_at.to_le_bytes());
        dictionary.extend_from_slice(&term_len.to_le_bytes());
        dictionary.extend_from_slice(&(e.offset as u32).to_le_bytes());
        dictionary.extend_from_slice(&e.bytes.to_le_bytes());
        dictionary.extend_from_slice(&e.count.to_le_bytes());
        dictionary.extend_from_slice(df.to_le_bytes().as_slice());
    }
    dictionary.extend_from_slice(&terms);

    Built {
        postings,
        dictionary,
        fieldnorms,
    }
}

/// Where one term's postings are, and how many documents contain it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TermEntry {
    /// Byte offset into the `TextPostings` section.
    pub offset: u64,
    /// Bytes the list occupies — its exact fetch range.
    pub bytes: u32,
    /// Postings in the list.
    pub count: u32,
    /// **Documents** containing the term, in this segment. The input to IDF.
    pub df: u32,
}

/// The term dictionary: sorted terms, fixed-width entries, and a blob of term bytes.
#[derive(Debug, Clone)]
pub struct TermDict {
    terms: Vec<String>,
    entries: Vec<TermEntry>,
    rows: u32,
    total_tokens: u64,
}

impl TermDict {
    /// Decodes a dictionary, returning `None` for anything malformed.
    #[must_use]
    pub fn decode(raw: &[u8]) -> Option<Self> {
        if raw.get(..8)? != MAGIC || u16::from_le_bytes(raw.get(8..10)?.try_into().ok()?) != VERSION
        {
            return None;
        }
        let count = u32::from_le_bytes(raw.get(10..14)?.try_into().ok()?) as usize;
        let terms_len = u32::from_le_bytes(raw.get(14..18)?.try_into().ok()?) as usize;
        let rows = u32::from_le_bytes(raw.get(18..22)?.try_into().ok()?);
        let total_tokens = u64::from_le_bytes(raw.get(22..30)?.try_into().ok()?);
        // ⚠️ Length checked up front. A truncated table decoded lazily answers some lookups
        // and silently loses the terms past the cut.
        if raw.len() != HEADER + count * ENTRY + terms_len {
            return None;
        }
        let blob = raw.get(HEADER + count * ENTRY..)?;
        let mut terms = Vec::with_capacity(count);
        let mut entries = Vec::with_capacity(count);
        for i in 0..count {
            let b = raw.get(HEADER + i * ENTRY..HEADER + (i + 1) * ENTRY)?;
            let at = u32::from_le_bytes(b.get(0..4)?.try_into().ok()?) as usize;
            let len = u16::from_le_bytes(b.get(4..6)?.try_into().ok()?) as usize;
            terms.push(
                std::str::from_utf8(blob.get(at..at + len)?)
                    .ok()?
                    .to_owned(),
            );
            entries.push(TermEntry {
                offset: u64::from(u32::from_le_bytes(b.get(6..10)?.try_into().ok()?)),
                bytes: u32::from_le_bytes(b.get(10..14)?.try_into().ok()?),
                count: u32::from_le_bytes(b.get(14..18)?.try_into().ok()?),
                df: u32::from_le_bytes(b.get(18..22)?.try_into().ok()?),
            });
        }
        // Unsorted cannot be searched, and searching it anyway answers "absent" for terms that
        // are present — a silent recall loss rather than a failure.
        if !terms.is_sorted_by(|a, b| a < b) {
            return None;
        }
        Some(Self {
            terms,
            entries,
            rows,
            total_tokens,
        })
    }

    /// Documents this segment holds, text or not — the `N` of IDF's numerator.
    #[must_use]
    pub fn doc_count(&self) -> u32 {
        self.rows
    }

    /// Tokens across every document, so `avgdl` is a sum rather than a scan.
    #[must_use]
    pub fn total_tokens(&self) -> u64 {
        self.total_tokens
    }

    /// Distinct terms.
    #[must_use]
    pub fn len(&self) -> usize {
        self.terms.len()
    }

    /// Whether the field has no postings at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.terms.is_empty()
    }

    /// Every term, ascending.
    pub fn terms(&self) -> impl Iterator<Item = &str> {
        self.terms.iter().map(String::as_str)
    }

    /// One term's entry, or `None` when the vocabulary does not contain it.
    ///
    /// ⚠️ `None` is the **common** case, not an error: a query term the corpus never used
    /// contributes nothing and must cost nothing. `binary_search` rather than a hand-written
    /// loop, for the reason `sparse::Dictionary::lookup` gives.
    #[must_use]
    pub fn lookup(&self, term: &str) -> Option<TermEntry> {
        let i = self.terms.binary_search_by(|t| t.as_str().cmp(term)).ok()?;
        self.entries.get(i).copied()
    }

    /// Decodes one posting list from exactly the bytes its entry addresses.
    #[must_use]
    pub fn decode_list(&self, entry: &TermEntry, raw: &[u8]) -> Vec<(u32, u32)> {
        crate::sparse::read_list(raw, entry.count, ImpactEncoding::Varint, 0.0)
            .into_iter()
            .map(|(row, tf)| {
                #[expect(
                    clippy::cast_possible_truncation,
                    clippy::cast_sign_loss,
                    reason = "a varint impact is an exact u32 on the way in and out"
                )]
                let tf = tf as u32;
                (row, tf)
            })
            .collect()
    }
}
