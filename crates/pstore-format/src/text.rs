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

/// A language the Snowball stemmers cover (M14).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[allow(missing_docs, reason = "each variant is the language it names")]
pub enum Language {
    Arabic,
    Danish,
    Dutch,
    #[default]
    English,
    Finnish,
    French,
    German,
    Greek,
    Hungarian,
    Italian,
    Norwegian,
    Portuguese,
    Romanian,
    Russian,
    Spanish,
    Swedish,
    Tamil,
    Turkish,
}

impl Language {
    /// Every language, in the order their names sort.
    pub const ALL: [Self; 18] = [
        Self::Arabic,
        Self::Danish,
        Self::Dutch,
        Self::English,
        Self::Finnish,
        Self::French,
        Self::German,
        Self::Greek,
        Self::Hungarian,
        Self::Italian,
        Self::Norwegian,
        Self::Portuguese,
        Self::Romanian,
        Self::Russian,
        Self::Spanish,
        Self::Swedish,
        Self::Tamil,
        Self::Turkish,
    ];

    /// Its wire name: lowercase English.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Arabic => "arabic",
            Self::Danish => "danish",
            Self::Dutch => "dutch",
            Self::English => "english",
            Self::Finnish => "finnish",
            Self::French => "french",
            Self::German => "german",
            Self::Greek => "greek",
            Self::Hungarian => "hungarian",
            Self::Italian => "italian",
            Self::Norwegian => "norwegian",
            Self::Portuguese => "portuguese",
            Self::Romanian => "romanian",
            Self::Russian => "russian",
            Self::Spanish => "spanish",
            Self::Swedish => "swedish",
            Self::Tamil => "tamil",
            Self::Turkish => "turkish",
        }
    }

    /// The language a wire name names, or `None`.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|l| l.name() == name)
    }

    fn algorithm(self) -> rust_stemmers::Algorithm {
        use rust_stemmers::Algorithm as A;
        match self {
            Self::Arabic => A::Arabic,
            Self::Danish => A::Danish,
            Self::Dutch => A::Dutch,
            Self::English => A::English,
            Self::Finnish => A::Finnish,
            Self::French => A::French,
            Self::German => A::German,
            Self::Greek => A::Greek,
            Self::Hungarian => A::Hungarian,
            Self::Italian => A::Italian,
            Self::Norwegian => A::Norwegian,
            Self::Portuguese => A::Portuguese,
            Self::Romanian => A::Romanian,
            Self::Russian => A::Russian,
            Self::Spanish => A::Spanish,
            Self::Swedish => A::Swedish,
            Self::Tamil => A::Tamil,
            Self::Turkish => A::Turkish,
        }
    }
}

/// How text becomes terms (M14): an index's, fixed in its schema when the index is created.
///
/// ⚠️ **The default is the analyzer before M14**, byte for byte: split on anything that is
/// not alphanumeric, lowercase, nothing else. Every index that declares nothing, and every
/// segment written before M14, was analyzed that way -- so it is not a default that can
/// ever change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "each is an independent option of the wire's `full_text_search`"
)]
pub struct Analyzer {
    /// The stemmer's language, and what `remove_stopwords` needs to be English.
    pub language: Language,
    /// Reduce each token to its Snowball stem.
    pub stemming: bool,
    /// Drop the Lucene English stopwords.
    pub remove_stopwords: bool,
    /// Keep case rather than lowercasing.
    pub case_sensitive: bool,
    /// NFKD, then drop combining marks: `é` becomes `e`.
    pub ascii_folding: bool,
    /// How a string becomes tokens (M158).
    pub tokenizer: Tokenizer,
}

/// How a string becomes tokens (M158): the vendor's `tokenizer` option.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Tokenizer {
    /// Maximal runs of alphanumeric characters, as every analyzer before M158.
    #[default]
    WordV1,
    /// The client's own tokens: a document's string is one token, kept byte for byte, and a
    /// query operand is its tokens joined by [`TOKEN_SEPARATOR`].
    PreTokenizedArray,
}

/// What joins a `[]string` query operand's tokens into one string (M158): the ASCII unit
/// separator, which no query token may hold.
pub const TOKEN_SEPARATOR: char = '\u{1F}';

/// Lucene's English stopword set.
const STOPWORDS: [&str; 33] = [
    "a", "an", "and", "are", "as", "at", "be", "but", "by", "for", "if", "in", "into", "is", "it",
    "no", "not", "of", "on", "or", "such", "that", "the", "their", "then", "there", "these",
    "they", "this", "to", "was", "will", "with",
];

/// An analyzer and the BM25 parameters that score its terms (M14): all of what an index's
/// schema says about full text.
///
/// ⚠️ **Equal by bits**, so equality is an equivalence: a declaration is compared with the
/// stored schema after both are `f32`, which is what makes re-declaring `1.2` equal to it.
#[derive(Debug, Clone, Copy)]
pub struct FullText {
    /// How text becomes terms.
    pub analyzer: Analyzer,
    /// BM25's term-frequency saturation, in `[0, 3]`.
    pub k1: f32,
    /// BM25's length normalisation, in `[0, 1]`.
    pub b: f32,
}

impl PartialEq for FullText {
    fn eq(&self, other: &Self) -> bool {
        self.analyzer == other.analyzer
            && self.k1.to_bits() == other.k1.to_bits()
            && self.b.to_bits() == other.b.to_bits()
    }
}

impl Eq for FullText {}

impl Default for FullText {
    fn default() -> Self {
        Self {
            analyzer: Analyzer::default(),
            k1: 1.2,
            b: 0.75,
        }
    }
}

impl FullText {
    /// A single line [`Self::decode`] reads back exactly: `k1` and `b` as their bits, so a
    /// declaration compares equal to what was stored.
    #[must_use]
    pub fn encode(&self) -> String {
        let a = &self.analyzer;
        // M158: today's line, byte for byte, for `word_v1`; a `v2` line, which a build before
        // M158 refuses, for any other tokenizer.
        let line = format!(
            "v1;{};{}{}{}{};{:08x};{:08x}",
            a.language.name(),
            u8::from(a.stemming),
            u8::from(a.remove_stopwords),
            u8::from(a.case_sensitive),
            u8::from(a.ascii_folding),
            self.k1.to_bits(),
            self.b.to_bits()
        );
        match a.tokenizer {
            Tokenizer::WordV1 => line,
            Tokenizer::PreTokenizedArray => {
                format!(
                    "v2{};pre_tokenized_array",
                    line.get(2..).unwrap_or_default()
                )
            }
        }
    }

    /// The full-text schema [`Self::encode`] wrote, or `None` for anything else.
    #[must_use]
    pub fn decode(s: &str) -> Option<Self> {
        let mut parts = s.split(';');
        let tokenizer = match parts.next()? {
            "v1" => Tokenizer::WordV1,
            "v2" => match s.rsplit(';').next()? {
                "pre_tokenized_array" => Tokenizer::PreTokenizedArray,
                _ => return None,
            },
            _ => return None,
        };
        let language = Language::parse(parts.next()?)?;
        let flags: Vec<bool> = parts
            .next()?
            .chars()
            .map(|c| match c {
                '0' => Some(false),
                '1' => Some(true),
                _ => None,
            })
            .collect::<Option<_>>()?;
        let [stemming, remove_stopwords, case_sensitive, ascii_folding] = flags.as_slice() else {
            return None;
        };
        let bits = |p: Option<&str>| p.and_then(|h| u32::from_str_radix(h, 16).ok());
        let k1 = f32::from_bits(bits(parts.next())?);
        let b = f32::from_bits(bits(parts.next())?);
        // A `v2` line names its tokenizer last, read above.
        if tokenizer != Tokenizer::WordV1 {
            parts.next()?;
        }
        if parts.next().is_some() {
            return None;
        }
        Some(Self {
            analyzer: Analyzer {
                language,
                stemming: *stemming,
                remove_stopwords: *remove_stopwords,
                case_sensitive: *case_sensitive,
                ascii_folding: *ascii_folding,
                tokenizer,
            },
            k1,
            b,
        })
    }
}

/// The terms `s` becomes under `a`: split, case, fold, stopwords, stem -- in that order.
///
/// ⚠️ Stopwords compare after case and folding, so a `case_sensitive` index keeps `The`, and
/// Snowball stems lowercase input, so it leaves `Running` whole there. Stated in the spec,
/// not repaired: repairing it is a different analyzer, and analyzers are fixed per index.
#[must_use]
pub fn analyze(a: &Analyzer, s: &str) -> Vec<String> {
    tokens(a, s).into_iter().filter_map(|t| t.term).collect()
}

/// The terms a query operand `q` becomes under `a` (M158): [`analyze`], except that under
/// `pre_tokenized_array` `q` is tokens joined by [`TOKEN_SEPARATOR`], each one term, whole.
#[must_use]
pub fn analyze_query(a: &Analyzer, q: &str) -> Vec<String> {
    query_tokens(a, q)
        .into_iter()
        .filter_map(|t| t.term)
        .collect()
}

/// [`tokens`] of a query operand (M158): under `pre_tokenized_array`, each non-empty piece
/// between [`TOKEN_SEPARATOR`]s, with its span.
#[must_use]
pub fn query_tokens(a: &Analyzer, q: &str) -> Vec<Token> {
    if a.tokenizer != Tokenizer::PreTokenizedArray {
        return tokens(a, q);
    }
    let mut out = Vec::new();
    let mut from = 0;
    for piece in q.split(TOKEN_SEPARATOR) {
        let to = from + piece.len();
        if !piece.is_empty() {
            out.push(Token {
                span: from..to,
                term: Some(piece.to_owned()),
            });
        }
        from = to + TOKEN_SEPARATOR.len_utf8();
    }
    out
}

/// One token of a text (M63): the byte span it came from, and the term the analyzer made of
/// it -- `None` for a stopword, which has a span and no term.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    /// The token's bytes in the input: one maximal run of alphanumeric characters.
    pub span: std::ops::Range<usize>,
    /// What [`analyze`] returns for it, or `None` when the analyzer drops it.
    pub term: Option<String>,
}

/// [`analyze`] with the span of each token (M63): `analyze` is the terms of this.
///
/// ⚠️ **One pipeline.** Highlighting must mark exactly the tokens BM25 matched, so the two
/// cannot be separate implementations of "split, fold, drop, stem".
#[must_use]
pub fn tokens(a: &Analyzer, s: &str) -> Vec<Token> {
    use unicode_normalization::{UnicodeNormalization, char::is_combining_mark};
    // M158: a document's string is one token, whole -- U+001F included, so a stored token is
    // never split, whichever write stored it.
    if a.tokenizer == Tokenizer::PreTokenizedArray {
        return (!s.is_empty())
            .then(|| Token {
                span: 0..s.len(),
                term: Some(s.to_owned()),
            })
            .into_iter()
            .collect();
    }
    let stemmer = a
        .stemming
        .then(|| rust_stemmers::Stemmer::create(a.language.algorithm()));
    let mut out = Vec::new();
    let mut start = None;
    // The sentinel space closes a token that runs to the end of the text.
    for (i, c) in s.char_indices().chain(std::iter::once((s.len(), ' '))) {
        match (c.is_alphanumeric(), start) {
            (true, None) => start = Some(i),
            (false, Some(from)) => {
                start = None;
                let raw = &s[from..i];
                let t = if a.case_sensitive {
                    raw.to_owned()
                } else {
                    raw.to_lowercase()
                };
                let t: String = if a.ascii_folding {
                    t.nfkd().filter(|c| !is_combining_mark(*c)).collect()
                } else {
                    t
                };
                let term = if a.remove_stopwords && STOPWORDS.contains(&t.as_str()) {
                    None
                } else {
                    Some(match &stemmer {
                        Some(st) => st.stem(&t).into_owned(),
                        None => t,
                    })
                };
                out.push(Token {
                    span: from..i,
                    term,
                });
            }
            _ => {}
        }
    }
    out
}

/// `end` moved forward over the combining marks that follow it (M63). In decomposed text an
/// accent is not alphanumeric, so [`tokens`] ends a token before it; a client marking the
/// token must keep the accent inside its marks, or it lands after the closing one.
#[must_use]
pub fn extend_over_marks(s: &str, end: usize) -> usize {
    use unicode_normalization::char::is_combining_mark;
    end + s
        .get(end..)
        .unwrap_or_default()
        .chars()
        // Not a mark that is also a letter (U+0345): the splitter starts a token at that one.
        .take_while(|c| is_combining_mark(*c) && !c.is_alphanumeric())
        .map(char::len_utf8)
        .sum::<usize>()
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

/// The strings a text field holds (M150): a string's one, an array's string elements in order,
/// and nothing for anything else. An array's other elements are skipped: the door refuses
/// them, but a row written below it may hold one.
///
/// ⚠️ Every place that asks whether a row has text, or what its tokens are, reads this, so the
/// segment build, the fresh view and the M36 stamp cannot disagree about an array.
pub fn strings(v: &Value) -> impl Iterator<Item = &str> {
    let (one, many): (Option<&str>, &[Value]) = match v {
        Value::Str(s) => (Some(s.as_str()), &[]),
        Value::Array(items) => (None, items.as_slice()),
        _ => (None, &[]),
    };
    one.into_iter().chain(many.iter().filter_map(|e| match e {
        Value::Str(s) => Some(s.as_str()),
        _ => None,
    }))
}

/// Whether a text field's value holds text (M150): some string of it is non-empty.
#[must_use]
pub fn has_text(v: &Value) -> bool {
    strings(v).any(|s| !s.is_empty())
}

/// Builds the postings, the dictionary and the fieldnorms for one text attribute.
///
/// ⚠️ `docs` is taken in **segment row order**, and a document with no text still occupies a
/// row. Skipping it would move every later row by one, and every hit afterwards would name the
/// wrong document — with scores that are internally consistent and a top-k that looks fine.
#[must_use]
pub fn build(docs: &[Document], field: &str) -> Built {
    build_with(docs, field, &Analyzer::default())
}

/// [`build`], analyzing under `analyzer` (M14).
#[must_use]
pub fn build_with(docs: &[Document], field: &str, analyzer: &Analyzer) -> Built {
    let mut by_term: std::collections::BTreeMap<String, Vec<(u32, u32)>> =
        std::collections::BTreeMap::new();
    let mut fieldnorms = Vec::with_capacity(docs.len());
    for (row, d) in docs.iter().enumerate() {
        // M150: an array's strings, each analyzed alone and in order: what the same strings
        // joined by a space would give, since a space ends a token under every analyzer.
        let tokens: Vec<String> = d
            .attrs
            .get(field)
            .into_iter()
            .flat_map(strings)
            .flat_map(|s| analyze(analyzer, s))
            .collect();
        fieldnorms.push(tokens.len() as u32);
        let mut tf: std::collections::BTreeMap<&str, u32> = std::collections::BTreeMap::new();
        for t in &tokens {
            *tf.entry(t.as_str()).or_insert(0) += 1;
        }
        // The term allocated once per vocabulary word, not once per row it is in (M119).
        for (term, n) in tf {
            if let Some(list) = by_term.get_mut(term) {
                list.push((row as u32, n));
            } else {
                by_term.insert(term.to_owned(), vec![(row as u32, n)]);
            }
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

/// Several text fields built for one segment (M151): the fields carried, in order, and their
/// postings, fieldnorms and dictionary sidecar joined.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Fields {
    /// The fields carried: those asked for that some row has a token in, in the order asked.
    pub names: Vec<String>,
    /// The `TextPostings` section: each carried field's postings, back to back.
    pub postings: Vec<u8>,
    /// The sidecar: one field's dictionary as [`build_with`] writes it, or a container of
    /// several. Empty when nothing is carried.
    pub dictionary: Vec<u8>,
    /// Each carried field's token count for every row, field after field: `names.len() *
    /// rows` of them.
    pub fieldnorms: Vec<u32>,
}

const CONTAINER_MAGIC: &[u8; 8] = b"PSTORETM";
const CONTAINER_VERSION: u16 = 1;
/// MAGIC(8) VERSION(2) fields(2).
///
/// ⚠️ Written as one number: `8 + 2 + 2` has an equivalent mutant, `2 + 2` as `2 * 2`, which
/// no test can fail and which PR #8's mutation gate reported as missed.
const CONTAINER_HEADER: usize = 12;
/// dictionary_len(4) postings_start(8) postings_len(8).
const CONTAINER_ENTRY: usize = 4 + 8 + 8;

/// Builds every field of `fields` that some row has text in (M151).
///
/// ⚠️ **One carried field is [`build_with`]'s output, byte for byte**, and several are each
/// field's [`build_with`] joined: a field's slice of a segment is its one-field build, so a
/// reader of one field reads what it always read, through narrower ranges. A field no row has
/// a token in is not carried, as a one-field build of it writes no postings.
///
/// `None` for an empty list or a repeated name: a field is found by its position among the
/// segment's names, and a repeat makes that ambiguous.
#[must_use]
pub fn build_fields(docs: &[Document], fields: &[(&str, &Analyzer)]) -> Option<Fields> {
    let mut seen = std::collections::BTreeSet::new();
    if fields.is_empty() || !fields.iter().all(|(n, _)| seen.insert(*n)) {
        return None;
    }
    // ⚠️ Carried when it has POSTINGS, not when some row has a non-empty string: text that
    // analyzes to no token (`"The"` under stopword removal) writes no postings, and the
    // segment writer records a one-field segment's name only beside its postings. Carrying
    // such a field would add its rows to `doc_count` and lower `avgdl` (code review).
    let built: Vec<(String, Built)> = fields
        .iter()
        .map(|(n, a)| ((*n).to_owned(), build_with(docs, n, a)))
        .filter(|(_, b)| !b.postings.is_empty())
        .collect();
    if let [(name, one)] = built.as_slice() {
        return Some(Fields {
            names: vec![name.clone()],
            postings: one.postings.clone(),
            dictionary: one.dictionary.clone(),
            fieldnorms: one.fieldnorms.clone(),
        });
    }
    let mut out = Fields::default();
    if built.is_empty() {
        return Some(out);
    }
    out.dictionary.extend_from_slice(CONTAINER_MAGIC);
    out.dictionary
        .extend_from_slice(&CONTAINER_VERSION.to_le_bytes());
    out.dictionary
        .extend_from_slice(&(built.len() as u16).to_le_bytes());
    let mut start = 0u64;
    for (_, b) in &built {
        out.dictionary
            .extend_from_slice(&(b.dictionary.len() as u32).to_le_bytes());
        out.dictionary.extend_from_slice(&start.to_le_bytes());
        out.dictionary
            .extend_from_slice(&(b.postings.len() as u64).to_le_bytes());
        start += b.postings.len() as u64;
    }
    for (name, b) in built {
        out.dictionary.extend_from_slice(&b.dictionary);
        out.postings.extend_from_slice(&b.postings);
        out.fieldnorms.extend_from_slice(&b.fieldnorms);
        out.names.push(name);
    }
    Some(out)
}

/// A segment's term dictionaries, one per carried text field (M151), each a slice of the
/// fetched sidecar.
#[derive(Debug, Clone)]
pub struct Dictionaries {
    /// Per field: its dictionary's bytes, decoded, and its span within `TextPostings`, or
    /// `None` for a segment's only field, whose span is the whole section.
    fields: Vec<(
        bytes::Bytes,
        std::sync::Arc<TermDict>,
        Option<std::ops::Range<u64>>,
    )>,
}

impl Dictionaries {
    /// Decodes the sidecar of a segment carrying `n` text fields: for one, the dictionary
    /// itself; for several, a `PSTORETM` container. `None` for anything malformed.
    ///
    /// ⚠️ Only what the sidecar alone can show is checked here: that the postings spans run
    /// back to back from 0 is, but that the last ends where `TextPostings` does needs the
    /// segment, and `pstore-index`'s `TextIndex::for_field` checks it.
    #[must_use]
    pub fn decode(raw: &bytes::Bytes, n: usize) -> Option<Self> {
        if n == 1 {
            let d = TermDict::decode(raw)?;
            return Some(Self {
                fields: vec![(raw.clone(), std::sync::Arc::new(d), None)],
            });
        }
        if n < 2
            || raw.get(..8)? != CONTAINER_MAGIC
            || u16::from_le_bytes(raw.get(8..10)?.try_into().ok()?) != CONTAINER_VERSION
            || usize::from(u16::from_le_bytes(raw.get(10..12)?.try_into().ok()?)) != n
        {
            return None;
        }
        let mut at = CONTAINER_HEADER + n * CONTAINER_ENTRY;
        let mut start = 0u64;
        let mut fields = Vec::with_capacity(n);
        for i in 0..n {
            let e = raw.get(CONTAINER_HEADER + i * CONTAINER_ENTRY..)?;
            let len = u32::from_le_bytes(e.get(0..4)?.try_into().ok()?) as usize;
            let lo = u64::from_le_bytes(e.get(4..12)?.try_into().ok()?);
            let span = u64::from_le_bytes(e.get(12..20)?.try_into().ok()?);
            if lo != start {
                return None;
            }
            start = lo.checked_add(span)?;
            let end = at.checked_add(len)?;
            if end > raw.len() {
                return None;
            }
            let slice = raw.slice(at..end);
            let d = TermDict::decode(&slice)?;
            fields.push((slice, std::sync::Arc::new(d), Some(lo..start)));
            at = end;
        }
        (at == raw.len()).then_some(Self { fields })
    }

    /// [`Self::decode`], kept for `raw` itself, as [`TermDict::decode_shared`] keeps one
    /// dictionary (M122), charged the buffer and every decoded dictionary's heap.
    #[must_use]
    pub fn decode_shared(raw: &bytes::Bytes, n: usize) -> Option<std::sync::Arc<Self>> {
        static KEPT: std::sync::LazyLock<crate::shared::ByBuffer<Dictionaries>> =
            std::sync::LazyLock::new(|| crate::shared::ByBuffer::new(DECODED_BUDGET));
        // ⚠️ `n` is not part of the key: one buffer is one segment's sidecar, whose field
        // count is its segment's. A different `n` for the same buffer would be a different
        // segment's footer paired with it, which no caller does.
        KEPT.get_or(
            raw,
            |d| d.fields.iter().map(|(_, t, _)| t.heap()).sum(),
            |b| Self::decode(b, n),
        )
        .filter(|d| d.fields() == n)
    }

    /// Fields decoded: never none, since `decode` refuses a count of 0. Named
    /// `fields` rather than `len`, which would ask for an `is_empty` that is always false.
    #[must_use]
    pub fn fields(&self) -> usize {
        self.fields.len()
    }

    /// Field `i`'s dictionary bytes: a slice of the buffer decoded.
    #[must_use]
    pub fn dictionary(&self, i: usize) -> Option<&bytes::Bytes> {
        self.fields.get(i).map(|(b, _, _)| b)
    }

    /// Field `i`'s decoded dictionary.
    #[must_use]
    pub fn term_dict(&self, i: usize) -> Option<&std::sync::Arc<TermDict>> {
        self.fields.get(i).map(|(_, d, _)| d)
    }

    /// Field `i`'s postings span within `TextPostings`; `None` for a segment's only field,
    /// whose span is the whole section, and for a field that is not there.
    #[must_use]
    pub fn postings(&self, i: usize) -> Option<std::ops::Range<u64>> {
        self.fields.get(i).and_then(|(_, _, s)| s.clone())
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

/// What the decoded term dictionaries kept by [`TermDict::decode_shared`] may hold alive, in
/// all: their buffers and their decoded heaps. ⚠️ [`Dictionaries::decode_shared`] keeps its
/// own, to the same budget (M151), so the two together may hold twice this: a container
/// never enters the first, and a one-field sidecar never the second.
pub const DECODED_BUDGET: usize = 32 << 20;

/// The term dictionary: sorted terms, fixed-width entries, and a blob of term bytes.
#[derive(Debug, Clone)]
pub struct TermDict {
    /// The term bytes, once, and each term's span in them (M119): a `String` per term was an
    /// allocation per vocabulary word on every open of every text segment.
    blob: Vec<u8>,
    terms: Vec<(u32, u16)>,
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
            let len = u16::from_le_bytes(b.get(4..6)?.try_into().ok()?);
            std::str::from_utf8(blob.get(at..at + usize::from(len))?).ok()?;
            terms.push((u32::try_from(at).ok()?, len));
            entries.push(TermEntry {
                offset: u64::from(u32::from_le_bytes(b.get(6..10)?.try_into().ok()?)),
                bytes: u32::from_le_bytes(b.get(10..14)?.try_into().ok()?),
                count: u32::from_le_bytes(b.get(14..18)?.try_into().ok()?),
                df: u32::from_le_bytes(b.get(18..22)?.try_into().ok()?),
            });
        }
        // Unsorted cannot be searched, and searching it anyway answers "absent" for terms that
        // are present — a silent recall loss rather than a failure. Bytes compare as the
        // strings they spell do.
        let at = |t: &(u32, u16)| blob.get(t.0 as usize..t.0 as usize + usize::from(t.1));
        if !terms.is_sorted_by(|a, b| at(a) < at(b)) {
            return None;
        }
        Some(Self {
            blob: blob.to_vec(),
            terms,
            entries,
            rows,
            total_tokens,
        })
    }

    /// [`Self::decode`], kept for `raw` itself (M122): every text query decoded every text
    /// segment's dictionary, though the pinned cache hands back the same buffer for it each
    /// time. Process-wide, bounded; see `shared`.
    #[must_use]
    pub fn decode_shared(raw: &bytes::Bytes) -> Option<std::sync::Arc<Self>> {
        static KEPT: std::sync::LazyLock<crate::shared::ByBuffer<TermDict>> =
            std::sync::LazyLock::new(|| crate::shared::ByBuffer::new(DECODED_BUDGET));
        KEPT.get_or(raw, Self::heap, |b| Self::decode(b))
    }

    /// The bytes this holds beyond itself.
    pub(crate) fn heap(&self) -> usize {
        self.blob.len()
            + self.terms.len() * std::mem::size_of::<(u32, u16)>()
            + self.entries.len() * std::mem::size_of::<TermEntry>()
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
        self.terms.iter().map(|t| self.term(*t))
    }

    /// The term at a span `decode` validated as UTF-8 inside the blob.
    fn term(&self, (at, len): (u32, u16)) -> &str {
        self.blob
            .get(at as usize..at as usize + usize::from(len))
            .and_then(|b| std::str::from_utf8(b).ok())
            .unwrap_or_default()
    }

    /// One term's entry, or `None` when the vocabulary does not contain it.
    ///
    /// ⚠️ `None` is the **common** case, not an error: a query term the corpus never used
    /// contributes nothing and must cost nothing. `binary_search` rather than a hand-written
    /// loop, for the reason `sparse::Dictionary::lookup` gives.
    #[must_use]
    pub fn lookup(&self, term: &str) -> Option<TermEntry> {
        let i = self
            .terms
            .binary_search_by(|t| self.term(*t).cmp(term))
            .ok()?;
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
