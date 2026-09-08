//! BM25 over object storage.
//!
//! ⚠️ **The scorer takes its statistics rather than computing them.** IDF is a property of the
//! *corpus*, not of a segment: a term common in one segment and rare in another gets two
//! different IDFs, and merging per-segment top-k then ranks by numbers that were never
//! comparable. D-30's remedy is two-pass — gather per-segment DF summaries in RT-A, score in
//! RT-B — and it costs no round trip because the summaries live in the dictionary sidecar,
//! which is fetched beside the footer anyway.
//!
//! ⚠️ **The caller that gathers them across segments does not exist yet**, and that is stated
//! rather than implied: `pstore_query::query` takes one key, and a stable cross-segment
//! identity is the thing M5a and M5b both handed forward. What this module builds is the half
//! that makes such a caller possible, and [`Stats::merge`] is the whole of the gathering.

use pstore_blob::{BlobStore, Key};
use pstore_format::text::{TermDict, TermEntry, dict_key};
use pstore_format::{FormatError, Section, Segment};

/// BM25's term-frequency saturation.
pub const K1: f32 = 1.2;
/// BM25's length-normalisation weight.
pub const B: f32 = 0.75;

/// Corpus statistics — the input to IDF, and the thing that must not come from one segment.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Stats {
    /// Documents in the corpus.
    pub doc_count: u64,
    /// Tokens across every document, so `avgdl` is a division rather than a scan.
    pub total_tokens: u64,
    /// Documents containing each term.
    pub df: std::collections::BTreeMap<String, u32>,
}

impl Stats {
    /// Sums several segments' summaries.
    ///
    /// ⚠️ Addition, and that is the point: gathering global statistics is not a query, a join
    /// or a second pass over the data — it is adding up numbers each segment already carries
    /// in the object a cold query fetches anyway.
    #[must_use]
    pub fn merge(parts: impl IntoIterator<Item = Self>) -> Self {
        let mut out = Self::default();
        for p in parts {
            out.doc_count += p.doc_count;
            out.total_tokens += p.total_tokens;
            for (term, df) in p.df {
                *out.df.entry(term).or_insert(0) += df;
            }
        }
        out
    }

    /// Average document length. Zero for an empty corpus, which makes the norm `k1·(1−b)`.
    #[must_use]
    pub fn avgdl(&self) -> f32 {
        if self.doc_count == 0 {
            return 0.0;
        }
        #[expect(
            clippy::cast_precision_loss,
            reason = "a corpus beyond 2^24 tokens does not need this to the token"
        )]
        let v = self.total_tokens as f32 / self.doc_count as f32;
        v
    }
}

/// An opened text index: the segment's directory and the term dictionary.
#[derive(Debug)]
pub struct TextIndex {
    dict: TermDict,
    postings: std::ops::Range<u64>,
    norms: std::ops::Range<u64>,
}

impl TextIndex {
    /// Opens a text index. **One round trip**: the dictionary is a different object from the
    /// segment, so it is fetched *beside* the footer rather than after it.
    pub async fn open<S: BlobStore>(store: &S, key: &Key) -> Result<Self, FormatError> {
        let (seg, raw) = futures_util::future::join(
            Segment::open(store, key),
            store.get_immutable(&dict_key(key), pstore_blob::Class::Pinned),
        )
        .await;
        let raw = raw.map_err(|e| FormatError::Blob(e.to_string()))?;
        Self::from_segment(&seg?, raw.as_ref())
    }

    /// Opens a text index over an **already-open** segment, so a hybrid query opens once.
    pub fn from_segment(segment: &Segment, dictionary: &[u8]) -> Result<Self, FormatError> {
        // ⚠️ An error, not an empty index. A segment with no text answering every query with
        // nothing is indistinguishable from a corpus with no matches, so a caller could not
        // tell an unindexed field from an unlucky query.
        if !segment.has_text() {
            return Err(FormatError::UnknownField);
        }
        let postings = segment
            .section(Section::TextPostings)
            .ok_or(FormatError::Corrupt("text with no postings section"))?;
        let norms = segment
            .section(Section::Fieldnorms)
            .ok_or(FormatError::Corrupt("text with no fieldnorms section"))?;
        let dict = TermDict::decode(dictionary)
            .ok_or(FormatError::Corrupt("the term dictionary did not decode"))?;
        Ok(Self {
            dict,
            postings,
            norms,
        })
    }

    /// This segment's contribution to the corpus statistics, from the dictionary alone.
    #[must_use]
    pub fn summary(&self) -> Stats {
        Stats {
            doc_count: u64::from(self.dict.doc_count()),
            total_tokens: self.dict.total_tokens(),
            df: self
                .dict
                .terms()
                .filter_map(|t| self.dict.lookup(t).map(|e| (t.to_owned(), e.df)))
                .collect(),
        }
    }

    /// Where the postings section lives, for a byte assertion that can name a section.
    #[must_use]
    pub fn span(&self) -> std::ops::Range<u64> {
        self.postings.clone()
    }

    /// The dictionary this index was opened with.
    #[must_use]
    pub fn dictionary(&self) -> &TermDict {
        &self.dict
    }

    /// Bytes of postings the query's own lists occupy.
    #[must_use]
    pub fn list_bytes(&self, terms: &[String]) -> u64 {
        self.entries(terms)
            .iter()
            .map(|(_, e)| u64::from(e.bytes))
            .sum()
    }

    /// The dictionary entries a query touches, in ascending byte order.
    fn entries(&self, terms: &[String]) -> Vec<(String, TermEntry)> {
        let mut out: Vec<(String, TermEntry)> = terms
            .iter()
            .filter_map(|t| self.dict.lookup(t).map(|e| (t.clone(), e)))
            .collect();
        out.sort_by_key(|(_, e)| e.offset);
        out.dedup_by(|a, b| a.0 == b.0);
        out
    }

    /// The `k` best rows for a bag of terms, scored by BM25 against **global** `stats`.
    ///
    /// **One further round trip**, whatever the term count: the postings and the fieldnorms
    /// are different sections of the same object and both spans are known before either is
    /// issued.
    pub async fn search<S: BlobStore>(
        &self,
        store: &S,
        key: &Key,
        terms: &[String],
        stats: &Stats,
        k: usize,
    ) -> Result<Vec<(usize, f32)>, FormatError> {
        let entries = self.entries(terms);
        if entries.is_empty() {
            return Ok(Vec::new());
        }
        let mut ranges: Vec<std::ops::Range<u64>> = entries
            .iter()
            .map(|(_, e)| {
                let lo = self.postings.start + e.offset;
                lo..lo + u64::from(e.bytes)
            })
            .collect();
        // ⚠️ The fieldnorms ride WITH the postings. They are a different section, their span
        // is known at open, and awaiting them separately would make BM25's length
        // normalisation cost a round trip that the formula does not.
        ranges.push(self.norms.clone());
        let bufs = store
            .get_ranges_as(key, &ranges, pstore_blob::Class::Bulk)
            .await?;
        let norms = bufs.last().cloned().unwrap_or_default();

        // ⚠️ One `#[expect]` for the whole scorer rather than a cast dance per term: BM25 is
        // arithmetic on counts, every count here is far below 2^24, and writing it any other
        // way obscured the formula the oracle is compared against.
        #[expect(
            clippy::cast_precision_loss,
            reason = "BM25 is arithmetic on counts; all of them are far below f32's exact range"
        )]
        {
            let n = stats.doc_count as f32;
            let avgdl = stats.avgdl();
            let mut scores: std::collections::HashMap<u32, f32> = std::collections::HashMap::new();
            for (i, (term, e)) in entries.iter().enumerate() {
                let Some(buf) = bufs.get(i) else { continue };
                // ⚠️ `stats.df`, never `e.df`. The entry's df is THIS SEGMENT's, which is the
                // whole bug D-30 exists to prevent: a term common here and rare elsewhere gets
                // an IDF that is not comparable with any other segment's. Falling back to the
                // entry's own df is what a single-segment caller does, and it is the same
                // number then.
                let df = f32::from(0u8) + *stats.df.get(term).unwrap_or(&e.df) as f32;
                let idf = (1.0 + (n - df + 0.5) / (df + 0.5)).ln();
                for (row, tf) in self.dict.decode_list(e, buf) {
                    let len =
                        pstore_format::text::norm_at(&norms, row as usize).unwrap_or(0) as f32;
                    let norm = K1 * (1.0 - B + B * len / if avgdl > 0.0 { avgdl } else { 1.0 });
                    let tf = tf as f32;
                    *scores.entry(row).or_insert(0.0) += idf * (tf * (K1 + 1.0)) / (tf + norm);
                }
            }
            let mut scored: Vec<(usize, f32)> =
                scores.into_iter().map(|(r, s)| (r as usize, s)).collect();
            scored.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            scored.truncate(k);
            Ok(scored)
        }
    }
}
