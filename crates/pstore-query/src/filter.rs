//! Filters — which documents a query may answer with (M9b).
//!
//! ⚠️ **Evaluated before a leg's limit, never after it.** A predicate applied to a top-`k`
//! returns fewer than `k` whenever it is selective, and nothing says so. `run` therefore
//! builds each segment's [`Mask`] from its blocks and applies it to exhaustive legs.

use pstore_format::{Number, Value, Zones, cmp_numbers, datetime, typed};
use std::cmp::Ordering;
use std::collections::BTreeMap;

/// The document id, addressed as if it were an attribute — turbopuffer's `id`.
pub const ID: &str = "id";

/// A comparison's operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    /// Present and equal.
    Eq,
    /// Present and less than.
    Lt,
    /// Present and at most.
    Lte,
    /// Present and greater than.
    Gt,
    /// Present and at least.
    Gte,
}

/// How a token predicate matches the query's tokens against a row's (M14.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenOp {
    /// Every query token is among the row's: `ContainsAllTokens`.
    All,
    /// At least one is: `ContainsAnyToken`.
    Any,
    /// They appear consecutively and in order: `ContainsTokenSequence`.
    Sequence,
    /// [`Self::All`], the last raw token a prefix: `last_as_prefix` (M138).
    AllPrefix,
    /// [`Self::Any`], the last raw token a prefix: `last_as_prefix` (M138).
    AnyPrefix,
}

/// A token predicate's analyzer, and its text already analyzed by it (M14.2): analyzed once
/// when bound, never once per row (code review).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bound {
    analyzer: pstore_format::text::Analyzer,
    query: Vec<String>,
    /// The literal prefix of a `*Prefix` op (M138): the last raw token as [`plain`] makes it.
    prefix: Option<String>,
}

impl Bound {
    /// `text` analyzed by `analyzer` for `op`. A prefix op splits off the last raw token as a
    /// literal prefix, and analyzes the text before it (M138).
    #[must_use]
    pub fn new(analyzer: pstore_format::text::Analyzer, op: TokenOp, text: &str) -> Self {
        use pstore_format::text::{analyze, analyze_query, query_tokens};
        // M158: an operand, read as one -- under `pre_tokenized_array`, tokens joined by U+001F.
        let last = matches!(op, TokenOp::AllPrefix | TokenOp::AnyPrefix)
            .then(|| query_tokens(&analyzer, text).pop())
            .flatten();
        match last {
            Some(t) => Self {
                analyzer,
                query: analyze_query(&analyzer, text.get(..t.span.start).unwrap_or_default()),
                prefix: analyze(&plain(&analyzer), text.get(t.span).unwrap_or_default()).pop(),
            },
            None => Self {
                analyzer,
                query: analyze_query(&analyzer, text),
                prefix: None,
            },
        }
    }

    /// The analyzer it was bound to.
    #[must_use]
    pub fn analyzer(&self) -> &pstore_format::text::Analyzer {
        &self.analyzer
    }
}

/// `a` with neither stemming nor stopword removal (M138): what a literal prefix, and the row
/// tokens it is matched against, are made with. Case and folding stay the index's.
pub(crate) fn plain(a: &pstore_format::text::Analyzer) -> pstore_format::text::Analyzer {
    pstore_format::text::Analyzer {
        stemming: false,
        remove_stopwords: false,
        ..*a
    }
}

/// Which pattern language a [`Pattern`] was written in (M15).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PatternKind {
    /// A Unix glob over the whole value.
    Glob,
    /// [`PatternKind::Glob`], case-insensitive by simple case folding.
    IGlob,
    /// A `regex` crate pattern, unanchored.
    Regex,
}

/// A pattern compiled once, when parsed (M15). Equal by kind and source.
#[derive(Debug, Clone)]
pub struct Pattern {
    kind: PatternKind,
    source: String,
    re: regex::Regex,
    /// The folded trigrams every match must contain (M15.2): what prunes a block.
    required: Vec<pstore_format::trigram::Trigram>,
}

/// The longest source a pattern may have, in bytes (M15).
pub const MAX_PATTERN: usize = 4096;
/// The most a compiled pattern may take (M15), set rather than left to the crate's default.
const COMPILED_LIMIT: usize = 1 << 20;

impl PartialEq for Pattern {
    fn eq(&self, other: &Self) -> bool {
        self.kind == other.kind && self.source == other.source
    }
}

impl Pattern {
    /// `source` as `kind` reads it, compiled -- or why it cannot be.
    ///
    /// # Errors
    /// A source longer than [`MAX_PATTERN`], a glob with an unterminated `[` or a trailing
    /// `\`, or a regex that does not compile within the size limit.
    pub fn new(kind: PatternKind, source: &str) -> Result<Self, String> {
        if source.len() > MAX_PATTERN {
            return Err(format!("a pattern is at most {MAX_PATTERN} bytes"));
        }
        let re = match kind {
            PatternKind::Regex => source.to_owned(),
            PatternKind::Glob => glob_regex(source, false)?,
            PatternKind::IGlob => glob_regex(source, true)?,
        };
        let re = regex::RegexBuilder::new(&re)
            .size_limit(COMPILED_LIMIT)
            .build()
            .map_err(|e| e.to_string())?;
        let runs = match kind {
            PatternKind::Glob | PatternKind::IGlob => glob_runs(source),
            PatternKind::Regex => regex_syntax::Parser::new()
                .parse(source)
                .map(|h| {
                    let mut out = Vec::new();
                    hir_runs(&h, &mut out);
                    out
                })
                // Unparsed here though compiled there: require nothing, which is sound.
                .unwrap_or_default(),
        };
        let mut required = Vec::new();
        for run in runs {
            for t in pstore_format::trigram::trigrams(&run) {
                if !required.contains(&t) {
                    required.push(t);
                }
            }
        }
        Ok(Self {
            kind,
            source: source.to_owned(),
            re,
            required,
        })
    }

    /// Its language.
    #[must_use]
    pub fn kind(&self) -> PatternKind {
        self.kind
    }

    /// Its source, as written.
    #[must_use]
    pub fn source(&self) -> &str {
        &self.source
    }
}

/// A glob's literal runs (M15.2): what lies between its wildcards and classes, escapes
/// included in the run.
fn glob_runs(glob: &str) -> Vec<String> {
    let mut runs = vec![String::new()];
    let mut it = glob.chars();
    while let Some(c) = it.next() {
        match c {
            '*' | '?' => runs.push(String::new()),
            '[' => {
                // Read exactly as `glob_regex` reads it (code review, M15 B1): a class whose
                // end is found differently here would leave its members in a required run.
                if glob_class(&mut it).is_err() {
                    return vec![String::new()];
                }
                runs.push(String::new());
            }
            '\\' => {
                if let (Some(n), Some(run)) = (it.next(), runs.last_mut()) {
                    run.push(n);
                }
            }
            c => {
                if let Some(run) = runs.last_mut() {
                    run.push(c);
                }
            }
        }
    }
    runs
}

/// The literal runs every match of `h` contains (M15.2), conservatively: only literals that
/// are adjacent in a concatenation join. A capture, or a repetition of at least one, is its
/// own requirement and never joins its neighbours; anything else requires nothing.
fn hir_runs(h: &regex_syntax::hir::Hir, out: &mut Vec<String>) {
    use regex_syntax::hir::HirKind;
    let lit = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
    match h.kind() {
        HirKind::Literal(l) => out.push(lit(&l.0)),
        HirKind::Capture(c) => hir_runs(&c.sub, out),
        HirKind::Repetition(r) if r.min >= 1 => hir_runs(&r.sub, out),
        HirKind::Concat(items) => {
            let mut run = String::new();
            for item in items {
                if let HirKind::Literal(l) = item.kind() {
                    run.push_str(&lit(&l.0));
                } else {
                    out.push(std::mem::take(&mut run));
                    hir_runs(item, out);
                }
            }
            out.push(run);
        }
        _ => {}
    }
}

/// A character as a regex literal, escaped whatever it is.
fn lit(c: char) -> String {
    if c.is_alphanumeric() {
        c.to_string()
    } else {
        format!("\\x{{{:x}}}", c as u32)
    }
}

/// A glob as a regex over the whole value: `*` is any run, `?` any character -- `/` and a
/// newline included -- `[..]` and `[!..]` classes with ranges, and `\` escapes.
fn glob_regex(glob: &str, ci: bool) -> Result<String, String> {
    let mut out = String::from(if ci { "(?is)\\A" } else { "(?s)\\A" });
    let mut it = glob.chars();
    while let Some(c) = it.next() {
        match c {
            '*' => out.push_str(".*"),
            '?' => out.push('.'),
            '\\' => out.push_str(&lit(it.next().ok_or("a glob ends in an escape")?)),
            '[' => out.push_str(&glob_class(&mut it)?),
            c => out.push_str(&lit(c)),
        }
    }
    out.push_str("\\z");
    Ok(out)
}

/// A glob class, read after its `[`, as a regex class: an optional leading `!` negates it, the
/// first member may be `]`, `a-z` is a range, and every other character -- a `-` included -- is
/// a member. The one reader both the translation and the literal runs use.
fn glob_class(it: &mut std::str::Chars<'_>) -> Result<String, String> {
    let unterminated = || "a glob has an unterminated [".to_owned();
    let mut members: Vec<char> = Vec::new();
    let mut negated = false;
    let mut first = true;
    loop {
        let c = it.next().ok_or_else(unterminated)?;
        match c {
            '!' if first && !negated && members.is_empty() => {
                negated = true;
                continue;
            }
            ']' if !first => break,
            c => members.push(c),
        }
        first = false;
    }
    let mut out = String::from(if negated { "[^" } else { "[" });
    let mut i = 0;
    while let Some(&c) = members.get(i) {
        match (members.get(i + 1), members.get(i + 2)) {
            (Some('-'), Some(&hi)) => {
                out.push_str(&format!("{}-{}", lit(c), lit(hi)));
                i += 3;
            }
            _ => {
                out.push_str(&lit(c));
                i += 1;
            }
        }
    }
    out.push(']');
    Ok(out)
}

/// Whether `a` is within `k` edits of `b`, in characters (M15): a band of width `2k+1`, and
/// out at once when the lengths differ by more.
///
/// ⚠️ `a` is read once, in order, beside its length (M120): a row's string was collected into
/// a `Vec` of its characters per row, and the table took a fresh row per character. Two rows
/// now, each reset as the fresh one was made.
fn within(a_len: usize, a: impl Iterator<Item = char>, b: &[char], k: usize) -> bool {
    if a_len.abs_diff(b.len()) > k {
        return false;
    }
    let inf = usize::MAX / 2;
    let mut prev: Vec<usize> = (0..=b.len())
        .map(|j| if j <= k { j } else { inf })
        .collect();
    let mut row = vec![inf; b.len() + 1];
    for (i, ca) in (1..).zip(a) {
        row.fill(inf);
        if let Some(first) = row.first_mut().filter(|_| i <= k) {
            *first = i;
        }
        for j in i.saturating_sub(k).max(1)..=(i + k).min(b.len()) {
            let (Some(&diag), Some(&up), Some(&left), Some(cb)) =
                (prev.get(j - 1), prev.get(j), row.get(j - 1), b.get(j - 1))
            else {
                continue;
            };
            if let Some(cell) = row.get_mut(j) {
                *cell = (diag + usize::from(ca != *cb)).min(up + 1).min(left + 1);
            }
        }
        std::mem::swap(&mut prev, &mut row);
    }
    prev.last().is_some_and(|d| *d <= k)
}

/// Whether some substring of `value`, possibly empty, is within `k` edits of `q`, in
/// characters (M145): Sellers' table, the edit-distance table with a free start and end in
/// the value, one row per character of the value and out at the first row that reaches `k`.
/// Case-insensitive compares [folded](pstore_format::trigram::fold) characters, one to one.
fn near(value: &str, q: &[char], k: usize, case_sensitive: bool) -> bool {
    let key = |c: char| {
        if case_sensitive {
            c
        } else {
            pstore_format::trigram::fold(c)
        }
    };
    let q: Vec<char> = q.iter().map(|&c| key(c)).collect();
    // `prev[j]`: the fewest edits taking some suffix of the value read so far to `q[..j]`.
    let mut prev: Vec<usize> = (0..=q.len()).collect();
    let mut row = prev.clone();
    if prev.last().is_some_and(|d| *d <= k) {
        return true;
    }
    for c in value.chars().map(key) {
        let mut left = 0;
        if let Some(first) = row.first_mut() {
            *first = 0;
        }
        for (j, want) in q.iter().enumerate() {
            let (Some(&diag), Some(&up)) = (prev.get(j), prev.get(j + 1)) else {
                break;
            };
            let d = (diag + usize::from(c != *want)).min(up + 1).min(left + 1);
            if let Some(cell) = row.get_mut(j + 1) {
                *cell = d;
            }
            left = d;
        }
        if row.last().is_some_and(|d| *d <= k) {
            return true;
        }
        std::mem::swap(&mut prev, &mut row);
    }
    false
}

/// A fuzzy predicate's value, with what every row and every block compares it by, taken once
/// (M120): its characters, and its distinct folded trigrams.
#[derive(Debug, Clone)]
pub struct Needle {
    text: String,
    chars: Vec<char>,
    trigrams: Vec<pstore_format::trigram::Trigram>,
}

impl Needle {
    /// The value as written.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.text
    }
}

impl From<String> for Needle {
    fn from(text: String) -> Self {
        Self {
            chars: text.chars().collect(),
            trigrams: pstore_format::trigram::trigrams(&text),
            text,
        }
    }
}

impl From<&str> for Needle {
    fn from(text: &str) -> Self {
        text.to_owned().into()
    }
}

/// Equal when the values are: the rest is derived from them.
impl PartialEq for Needle {
    fn eq(&self, other: &Self) -> bool {
        self.text == other.text
    }
}

/// A predicate over one document's id and attributes.
#[derive(Debug, Clone, PartialEq)]
pub enum Predicate {
    /// `attr op value`. A type mismatch or an absent attribute is false.
    Cmp(String, Op, Value),
    /// The attribute is absent — turbopuffer's `Eq null`.
    Absent(String),
    /// Present and equal to one of these.
    In(String, Vec<Value>),
    /// An array holding an element equal to one of these (M9h.2). `Contains x` is
    /// `ContainsAny [x]`. A scalar or absent attribute is false; an empty list admits nothing.
    ContainsAny(String, Vec<Value>),
    /// An array holding an element `e` with `e op value`, compared as [`Self::Cmp`] compares
    /// (M135): `AnyLt`, `AnyLte`, `AnyGt`, `AnyGte`. An element of another type is false for
    /// that element; a scalar, absent or empty attribute is false. The operator is never
    /// [`Op::Eq`]: that is [`Self::ContainsAny`].
    AnyCmp(String, Op, Value),
    /// Every clause; `And []` is true.
    And(Vec<Predicate>),
    /// Any clause; `Or []` is false.
    Or(Vec<Predicate>),
    /// Not the clause. `NotEq v` is `Not(Cmp(Eq, v))`, so it admits an absent attribute.
    Not(Box<Predicate>),
    /// The attribute's tokens against the text's, both analyzed by the analyzer (M14.2): the
    /// index's, which the engine [binds](Predicate::bound). A missing or non-string
    /// attribute -- an array included -- is false.
    ///
    /// ⚠️ **Unbound is unknown**, and unknown admits nothing whatever encloses it, `Not`
    /// included (spec review round 2): answering as the default analyzer, or as "everything"
    /// under a `Not`, would be a wrong answer from a missed bind that no test could tell
    /// from a right one on a default index.
    Tokens(String, TokenOp, String, Option<Bound>),
    /// A scalar string attribute the pattern matches (M15). Anything else is false.
    Pattern(String, Pattern),
    /// A scalar string attribute within `k` edits of the value (M15). Anything else is false.
    Fuzzy(String, Needle, u8),
    /// The vendor's `Fuzzy` (M145): some substring of a string, or of one string element of
    /// an array, within `k` edits of the value, comparing [folded](pstore_format::trigram::fold)
    /// characters when the flag (case-sensitive) is false. Anything else is false.
    ///
    /// ⚠️ **It never prunes**: the M15.2 sketch holds only scalar strings, so a block of arrays
    /// would read as holding none of the value's trigrams.
    FuzzySubstring(String, Needle, u8, bool),
    /// A location within `meters` of `(lat, lon)`, inclusive (M66). An attribute that is not a
    /// location (see [`pstore_format::geo::location`]) is false.
    GeoRadius(String, f64, f64, f64),
    /// A location in `[south, west, north, east]`, edges inclusive; `west > east` crosses the
    /// antimeridian (M66). An attribute that is not a location is false.
    GeoBox(String, f64, f64, f64, f64),
}

impl Predicate {
    /// Whether a document with this id and these attributes is admitted: only when the whole
    /// predicate is known true.
    ///
    /// ⚠️ **Three-valued, checked once**: a predicate holding an unbound token predicate is
    /// unknown whatever encloses it, `Not` included, so it admits nothing -- and every other
    /// predicate is two-valued, so it evaluates short-circuiting (code review: collecting
    /// every child's value per row cost every filter, not only token ones).
    #[must_use]
    pub fn admits(&self, id: &str, attrs: &BTreeMap<String, Value>) -> bool {
        !self.unbound() && self.holds(id, attrs)
    }

    /// Every name it reads, [`ID`] among them when it reads the id (M119): what a mask must
    /// decode of a row, and all [`Self::admits`] looks at.
    pub(crate) fn names(&self) -> Vec<&str> {
        let mut out = Vec::new();
        self.names_into(&mut out);
        out.sort_unstable();
        out.dedup();
        out
    }

    fn names_into<'a>(&'a self, out: &mut Vec<&'a str>) {
        match self {
            Self::And(ps) | Self::Or(ps) => ps.iter().for_each(|p| p.names_into(out)),
            Self::Not(p) => p.names_into(out),
            Self::Cmp(name, ..)
            | Self::Absent(name)
            | Self::In(name, _)
            | Self::ContainsAny(name, _)
            | Self::AnyCmp(name, ..)
            | Self::Tokens(name, ..)
            | Self::Pattern(name, _)
            | Self::Fuzzy(name, ..)
            | Self::FuzzySubstring(name, ..)
            | Self::GeoRadius(name, ..)
            | Self::GeoBox(name, ..) => out.push(name),
        }
    }

    /// Whether a token predicate anywhere in it is unbound.
    fn unbound(&self) -> bool {
        match self {
            Self::Tokens(.., b) => b.is_none(),
            Self::And(ps) | Self::Or(ps) => ps.iter().any(Self::unbound),
            Self::Not(p) => p.unbound(),
            _ => false,
        }
    }

    /// This predicate with every token predicate in it bound to `analyzer` (M14.2).
    #[must_use]
    pub fn bound(&self, analyzer: &pstore_format::text::Analyzer) -> Self {
        self.bound_texts(analyzer, &BTreeMap::new())
    }

    /// This predicate with every token predicate in it bound (M156): one over a field named in
    /// `texts`, the index's further text fields, to that field's analyzer; any other to
    /// `primary`, the analyzer [`Self::bound`] takes.
    #[must_use]
    pub fn bound_texts(
        &self,
        primary: &pstore_format::text::Analyzer,
        texts: &BTreeMap<String, pstore_format::text::FullText>,
    ) -> Self {
        match self {
            Self::Tokens(name, op, text, _) => {
                let analyzer = texts.get(name).map_or(primary, |f| &f.analyzer);
                Self::Tokens(
                    name.clone(),
                    *op,
                    text.clone(),
                    Some(Bound::new(*analyzer, *op, text)),
                )
            }
            Self::And(all) => {
                Self::And(all.iter().map(|p| p.bound_texts(primary, texts)).collect())
            }
            Self::Or(any) => Self::Or(any.iter().map(|p| p.bound_texts(primary, texts)).collect()),
            Self::Not(p) => Self::Not(Box::new(p.bound_texts(primary, texts))),
            other => other.clone(),
        }
    }

    /// The attribute of every token predicate in it, through `And`, `Or` and `Not` (M156): the
    /// fields whose analyzer its bind reads.
    #[must_use]
    pub fn token_fields(&self) -> Vec<&str> {
        match self {
            Self::Tokens(name, ..) => vec![name.as_str()],
            Self::And(ps) | Self::Or(ps) => ps.iter().flat_map(Self::token_fields).collect(),
            Self::Not(p) => p.token_fields(),
            _ => Vec::new(),
        }
    }

    /// Two-valued, for a predicate [`Self::admits`] has found fully bound.
    fn holds(&self, id: &str, attrs: &BTreeMap<String, Value>) -> bool {
        // ⚠️ Borrowed, not cloned (M119): this runs for every row of every admitted block, per
        // leaf, and a clone was an allocation for every string and a deep copy of every array.
        // Only the id, which is not an attribute, is built.
        let get = |name: &str| -> Option<std::borrow::Cow<'_, Value>> {
            if name == ID {
                Some(std::borrow::Cow::Owned(Value::Str(id.to_owned())))
            } else {
                attrs.get(name).map(std::borrow::Cow::Borrowed)
            }
        };
        match self {
            Self::Cmp(name, op, want) => get(name)
                .as_deref()
                .is_some_and(|have| compare(have, *op, want)),
            Self::Absent(name) => get(name).is_none(),
            Self::In(name, set) => get(name)
                .as_deref()
                .is_some_and(|have| set.iter().any(|w| compare(have, Op::Eq, w))),
            Self::ContainsAny(name, set) => match get(name).as_deref() {
                Some(Value::Array(items)) => items
                    .iter()
                    .any(|have| set.iter().any(|w| compare(have, Op::Eq, w))),
                _ => false,
            },
            Self::AnyCmp(name, op, want) => match get(name).as_deref() {
                Some(Value::Array(items)) => items.iter().any(|have| compare(have, *op, want)),
                _ => false,
            },
            Self::And(all) => all.iter().all(|p| p.holds(id, attrs)),
            Self::Or(any) => any.iter().any(|p| p.holds(id, attrs)),
            Self::Not(p) => !p.holds(id, attrs),
            // Unbound never reaches here: `admits` checked first.
            // M138: a string array is its string elements, each analyzed alone; one with none
            // admits nothing, as a number does.
            Self::Tokens(name, op, _, bound) => match (get(name).as_deref(), bound) {
                (Some(Value::Str(s)), Some(b)) => tokens_match(*op, b, &[s.as_str()]),
                (Some(Value::Array(items)), Some(b)) => {
                    let elems: Vec<&str> = items
                        .iter()
                        .filter_map(|v| match v {
                            Value::Str(s) => Some(s.as_str()),
                            _ => None,
                        })
                        .collect();
                    !elems.is_empty() && tokens_match(*op, b, &elems)
                }
                _ => false,
            },
            Self::Pattern(name, p) => match get(name).as_deref() {
                Some(Value::Str(s)) => p.re.is_match(s),
                _ => false,
            },
            Self::GeoRadius(name, lat, lon, meters) => get(name)
                .as_deref()
                .and_then(pstore_format::geo::location)
                .is_some_and(|at| pstore_format::geo::distance_m((*lat, *lon), at) <= *meters),
            Self::GeoBox(name, south, west, north, east) => get(name)
                .as_deref()
                .and_then(pstore_format::geo::location)
                .is_some_and(|(lat, lon)| {
                    let across = west > east;
                    (*south..=*north).contains(&lat)
                        && if across {
                            lon >= *west || lon <= *east
                        } else {
                            (*west..=*east).contains(&lon)
                        }
                }),
            Self::Fuzzy(name, v, k) => match get(name).as_deref() {
                Some(Value::Str(s)) => {
                    within(s.chars().count(), s.chars(), &v.chars, usize::from(*k))
                }
                _ => false,
            },
            Self::FuzzySubstring(name, v, k, case) => {
                let hit = |s: &str| near(s, &v.chars, usize::from(*k), *case);
                match get(name).as_deref() {
                    Some(Value::Str(s)) => hit(s),
                    Some(Value::Array(items)) => {
                        items.iter().any(|e| matches!(e, Value::Str(s) if hit(s)))
                    }
                    _ => false,
                }
            }
        }
    }

    /// How many patterns it holds, fuzzy ones included (M15): what the door caps.
    #[must_use]
    pub fn patterns(&self) -> usize {
        match self {
            Self::Pattern(..) | Self::Fuzzy(..) | Self::FuzzySubstring(..) => 1,
            Self::And(ps) | Self::Or(ps) => ps.iter().map(Self::patterns).sum(),
            Self::Not(p) => p.patterns(),
            _ => 0,
        }
    }

    /// Whether a block with these zone maps **could** hold an admitted row. `false` only
    /// when it provably cannot: a wrongly dropped block silently loses rows.
    ///
    /// ⚠️ A zone covers only rows that HAVE the attribute as a number, and a block may hold
    /// rows without it -- so absence, `Not`, and anything over a string, a bool or the id
    /// cannot prune. A numeric literal is checked against **both** the int and the float
    /// zone, because the two are compared as one (M9h.1).
    #[must_use]
    pub fn could_admit(&self, zones: &Zones) -> bool {
        match self {
            Self::Cmp(name, op, want) => match Number::of(want) {
                Some(x) => zones.could_hold_number(name, |lo, hi| {
                    let (lo, hi) = (cmp_numbers(lo, x), cmp_numbers(hi, x));
                    match op {
                        Op::Eq => {
                            lo.is_some_and(Ordering::is_le) && hi.is_some_and(Ordering::is_ge)
                        }
                        Op::Lt => lo.is_some_and(Ordering::is_lt),
                        Op::Lte => lo.is_some_and(Ordering::is_le),
                        Op::Gt => hi.is_some_and(Ordering::is_gt),
                        Op::Gte => hi.is_some_and(Ordering::is_ge),
                    }
                }),
                None => match want {
                    Value::Str(s) if name != ID => could_hold_string(zones, name, *op, s),
                    _ => true,
                },
            },
            // ⚠️ A string or bool member can never prune: one name may hold strings in some
            // rows and numbers in others (M9a stores values as written), and the zones cover
            // only the numbers.
            Self::In(name, set) => set
                .iter()
                .any(|v| Self::Cmp(name.clone(), Op::Eq, v.clone()).could_admit(zones)),
            Self::And(all) => all.iter().all(|p| p.could_admit(zones)),
            Self::Or(any) => any.iter().any(|p| p.could_admit(zones)),
            // An array has no zone (M9h.2), and a zone of the name's scalars says nothing of
            // the arrays beside them.
            Self::ContainsAny(..) | Self::AnyCmp(..) | Self::Absent(_) | Self::Not(_) => true,
            // Tokens have no zone: every block could hold a match (M14.2). Nor has a
            // location, an array of two numbers (M66).
            Self::Tokens(..) | Self::GeoRadius(..) | Self::GeoBox(..) => true,
            // M145: the sketch never saw an array's strings.
            Self::FuzzySubstring(..) => true,
            // M15.2: a block whose sketch lacks a required trigram holds no match. No sketch,
            // or no filter for the attribute, rules nothing out.
            Self::Pattern(name, p) => zones
                .sketch
                .as_ref()
                .is_none_or(|(s, b)| s.may_hold(name, *b, &p.required)),
            // The q-gram lemma: within k edits, a value keeps all but at most 3k of `v`'s
            // distinct trigrams, so a block holding fewer cannot match.
            Self::Fuzzy(name, v, k) => {
                let tris = &v.trigrams;
                let need = tris.len().saturating_sub(3 * usize::from(*k));
                need == 0
                    || zones
                        .sketch
                        .as_ref()
                        .is_none_or(|(s, b)| s.held(name, *b, tris).is_none_or(|n| n >= need))
            }
        }
    }
}

/// Whether a row's string elements satisfy `op` against the bound query (M14.2, M138). An
/// empty query is the vacuous reading: every string for `All` and `Sequence`, none for `Any`.
/// `All` and `Any` look across the elements together; a sequence lies within one element --
/// except under `pre_tokenized_array` (M158), where each element is a token and a sequence
/// runs across them.
fn tokens_match(op: TokenOp, b: &Bound, elems: &[&str]) -> bool {
    use pstore_format::text::{Tokenizer, analyze};
    let mut rows: Vec<Vec<String>> = elems.iter().map(|e| analyze(&b.analyzer, e)).collect();
    if b.analyzer.tokenizer == Tokenizer::PreTokenizedArray {
        rows = vec![rows.concat()];
    }
    let has = |q: &String| rows.iter().any(|r| r.contains(q));
    // Only a prefix op analyzes the row a second time.
    let prefixed = || {
        b.prefix.as_ref().is_some_and(|p| {
            elems.iter().any(|e| {
                analyze(&plain(&b.analyzer), e)
                    .iter()
                    .any(|f| f.starts_with(p.as_str()))
            })
        })
    };
    let query = &b.query;
    match op {
        TokenOp::All => query.iter().all(has),
        TokenOp::Any => query.iter().any(has),
        TokenOp::Sequence => {
            query.is_empty()
                || rows
                    .iter()
                    .any(|r| r.windows(query.len()).any(|w| w == query))
        }
        TokenOp::AllPrefix => query.iter().all(has) && (b.prefix.is_none() || prefixed()),
        TokenOp::AnyPrefix => query.iter().any(has) || prefixed(),
    }
}

/// Whether a row holding `name` could satisfy `name op s` for a string literal (M9h.3).
///
/// ⚠️ Only a datetime entry can rule the block out, and only when it says the name holds no
/// string here: a string row compares bytewise, and nothing describes those. With no entry,
/// no datetime row exists but a string row may, so the block is kept. The caller keeps `id`
/// out: no zone describes it.
fn could_hold_string(zones: &Zones, name: &str, op: Op, s: &str) -> bool {
    let Some(&(lo, hi, strings)) = zones.datetimes.get(name) else {
        return true;
    };
    if strings {
        return true;
    }
    // Not RFC 3339, so it matches no datetime row -- and there is no string row.
    let Some(x) = datetime::parse(s) else {
        return false;
    };
    match op {
        Op::Eq => lo <= x && x <= hi,
        Op::Lt => lo < x,
        Op::Lte => lo <= x,
        Op::Gt => hi > x,
        Op::Gte => hi >= x,
    }
}

/// `have op want`: numbers as one exact line (an int and a float compare by value, M9h.1),
/// strings by bytes, bools with `false < true`, datetimes by instant -- a string literal read
/// as RFC 3339 against a datetime row (M9h.3) -- and anything else false.
fn compare(have: &Value, op: Op, want: &Value) -> bool {
    let ord = match (have, want) {
        (Value::Str(a), Value::Str(b)) => Some(a.as_bytes().cmp(b.as_bytes())),
        (Value::DateTime(a), Value::DateTime(b)) => Some(a.cmp(b)),
        (Value::DateTime(a), Value::Str(b)) => datetime::parse(b).map(|b| a.cmp(&b)),
        // M171: a string literal read as the row's type, as a datetime row reads one.
        (Value::Uuid(a), Value::Uuid(b)) => Some(a.cmp(b)),
        (Value::Uuid(a), Value::Str(b)) => typed::parse_uuid(b).map(|b| a.cmp(&b)),
        (Value::Bytes(a), Value::Bytes(b)) => Some(a.cmp(b)),
        (Value::Bytes(a), Value::Str(b)) => typed::parse_bytes(b).map(|b| a.as_slice().cmp(&b)),
        (Value::Bool(a), Value::Bool(b)) => Some(a.cmp(b)),
        _ => match (Number::of(have), Number::of(want)) {
            (Some(a), Some(b)) => cmp_numbers(a, b),
            _ => None,
        },
    };
    let Some(ord) = ord else {
        return false;
    };
    match op {
        Op::Eq => ord.is_eq(),
        Op::Lt => ord.is_lt(),
        Op::Lte => ord.is_le(),
        Op::Gt => ord.is_gt(),
        Op::Gte => ord.is_ge(),
    }
}

/// The data rows of one segment a predicate admits.
///
/// ⚠️ A bitmap over the segment's rows, not a `HashSet` (M119): under a filter every leg is
/// exhaustive, so every candidate of every segment is looked up here, and SipHash on a dense
/// row number cost more than the lookup it guarded.
#[derive(Debug, Default)]
pub(crate) struct Mask(Vec<bool>);

impl Mask {
    /// Whether `row` is admitted.
    pub(crate) fn contains(&self, row: &usize) -> bool {
        self.0.get(*row).copied().unwrap_or(false)
    }
}

impl FromIterator<usize> for Mask {
    fn from_iter<I: IntoIterator<Item = usize>>(rows: I) -> Self {
        let mut bits = Vec::new();
        for row in rows {
            if row >= bits.len() {
                bits.resize(row + 1, false);
            }
            if let Some(b) = bits.get_mut(row) {
                *b = true;
            }
        }
        Self(bits)
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    reason = "assertions in tests are the reporting mechanism"
)]
mod tests {
    use super::*;

    #[allow(
        clippy::indexing_slicing,
        clippy::cast_possible_truncation,
        reason = "a test's indexes are in range by construction"
    )]
    #[test]
    fn the_needle_is_the_strings() {
        // The distance M120 replaced, verbatim: a fresh row per character of `a`.
        fn old(a: &[char], b: &[char], k: usize) -> bool {
            if a.len().abs_diff(b.len()) > k {
                return false;
            }
            let inf = usize::MAX / 2;
            let mut prev: Vec<usize> = (0..=b.len())
                .map(|j| if j <= k { j } else { inf })
                .collect();
            for (i, ca) in (1..).zip(a) {
                let mut row = vec![inf; b.len() + 1];
                if let Some(first) = row.first_mut().filter(|_| i <= k) {
                    *first = i;
                }
                for j in i.saturating_sub(k).max(1)..=(i + k).min(b.len()) {
                    let (Some(&diag), Some(&up), Some(&left), Some(cb)) =
                        (prev.get(j - 1), prev.get(j), row.get(j - 1), b.get(j - 1))
                    else {
                        continue;
                    };
                    if let Some(cell) = row.get_mut(j) {
                        *cell = (diag + usize::from(ca != cb)).min(up + 1).min(left + 1);
                    }
                }
                prev = row;
            }
            prev.last().is_some_and(|d| *d <= k)
        }
        let mut s = 0x5eed_u64;
        let mut next = move || {
            s = s.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
            s >> 33
        };
        let alphabet = ['a', 'b', 'c', 'ſ', 'σ'];
        let mut word = |n: u64| -> String {
            (0..next() % n)
                .map(|_| alphabet[(next() % 5) as usize])
                .collect()
        };
        let mut agreed = [0usize; 2];
        for _ in 0..4_000 {
            let (a, b) = (word(9), word(9));
            for k in 0..=3 {
                let (ac, needle) = (a.chars().collect::<Vec<_>>(), Needle::from(b.as_str()));
                let want = old(&ac, &needle.chars, k);
                assert_eq!(
                    within(a.chars().count(), a.chars(), &needle.chars, k),
                    want,
                    "{a:?} {b:?} k {k}"
                );
                agreed[usize::from(want)] += 1;
            }
        }
        assert!(
            agreed.iter().all(|n| *n > 100),
            "both answers exercised: {agreed:?}"
        );
        let n = Needle::from("ſσk");
        assert_eq!(n.trigrams, pstore_format::trigram::trigrams("ſσk"));
        assert_eq!(n.as_str(), "ſσk");
    }

    #[test]
    fn a_regex_compiles_to_at_most_one_mebibyte() {
        // The limit is stated, not the crate's default. The boundary is found with the crate's
        // own builder at 1 MiB -- each probe past it fails fast -- and a pattern refuses exactly
        // from there: a larger limit would compile the first `\\w{n}` past it, a smaller one
        // refuse the last within.
        let source = |n: usize| format!("\\w{{{n}}}");
        let within = |n: usize| {
            regex::RegexBuilder::new(&source(n))
                .size_limit(1 << 20)
                .build()
                .is_ok()
        };
        let (mut lo, mut hi) = (1, 4096);
        assert!(within(lo) && !within(hi));
        while hi - lo > 1 {
            let mid = (lo + hi) / 2;
            if within(mid) { lo = mid } else { hi = mid }
        }
        assert!(
            Pattern::new(PatternKind::Regex, &source(lo)).is_ok(),
            "{lo} refused"
        );
        assert!(
            Pattern::new(PatternKind::Regex, &source(hi)).is_err(),
            "{hi} compiled"
        );
    }

    #[test]
    fn a_regex_requires_its_literal_runs_and_nothing_optional() {
        let required = |r: &str| {
            let mut got: Vec<String> = Pattern::new(PatternKind::Regex, r)
                .unwrap()
                .required
                .iter()
                .map(|t| t.iter().collect())
                .collect();
            got.sort();
            got
        };
        // A concatenation's runs; a capture's; a repetition of at least one.
        assert_eq!(required("abcd.wxyz"), ["ABC", "BCD", "WXY", "XYZ"]);
        assert_eq!(required("q(abc)q"), ["ABC"]);
        assert_eq!(required("(abc)+"), ["ABC"]);
        assert_eq!(required("(abc){2,}"), ["ABC"]);
        // A repetition that may be empty requires nothing.
        assert!(required("(abc)*").is_empty());
        assert!(required("(abc)?").is_empty());
        assert!(required("x(abc){0,3}y").is_empty());
    }

    #[test]
    fn a_bang_negates_a_glob_class_only_first() {
        let m = |g: &str, s: &str| Pattern::new(PatternKind::Glob, g).unwrap().re.is_match(s);
        assert!(m("[!a]", "b") && !m("[!a]", "a"));
        assert!(m("[a!]", "!") && m("[a!]", "a") && !m("[a!]", "b"));
        assert!(m("[!!]", "a") && !m("[!!]", "!"));
        assert!(m("[]!]", "!") && m("[]!]", "]"));
    }

    #[test]
    fn every_pattern_counts_toward_the_cap_through_every_combinator() {
        let glob = || {
            Predicate::Pattern(
                "a".to_owned(),
                Pattern::new(PatternKind::Glob, "x*").unwrap(),
            )
        };
        let fuzzy = || Predicate::Fuzzy("a".to_owned(), "abc".into(), 1);
        let not = |p| Predicate::Not(Box::new(p));
        assert_eq!(glob().patterns(), 1);
        assert_eq!(not(glob()).patterns(), 1);
        assert_eq!(not(not(fuzzy())).patterns(), 1);
        let tree = Predicate::And(vec![
            glob(),
            Predicate::Or(vec![not(fuzzy()), eq("n", 1), glob()]),
            not(Predicate::And(vec![glob(), fuzzy()])),
        ]);
        assert_eq!(tree.patterns(), 5);
        assert_eq!(eq("n", 1).patterns(), 0);
    }

    #[test]
    fn two_patterns_are_equal_by_kind_and_source() {
        let p = |kind| Pattern::new(kind, "a*").unwrap();
        assert_eq!(p(PatternKind::Glob), p(PatternKind::Glob));
        assert_ne!(p(PatternKind::Glob), p(PatternKind::IGlob));
        assert_ne!(p(PatternKind::Glob), p(PatternKind::Regex));
        assert_ne!(
            p(PatternKind::Glob),
            Pattern::new(PatternKind::Glob, "a?").unwrap()
        );
    }

    fn zones(pairs: &[(&str, i64, i64)]) -> Zones {
        Zones {
            ints: pairs
                .iter()
                .map(|(k, lo, hi)| ((*k).to_owned(), (*lo, *hi)))
                .collect(),
            ..Zones::default()
        }
    }

    fn eq(name: &str, n: i64) -> Predicate {
        Predicate::Cmp(name.to_owned(), Op::Eq, Value::Int(n))
    }

    #[test]
    fn a_not_never_prunes_because_a_zone_says_nothing_of_rows_without_the_attribute() {
        // A block holding `{a: 5}` and `{}` has the zone `a: (5, 5)`. `NotEq a 5` admits the
        // `{}` row, so pruning it by negating "cannot match" would lose that row.
        let z = zones(&[("a", 5, 5)]);
        assert!(!eq("a", 5).admits("x", &BTreeMap::new()) && !eq("a", 6).could_admit(&z));
        assert!(Predicate::Not(Box::new(eq("a", 5))).could_admit(&z));
        assert!(Predicate::Absent("a".to_owned()).could_admit(&z));
    }

    #[test]
    fn integer_comparisons_prune_at_their_exact_edges() {
        let z = zones(&[("a", 10, 20)]);
        let at = |op, n| Predicate::Cmp("a".to_owned(), op, Value::Int(n)).could_admit(&z);
        assert!(at(Op::Eq, 10) && at(Op::Eq, 20) && !at(Op::Eq, 9) && !at(Op::Eq, 21));
        assert!(at(Op::Lt, 11) && !at(Op::Lt, 10));
        assert!(at(Op::Lte, 10) && !at(Op::Lte, 9));
        assert!(at(Op::Gt, 19) && !at(Op::Gt, 20));
        assert!(at(Op::Gte, 20) && !at(Op::Gte, 21));
        let set = |vs: Vec<Value>| Predicate::In("a".to_owned(), vs).could_admit(&z);
        assert!(set(vec![Value::Int(3), Value::Int(15)]) && !set(vec![Value::Int(3)]));
        // A string member cannot prune: the zone covers only the name's integers.
        assert!(set(vec![Value::Str("x".to_owned())]));
    }

    #[test]
    fn and_prunes_if_any_child_does_or_only_if_all_do() {
        let z = zones(&[("a", 10, 20)]);
        let (yes, no) = (eq("a", 15), eq("a", 99));
        assert!(!Predicate::And(vec![yes.clone(), no.clone()]).could_admit(&z));
        assert!(Predicate::Or(vec![yes.clone(), no.clone()]).could_admit(&z));
        assert!(!Predicate::Or(vec![no.clone(), no]).could_admit(&z));
        assert!(Predicate::And(vec![]).could_admit(&z));
        assert!(!Predicate::Or(vec![]).could_admit(&z));
    }

    #[test]
    fn a_string_literal_prunes_a_datetime_zone_at_its_exact_edges() {
        // M9h.3, code review: the soundness harness sees rows lost, never blocks kept, so the
        // edges that make pruning PAY are pinned here.
        let us = |s: &str| datetime::parse(s).unwrap();
        let (lo, hi) = (us("2024-01-01T00:00:00Z"), us("2024-12-31T00:00:00Z"));
        let lit = |t: i64| Value::Str(datetime::format(t));
        let z = |strings: bool| Zones {
            datetimes: [("t".to_owned(), (lo, hi, strings))].into_iter().collect(),
            complete: true,
            ..Zones::default()
        };
        let at =
            |zones: &Zones, op, v: Value| Predicate::Cmp("t".to_owned(), op, v).could_admit(zones);
        let dated = z(false);
        for (op, no, yes) in [
            (Op::Eq, lo - 1, lo),
            (Op::Eq, hi + 1, hi),
            (Op::Lt, lo, lo + 1),
            (Op::Lte, lo - 1, lo),
            (Op::Gt, hi, hi - 1),
            (Op::Gte, hi + 1, hi),
        ] {
            assert!(!at(&dated, op, lit(no)), "{op:?} {no} pruned nothing");
            assert!(at(&dated, op, lit(yes)), "{op:?} {yes} was pruned");
        }
        // Not a date, so no datetime row matches, and the entry says there is no string row.
        assert!(!at(&dated, Op::Eq, Value::Str("not a date".to_owned())));
        // A string row beside the datetimes compares bytewise: nothing can be ruled out.
        let mixed = z(true);
        assert!(at(&mixed, Op::Eq, lit(hi + 1)));
        assert!(at(&mixed, Op::Eq, Value::Str("not a date".to_owned())));
        // No entry for the name: no datetime row, but a string row is unknown.
        let other = Predicate::Cmp("u".to_owned(), Op::Eq, lit(hi + 1));
        assert!(other.could_admit(&dated));
        // The id is described by no zone, even when an attribute of that name is.
        let id = Zones {
            datetimes: [(ID.to_owned(), (lo, hi, false))].into_iter().collect(),
            complete: true,
            ..Zones::default()
        };
        assert!(Predicate::Cmp(ID.to_owned(), Op::Eq, lit(hi + 1)).could_admit(&id));
    }

    /// M171: a UUID or bytes row against its own type's literal (a `$ref_new` resolves to
    /// one) and against a string spelling one; never against the other type or a string row.
    #[test]
    fn uuid_and_bytes_rows_compare_by_their_bytes() {
        let u = |n: u8| Value::Uuid([n; 16]);
        let y = |b: &[u8]| Value::Bytes(b.to_vec());
        let s = |x: &str| Value::Str(x.to_owned());
        let at = |row: Value, op, lit: Value| {
            let row = BTreeMap::from([("a".to_owned(), row)]);
            Predicate::Cmp("a".to_owned(), op, lit).admits("x", &row)
        };
        assert!(at(u(1), Op::Eq, u(1)) && !at(u(1), Op::Eq, u(2)));
        assert!(at(u(1), Op::Lt, u(2)) && !at(u(2), Op::Lt, u(1)));
        assert!(at(y(&[0, 1]), Op::Eq, y(&[0, 1])) && !at(y(&[0, 1]), Op::Eq, y(&[0])));
        assert!(at(y(&[0, 1]), Op::Gt, y(&[0])) && !at(y(&[0]), Op::Gt, y(&[0, 1])));
        let one = "01010101-0101-0101-0101-010101010101";
        assert!(at(u(1), Op::Eq, s(one)) && at(u(1), Op::Gte, s(one)));
        assert!(at(y(&[0, 1]), Op::Eq, s("AAE=")) && at(y(&[0, 2]), Op::Gt, s("AAE=")));
        // Never across types, and an unparseable literal compares with nothing.
        assert!(!at(u(1), Op::Eq, y(&[1; 16])) && !at(y(&[1; 16]), Op::Eq, u(1)));
        assert!(!at(s(one), Op::Eq, u(1)));
        assert!(!at(u(1), Op::Lt, s("zzz")) && !at(y(&[0]), Op::Lt, s("!!")));
    }

    #[test]
    fn a_datetime_literal_compares_by_instant() {
        // The engine API can pass a `DateTime` literal, which the server never builds (it
        // sends strings): it must compare by instant, not fall through to cross-type false.
        let row = BTreeMap::from([("t".to_owned(), Value::DateTime(10))]);
        let at = |op, t| Predicate::Cmp("t".to_owned(), op, Value::DateTime(t)).admits("x", &row);
        assert!(at(Op::Eq, 10) && !at(Op::Eq, 11));
        assert!(at(Op::Lt, 11) && !at(Op::Lt, 10));
        assert!(at(Op::Gt, 9) && !at(Op::Gt, 10));
    }

    #[test]
    fn a_missing_zone_never_prunes() {
        // A zone-free segment (M9a's fallback) has no zones at all.
        assert!(eq("a", 99).could_admit(&Zones::default()));
        assert!(Predicate::In("a".to_owned(), vec![Value::Int(1)]).could_admit(&Zones::default()));
    }

    #[test]
    fn a_cross_type_comparison_is_false_not_ordered() {
        // `Value` orders `Int` before `Str`; comparing through that would make every integer
        // "less than" every string.
        let a = BTreeMap::from([("a".to_owned(), Value::Int(1))]);
        let lt = Predicate::Cmp("a".to_owned(), Op::Lt, Value::Str("5".to_owned()));
        assert!(!lt.admits("x", &a));
        let gt = Predicate::Cmp("a".to_owned(), Op::Gt, Value::Str("5".to_owned()));
        assert!(!gt.admits("x", &a));
    }

    #[test]
    fn tokens_are_three_valued_and_never_admit_unbound() {
        use pstore_format::text::Analyzer;
        let row = BTreeMap::from([
            ("t".to_owned(), Value::Str("the quick brown fox".to_owned())),
            (
                "arr".to_owned(),
                Value::Array(vec![Value::Str("fox".to_owned())]),
            ),
        ]);
        let tok = |op, text: &str| Predicate::Tokens("t".to_owned(), op, text.to_owned(), None);
        let d = Analyzer::default();
        let yes = |p: Predicate| p.bound(&d).admits("x", &row);
        assert!(yes(tok(TokenOp::All, "fox QUICK")));
        assert!(!yes(tok(TokenOp::All, "fox wolf")));
        assert!(yes(tok(TokenOp::Any, "fox wolf")));
        assert!(!yes(tok(TokenOp::Any, "wolf")));
        assert!(yes(tok(TokenOp::Sequence, "quick brown")));
        assert!(!yes(tok(TokenOp::Sequence, "brown quick")));
        assert!(!yes(tok(TokenOp::Sequence, "quick fox")));
        // The vacuous readings of a query with no tokens.
        assert!(yes(tok(TokenOp::All, "")));
        assert!(yes(tok(TokenOp::Sequence, "!!")));
        assert!(!yes(tok(TokenOp::Any, "")));
        // An array of strings is matched element by element (M138, reversing M14.2); a missing
        // attribute admits nothing.
        let arr = Predicate::Tokens("arr".to_owned(), TokenOp::Any, "fox".to_owned(), None);
        assert!(yes(arr));
        assert!(!yes(Predicate::Tokens(
            "no".to_owned(),
            TokenOp::All,
            String::new(),
            None
        )));
        // Unbound: nothing, under every composition -- `Not` and `Or` included.
        let unbound = tok(TokenOp::Any, "wolf");
        let t = Predicate::And(vec![]);
        let f = Predicate::Or(vec![]);
        for p in [
            unbound.clone(),
            Predicate::Not(Box::new(unbound.clone())),
            Predicate::Or(vec![t.clone(), unbound.clone()]),
            Predicate::Not(Box::new(Predicate::And(vec![f, unbound.clone()]))),
        ] {
            assert!(!p.admits("x", &row), "{p:?}");
            assert!(
                p.bound(&d).admits("x", &row) || matches!(p, Predicate::Tokens(..)),
                "{p:?}"
            );
        }
        assert!(tok(TokenOp::All, "x").could_admit(&Zones::default()));
    }

    /// M145's reference: some substring of some string element within `k` edits of `q`, by
    /// the whole table over every start and end, on folded characters when case-insensitive.
    #[allow(
        clippy::indexing_slicing,
        reason = "the reference indexes its own tables"
    )]
    fn fuzzy_substring_reference(q: &str, k: usize, case: bool, v: Option<&Value>) -> bool {
        let key = |s: &str| -> Vec<char> {
            s.chars()
                .map(|c| {
                    if case {
                        c
                    } else {
                        pstore_format::trigram::fold(c)
                    }
                })
                .collect()
        };
        let lev = |a: &[char], b: &[char]| -> usize {
            let mut prev: Vec<usize> = (0..=b.len()).collect();
            for (i, ca) in a.iter().enumerate() {
                let mut row = vec![i + 1];
                for (j, cb) in b.iter().enumerate() {
                    let best = (prev[j] + usize::from(ca != cb))
                        .min(prev[j + 1] + 1)
                        .min(row[j] + 1);
                    row.push(best);
                }
                prev = row;
            }
            prev[b.len()]
        };
        let q = key(q);
        let hit = |s: &str| {
            let s = key(s);
            (0..=s.len()).any(|a| (a..=s.len()).any(|b| lev(&s[a..b], &q) <= k))
        };
        match v {
            Some(Value::Str(s)) => hit(s),
            Some(Value::Array(items)) => items.iter().any(|e| matches!(e, Value::Str(s) if hit(s))),
            _ => false,
        }
    }

    #[test]
    fn fuzzy_substrings_match_the_reference() {
        // M145 criterion 1.
        let alphabet = ['a', 'A', 'é', 'É'];
        let mut strings: Vec<String> = vec![String::new()];
        let mut last: Vec<String> = vec![String::new()];
        for _ in 0..4 {
            last = last
                .iter()
                .flat_map(|s| alphabet.iter().map(move |c| format!("{s}{c}")))
                .collect();
            strings.extend(last.iter().cloned());
        }
        let mut rows: Vec<Option<Value>> = vec![
            None,
            Some(Value::Int(5)),
            Some(Value::Array(vec![])),
            Some(Value::Array(vec![Value::Int(5)])),
        ];
        rows.extend(strings.iter().map(|s| Some(Value::Str(s.clone()))));
        for s in strings.iter().filter(|s| s.chars().count() == 4) {
            let s = Value::Str(s.clone());
            rows.push(Some(Value::Array(vec![
                Value::Str(String::new()),
                s.clone(),
            ])));
            rows.push(Some(Value::Array(vec![s, Value::Int(5)])));
        }
        let mut queries: Vec<String> = strings
            .iter()
            .filter(|s| s.chars().count() == 3)
            .cloned()
            .collect();
        queries.extend(["aAéÉ", "ééaaA", "AAAAA"].map(str::to_owned));
        let (mut admitted, mut refused) = (0usize, 0usize);
        for q in &queries {
            for k in 0..=2u8 {
                for case in [true, false] {
                    let p = Predicate::FuzzySubstring("a".to_owned(), q.as_str().into(), k, case);
                    let not = Predicate::Not(Box::new(p.clone()));
                    let not2 = Predicate::Not(Box::new(not.clone()));
                    for v in &rows {
                        let row: BTreeMap<String, Value> =
                            v.iter().map(|v| ("a".to_owned(), v.clone())).collect();
                        let want = fuzzy_substring_reference(q, usize::from(k), case, v.as_ref());
                        let case = format!("{q:?} k={k} case={case} over {v:?}");
                        assert_eq!(p.admits("x", &row), want, "{case}");
                        assert_eq!(not.admits("x", &row), !want, "Not {case}");
                        assert_eq!(not2.admits("x", &row), want, "Not Not {case}");
                        if want {
                            admitted += 1;
                        } else {
                            refused += 1;
                        }
                    }
                }
            }
        }
        assert!(
            admitted > 0 && refused > 0,
            "{admitted} admitted, {refused} refused"
        );
    }

    #[test]
    fn a_fuzzy_substring_never_prunes_on_the_sketch() {
        // M145 criterion 6: the block's scalar strings hold none of the value's trigrams, and
        // its arrays may hold a match the sketch never saw.
        let sketch = pstore_format::trigram::Sketch::build(&["s".to_owned()], 1, 256, |_, _| {
            vec!["zzzz".to_owned()]
        });
        let zones = pstore_format::Zones {
            sketch: Some((std::sync::Arc::new(sketch), 0)),
            ..pstore_format::Zones::default()
        };
        let m15 = Predicate::Fuzzy("s".to_owned(), "abcdef".into(), 1);
        assert!(
            !m15.could_admit(&zones),
            "the fixture must be one that prunes"
        );
        for case in [true, false] {
            let p = Predicate::FuzzySubstring("s".to_owned(), "abcdef".into(), 1, case);
            assert!(p.could_admit(&zones));
            assert!(Predicate::Not(Box::new(p)).could_admit(&zones));
        }
    }

    #[test]
    fn any_comparisons_match_the_reference() {
        // M135 criterion 1: every row shape against every literal, each operator, and the
        // compositions a client writes. The reference is the scalar comparison applied to each
        // element of an array, and false for anything that is not an array.
        let scalars = [
            Value::Int(-1),
            Value::Int(2),
            Value::Float(1.5),
            Value::Float(2.0),
            Value::Str("b".to_owned()),
            Value::Bool(false),
            Value::Bool(true),
            Value::DateTime(1_000_000),
        ];
        let mut rows: Vec<Option<Value>> = vec![None, Some(Value::Array(vec![]))];
        for a in &scalars {
            rows.push(Some(a.clone()));
            rows.push(Some(Value::Array(vec![a.clone()])));
            for b in &scalars {
                rows.push(Some(Value::Array(vec![a.clone(), b.clone()])));
            }
        }
        let literals = [
            Value::Int(-2),
            Value::Int(-1),
            Value::Int(0),
            Value::Int(2),
            Value::Int(3),
            Value::Float(1.5),
            Value::Float(1.75),
            Value::Str("a".to_owned()),
            Value::Str("b".to_owned()),
            Value::Str("c".to_owned()),
            Value::Bool(false),
            Value::Bool(true),
            Value::DateTime(999_999),
            Value::DateTime(1_000_000),
            Value::Str("1970-01-01T00:00:01Z".to_owned()),
            Value::Str("1970-01-01T00:00:02Z".to_owned()),
        ];
        let at = |v: &Option<Value>| -> BTreeMap<String, Value> {
            v.iter().map(|v| ("a".to_owned(), v.clone())).collect()
        };
        let reference = |op: Op, want: &Value, v: &Option<Value>| match v {
            Some(Value::Array(items)) => items.iter().any(|e| {
                Predicate::Cmp("a".to_owned(), op, want.clone()).admits("x", &at(&Some(e.clone())))
            }),
            _ => false,
        };
        let mut admitted = 0;
        for op in [Op::Lt, Op::Lte, Op::Gt, Op::Gte] {
            for want in &literals {
                let any = Predicate::AnyCmp("a".to_owned(), op, want.clone());
                let not = |p: Predicate| Predicate::Not(Box::new(p));
                let has = Predicate::ContainsAny("a".to_owned(), vec![want.clone()]);
                for v in &rows {
                    let r = reference(op, want, v);
                    let c = has.admits("x", &at(v));
                    let row = at(v);
                    let case = format!("{op:?} {want:?} over {v:?}");
                    assert_eq!(any.admits("x", &row), r, "{case}");
                    assert_eq!(not(any.clone()).admits("x", &row), !r, "Not {case}");
                    assert_eq!(not(not(any.clone())).admits("x", &row), r, "Not Not {case}");
                    let and = Predicate::And(vec![any.clone(), has.clone()]);
                    assert_eq!(and.admits("x", &row), r && c, "And {case}");
                    let or = Predicate::Or(vec![any.clone(), has.clone()]);
                    assert_eq!(or.admits("x", &row), r || c, "Or {case}");
                    admitted += usize::from(r);
                }
                // An array has no zone: a block is never ruled out.
                assert!(any.could_admit(&zones(&[("a", 100, 200)])));
                assert_eq!(any.names(), vec!["a"]);
            }
        }
        // The corpus is not vacuous: both outcomes occur.
        assert!(admitted > 0 && admitted < 4 * literals.len() * rows.len());
    }

    /// M138's reference for a token predicate, written from the spec rather than the code:
    /// per string element, the analyzer's terms and the `plain` forms (no stemming, no
    /// stopwords); the prefix the last `plain` form of the query.
    fn token_reference(
        op: TokenOp,
        a: &pstore_format::text::Analyzer,
        q: &str,
        v: Option<&Value>,
    ) -> bool {
        use pstore_format::text::{Analyzer, analyze, tokens};
        let elems: Vec<&str> = match v {
            Some(Value::Str(s)) => vec![s.as_str()],
            Some(Value::Array(xs)) => xs
                .iter()
                .filter_map(|x| match x {
                    Value::Str(s) => Some(s.as_str()),
                    _ => None,
                })
                .collect(),
            _ => vec![],
        };
        if elems.is_empty() {
            return false;
        }
        let plain = Analyzer {
            stemming: false,
            remove_stopwords: false,
            ..*a
        };
        let prefixed = matches!(op, TokenOp::AllPrefix | TokenOp::AnyPrefix);
        let raw = analyze(&plain, q);
        let (terms, prefix) = match (prefixed, raw.last(), tokens(a, q).last()) {
            (true, Some(last), Some(t)) => (analyze(a, &q[..t.span.start]), Some(last.clone())),
            _ => (analyze(a, q), None),
        };
        let rows: Vec<Vec<String>> = elems.iter().map(|e| analyze(a, e)).collect();
        let forms: Vec<Vec<String>> = elems.iter().map(|e| analyze(&plain, e)).collect();
        let has = |t: &String| rows.iter().any(|r| r.contains(t));
        let pre = |p: &String| forms.iter().flatten().any(|f| f.starts_with(p.as_str()));
        match op {
            TokenOp::All | TokenOp::AllPrefix => {
                terms.iter().all(has) && prefix.as_ref().is_none_or(pre)
            }
            TokenOp::Any | TokenOp::AnyPrefix => {
                terms.iter().any(has) || prefix.as_ref().is_some_and(pre)
            }
            TokenOp::Sequence => {
                terms.is_empty()
                    || rows
                        .iter()
                        .any(|r| r.windows(terms.len()).any(|w| w == terms))
            }
        }
    }

    fn analyzers() -> Vec<pstore_format::text::Analyzer> {
        use pstore_format::text::Analyzer;
        vec![
            Analyzer::default(),
            Analyzer {
                stemming: true,
                remove_stopwords: true,
                ..Analyzer::default()
            },
            Analyzer {
                case_sensitive: true,
                ..Analyzer::default()
            },
            Analyzer {
                ascii_folding: true,
                ..Analyzer::default()
            },
        ]
    }

    #[test]
    fn token_predicates_over_arrays_and_prefixes_match_the_reference() {
        // M138 criterion 1.
        let s = |x: &str| Value::Str(x.to_owned());
        let arr = |xs: &[&str]| Value::Array(xs.iter().map(|x| s(x)).collect());
        let rows: Vec<Option<Value>> = vec![
            None,
            Some(s("")),
            Some(s("The quick brown Fox")),
            Some(s("running theories café")),
            Some(Value::Array(vec![])),
            Some(Value::Array(vec![Value::Int(5)])),
            Some(arr(&["quick"])),
            Some(arr(&["quick", "brown fox"])),
            Some(arr(&["the quick", "the quick"])),
            Some(arr(&["Runs", "Café theory"])),
            Some(Value::Array(vec![
                Value::Int(5),
                s("brown"),
                Value::Bool(true),
            ])),
            Some(Value::Int(7)),
        ];
        let queries = [
            "",
            "!!",
            "fox",
            "the",
            "qu",
            "quick brown",
            "brown fox",
            "quick fox",
            "run",
            "running",
            "theory",
            "cafe",
            "Caf",
            "the quick br",
            "FOX the",
        ];
        let ops = [
            TokenOp::All,
            TokenOp::Any,
            TokenOp::Sequence,
            TokenOp::AllPrefix,
            TokenOp::AnyPrefix,
        ];
        let (mut admitted, mut refused) = (0usize, 0usize);
        for a in analyzers() {
            for op in ops {
                for q in queries {
                    let p = Predicate::Tokens("t".to_owned(), op, q.to_owned(), None).bound(&a);
                    for v in &rows {
                        let row: BTreeMap<String, Value> =
                            v.iter().map(|v| ("t".to_owned(), v.clone())).collect();
                        let want = token_reference(op, &a, q, v.as_ref());
                        let case = format!("{op:?} {q:?} over {v:?} with {a:?}");
                        assert_eq!(p.admits("x", &row), want, "{case}");
                        let not = Predicate::Not(Box::new(p.clone()));
                        assert_eq!(not.admits("x", &row), !want, "Not {case}");
                        let not2 = Predicate::Not(Box::new(not));
                        assert_eq!(not2.admits("x", &row), want, "Not Not {case}");
                        if want {
                            admitted += 1;
                        } else {
                            refused += 1;
                        }
                    }
                }
            }
        }
        assert!(
            admitted > 0 && refused > 0,
            "{admitted} admitted, {refused} refused"
        );
    }

    #[test]
    fn a_prefix_is_neither_stemmed_nor_dropped() {
        // M138 criterion 2, English stemming and stopwords on; each case a scalar string.
        use pstore_format::text::Analyzer;
        let en = Analyzer {
            stemming: true,
            remove_stopwords: true,
            ..Analyzer::default()
        };
        let folded = Analyzer {
            ascii_folding: true,
            ..en
        };
        let yes = |a: &Analyzer, q: &str, row: &str| {
            let p =
                Predicate::Tokens("t".to_owned(), TokenOp::AllPrefix, q.to_owned(), None).bound(a);
            p.admits(
                "x",
                &BTreeMap::from([("t".to_owned(), Value::Str(row.to_owned()))]),
            )
        };
        assert!(yes(&en, "run", "running"));
        // Stemmed, both pairs would match: run/run and theori/theori.
        assert!(!yes(&en, "running", "runs"));
        assert!(!yes(&en, "theory", "theories"));
        // A stopword as the prefix, and against a row the analyzer would drop it from.
        assert!(yes(&en, "the", "theory"));
        assert!(yes(&en, "quick the", "the quick"));
        assert!(!yes(&en, "quick the", "a quick"));
        // Never a shorter token.
        assert!(!yes(&en, "runner", "run"));
        assert!(yes(&folded, "cafe", "Café"));
        assert!(!yes(&en, "cafe", "Café"));
    }

    /// M156: a token predicate over a further field binds to that field's analyzer, through
    /// `And`, `Or` and `Not`; any other to the primary's. `token_fields` sees every one.
    #[test]
    fn a_further_fields_token_predicate_binds_to_its_own_analyzer() {
        use pstore_format::text::{Analyzer, FullText, Language};
        let stem = Analyzer {
            language: Language::English,
            stemming: true,
            ..Analyzer::default()
        };
        let texts = BTreeMap::from([(
            "title".to_owned(),
            FullText {
                analyzer: stem,
                ..FullText::default()
            },
        )]);
        let tok =
            |f: &str| Predicate::Tokens(f.to_owned(), TokenOp::Any, "Running".to_owned(), None);
        let p = Predicate::And(vec![
            tok("title"),
            Predicate::Not(Box::new(Predicate::Or(vec![tok("text"), tok("title")]))),
        ]);
        let mut seen = Vec::new();
        fn walk<'a>(p: &'a Predicate, out: &mut Vec<(&'a str, Analyzer)>) {
            match p {
                Predicate::Tokens(f, _, _, Some(b)) => out.push((f, *b.analyzer())),
                Predicate::And(ps) | Predicate::Or(ps) => ps.iter().for_each(|p| walk(p, out)),
                Predicate::Not(p) => walk(p, out),
                _ => {}
            }
        }
        let bound = p.bound_texts(&Analyzer::default(), &texts);
        walk(&bound, &mut seen);
        assert_eq!(
            seen,
            vec![
                ("title", stem),
                ("text", Analyzer::default()),
                ("title", stem)
            ]
        );
        // `bound` is `bound_texts` with no further field.
        assert_eq!(p.bound(&stem), p.bound_texts(&stem, &BTreeMap::new()));
        let run = BTreeMap::from([("title".to_owned(), Value::Str("run".to_owned()))]);
        let alone = tok("title");
        assert!(
            alone
                .bound_texts(&Analyzer::default(), &texts)
                .admits("x", &run)
        );
        assert!(!alone.bound(&Analyzer::default()).admits("x", &run));
        let mut fields = p.token_fields();
        fields.sort_unstable();
        assert_eq!(fields, ["text", "title", "title"]);
        assert!(
            Predicate::Cmp("n".to_owned(), Op::Eq, Value::Int(1))
                .token_fields()
                .is_empty()
        );
    }
}
