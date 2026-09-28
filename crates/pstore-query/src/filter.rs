//! Filters — which documents a query may answer with (M9b).
//!
//! ⚠️ **Evaluated before a leg's limit, never after it.** A predicate applied to a top-`k`
//! returns fewer than `k` whenever it is selective, and nothing says so. `run` therefore
//! builds each segment's [`Mask`] from its blocks and applies it to exhaustive legs.

use pstore_format::{Number, Value, Zones, cmp_numbers, datetime};
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
}

/// A token predicate's analyzer, and its text already analyzed by it (M14.2): analyzed once
/// when bound, never once per row (code review).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bound {
    analyzer: pstore_format::text::Analyzer,
    query: Vec<String>,
}

impl Bound {
    /// `text` analyzed by `analyzer`.
    #[must_use]
    pub fn new(analyzer: pstore_format::text::Analyzer, text: &str) -> Self {
        Self {
            analyzer,
            query: pstore_format::text::analyze(&analyzer, text),
        }
    }

    /// The analyzer it was bound to.
    #[must_use]
    pub fn analyzer(&self) -> &pstore_format::text::Analyzer {
        &self.analyzer
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
                for c in it.by_ref() {
                    if c == ']' {
                        break;
                    }
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
            '[' => {
                let mut class = String::from("[");
                let mut first = true;
                loop {
                    let c = it.next().ok_or("a glob has an unterminated [")?;
                    match c {
                        ']' if !first => break,
                        '!' if first => class.push('^'),
                        '-' if !first && !class.ends_with('[') && !class.ends_with('^') => {
                            class.push('-');
                        }
                        c => class.push_str(&lit(c)),
                    }
                    first = false;
                }
                class.push(']');
                out.push_str(&class);
            }
            c => out.push_str(&lit(c)),
        }
    }
    out.push_str("\\z");
    Ok(out)
}

/// Whether `a` is within `k` edits of `b`, in characters (M15): a band of width `2k+1`, and
/// out at once when the lengths differ by more.
fn within(a: &[char], b: &[char], k: usize) -> bool {
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
    Fuzzy(String, String, u8),
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
        match self {
            Self::Tokens(name, op, text, _) => Self::Tokens(
                name.clone(),
                *op,
                text.clone(),
                Some(Bound::new(*analyzer, text)),
            ),
            Self::And(all) => Self::And(all.iter().map(|p| p.bound(analyzer)).collect()),
            Self::Or(any) => Self::Or(any.iter().map(|p| p.bound(analyzer)).collect()),
            Self::Not(p) => Self::Not(Box::new(p.bound(analyzer))),
            other => other.clone(),
        }
    }

    /// Two-valued, for a predicate [`Self::admits`] has found fully bound.
    fn holds(&self, id: &str, attrs: &BTreeMap<String, Value>) -> bool {
        let get = |name: &str| -> Option<Value> {
            if name == ID {
                Some(Value::Str(id.to_owned()))
            } else {
                attrs.get(name).cloned()
            }
        };
        match self {
            Self::Cmp(name, op, want) => get(name).is_some_and(|have| compare(&have, *op, want)),
            Self::Absent(name) => get(name).is_none(),
            Self::In(name, set) => {
                get(name).is_some_and(|have| set.iter().any(|w| compare(&have, Op::Eq, w)))
            }
            Self::ContainsAny(name, set) => match get(name) {
                Some(Value::Array(items)) => items
                    .iter()
                    .any(|have| set.iter().any(|w| compare(have, Op::Eq, w))),
                _ => false,
            },
            Self::And(all) => all.iter().all(|p| p.holds(id, attrs)),
            Self::Or(any) => any.iter().any(|p| p.holds(id, attrs)),
            Self::Not(p) => !p.holds(id, attrs),
            // Unbound never reaches here: `admits` checked first.
            Self::Tokens(name, op, _, bound) => match (get(name), bound) {
                (Some(Value::Str(s)), Some(b)) => tokens_match(
                    *op,
                    &b.query,
                    &pstore_format::text::analyze(&b.analyzer, &s),
                ),
                _ => false,
            },
            Self::Pattern(name, p) => match get(name) {
                Some(Value::Str(s)) => p.re.is_match(&s),
                _ => false,
            },
            Self::Fuzzy(name, v, k) => match get(name) {
                Some(Value::Str(s)) => within(
                    &s.chars().collect::<Vec<_>>(),
                    &v.chars().collect::<Vec<_>>(),
                    usize::from(*k),
                ),
                _ => false,
            },
        }
    }

    /// How many patterns it holds, fuzzy ones included (M15): what the door caps.
    #[must_use]
    pub fn patterns(&self) -> usize {
        match self {
            Self::Pattern(..) | Self::Fuzzy(..) => 1,
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
            Self::ContainsAny(..) | Self::Absent(_) | Self::Not(_) => true,
            // Tokens have no zone: every block could hold a match (M14.2).
            Self::Tokens(..) => true,
            // M15.2: a block whose sketch lacks a required trigram holds no match. No sketch,
            // or no filter for the attribute, rules nothing out.
            Self::Pattern(name, p) => zones
                .sketch
                .as_ref()
                .is_none_or(|(s, b)| s.may_hold(name, *b, &p.required)),
            // The q-gram lemma: within k edits, a value keeps all but at most 3k of `v`'s
            // distinct trigrams, so a block holding fewer cannot match.
            Self::Fuzzy(name, v, k) => {
                let tris = pstore_format::trigram::trigrams(v);
                let need = tris.len().saturating_sub(3 * usize::from(*k));
                need == 0
                    || zones
                        .sketch
                        .as_ref()
                        .is_none_or(|(s, b)| s.held(name, *b, &tris).is_none_or(|n| n >= need))
            }
        }
    }
}

/// Whether a row's tokens satisfy `op` against the query's (M14.2). An empty query is the
/// vacuous reading: every string for `All` and `Sequence`, none for `Any`.
fn tokens_match(op: TokenOp, query: &[String], row: &[String]) -> bool {
    match op {
        TokenOp::All => query.iter().all(|q| row.contains(q)),
        TokenOp::Any => query.iter().any(|q| row.contains(q)),
        TokenOp::Sequence => query.is_empty() || row.windows(query.len()).any(|w| w == query),
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
pub(crate) type Mask = std::collections::HashSet<usize>;

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    reason = "assertions in tests are the reporting mechanism"
)]
mod tests {
    use super::*;

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
        // An array of strings, or a missing attribute, admits nothing.
        let arr = Predicate::Tokens("arr".to_owned(), TokenOp::Any, "fox".to_owned(), None);
        assert!(!yes(arr));
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
}
