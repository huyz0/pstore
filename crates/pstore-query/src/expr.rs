//! Exact scoring expressions (M182): `BM25`, `Sum`, `Max`, `Product` and a filter as a score,
//! as a typed tree evaluated over every admitted row. M203 adds the numeric nodes:
//! `Attribute`, literals, `Saturate`, `Decay` and `Dist`.
//!
//! ⚠️ **A whole-corpus scan, not a retriever.** A row's BM25 here is re-derived from its stored
//! value -- re-analyzed with the function the segment builder posts it with -- against the
//! statistics a text leg sums, so a lone `BM25` clause is the leg's score without reading a
//! posting list. What that costs is every admitted block, as a `rank_by` order pays.

use crate::filter::Predicate;
use crate::order::{open, visit_opened};
use crate::run::{QueryError, Target, Texts, leg_terms};
use pstore_blob::BlobStore;
use pstore_format::{Document, Value};
use pstore_index::text::{Stats, TextIndex, row_score};
use std::collections::HashSet;

/// The most nodes an expression may have.
pub const MAX_NODES: usize = 64;
/// The deepest an expression may nest, the root at depth 1.
pub const MAX_DEPTH: usize = 8;
/// The most clauses a `Sum` or a `Max` may have.
pub const MAX_CLAUSES: usize = 16;
/// The least positive weight a `Product` may take.
pub const MIN_WEIGHT: f32 = 1e-3;
/// The greatest weight a `Product` may take.
pub const MAX_WEIGHT: f32 = 1e3;
/// The largest magnitude a literal, a `Dist` origin or a `Saturate`/`Decay` midpoint may
/// have, and a float attribute reads as (M203). With M182's limits it keeps every
/// intermediate below `1e63`: finite, never NaN.
pub const MAX_MAGNITUDE: f64 = 1e30;
/// The least midpoint a `Saturate` or `Decay` may take.
pub const MIN_MIDPOINT: f64 = 1e-9;
/// The greatest exponent a `Saturate` or `Decay` may take; the least is above 0.
pub const MAX_EXPONENT: f64 = 16.0;

/// What a `Dist` measures from (M203).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Origin {
    /// A number: a numeric value's distance from it.
    Number(f64),
    /// An instant, in microseconds: a datetime value's distance from it, in milliseconds.
    Instant(i64),
}

/// A scoring expression (M182). Every score an expression [`Expr::check`] accepts produces is
/// `>= 0`; a clause under it may be signed (M203).
#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    /// The score a text leg `{field, query, last_as_prefix}` gives the row, `0` where it
    /// does not hit it.
    Bm25 {
        /// The text field.
        field: String,
        /// The query: a string, or tokens joined as a text leg's (M158).
        query: String,
        /// The last raw token a literal prefix (M180).
        last_as_prefix: bool,
    },
    /// The clauses' scores added.
    Sum(Vec<Expr>),
    /// The largest of the clauses' scores.
    Max(Vec<Expr>),
    /// The clause's score times the weight.
    Product(f32, Box<Expr>),
    /// `1` where the predicate admits the row, else `0`.
    Filter(Predicate),
    /// The row's value when it is an int, a `uint64` or a float -- a float clamped to
    /// [`MAX_MAGNITUDE`] -- else no value (M203). Signed.
    Attribute(String),
    /// A number. Signed when it is negative.
    Literal(f64),
    /// `x^p / (x^p + m^p)` of `x = max(of, 0)`; `0` where `of` has no value.
    Saturate {
        /// The clause saturated.
        of: Box<Expr>,
        /// `m`, where it reaches `0.5`.
        midpoint: f64,
        /// `p`.
        exponent: f64,
    },
    /// `m^p / (x^p + m^p)` of `x = max(of, 0)`; `0` where `of` has no value.
    Decay {
        /// The clause decayed.
        of: Box<Expr>,
        /// `m`, where it reaches `0.5`.
        midpoint: f64,
        /// `p`.
        exponent: f64,
    },
    /// `|v - origin|` of the attribute's value `v`: a number's from a number, a datetime's
    /// from an instant in milliseconds, and no value otherwise.
    Dist {
        /// The attribute.
        attr: String,
        /// Where it is measured from.
        origin: Origin,
    },
}

impl Expr {
    /// Refuses an expression outside the limits: more than [`MAX_NODES`] nodes, deeper than
    /// [`MAX_DEPTH`], a clause list empty or longer than [`MAX_CLAUSES`], or a weight that is
    /// neither `0` nor in `[MIN_WEIGHT, MAX_WEIGHT]`.
    ///
    /// # Errors
    /// Why, in a sentence a caller can be shown.
    ///
    /// M203: also a literal, midpoint, exponent or origin out of range, and a root that is not
    /// [non-negative](Expr::non_negative).
    pub fn check(&self) -> Result<(), String> {
        let mut nodes = 0;
        self.check_at(1, &mut nodes)?;
        if self.non_negative() {
            Ok(())
        } else {
            Err(
                "an expression's score must be non-negative by its form: wrap a signed clause \
                 (an Attribute, a negative number) in Max with a non-negative clause, as \
                 [\"Max\", [0, clause]], or in Saturate, Decay or Dist"
                    .to_owned(),
            )
        }
    }

    /// Whether every score this node gives is `>= 0`, by its form alone (M203): `Attribute`
    /// and a negative literal are signed; a `Sum` is non-negative when every clause is, a
    /// `Product` when its clause is, a `Max` when **any** clause is; the rest always are.
    #[must_use]
    pub fn non_negative(&self) -> bool {
        match self {
            Self::Attribute(_) => false,
            Self::Literal(v) => *v >= 0.0,
            Self::Sum(all) => all.iter().all(Self::non_negative),
            Self::Max(all) => all.iter().any(Self::non_negative),
            Self::Product(_, e) => e.non_negative(),
            Self::Bm25 { .. }
            | Self::Filter(_)
            | Self::Saturate { .. }
            | Self::Decay { .. }
            | Self::Dist { .. } => true,
        }
    }

    fn check_at(&self, depth: usize, nodes: &mut usize) -> Result<(), String> {
        *nodes += 1;
        if *nodes > MAX_NODES {
            return Err(format!("an expression has at most {MAX_NODES} nodes"));
        }
        if depth > MAX_DEPTH {
            return Err(format!("an expression nests at most {MAX_DEPTH} deep"));
        }
        match self {
            Self::Bm25 { .. } | Self::Filter(_) | Self::Attribute(_) => Ok(()),
            Self::Literal(v) => magnitude_ok(*v)
                .then_some(())
                .ok_or_else(|| format!("a number is finite and at most {MAX_MAGNITUDE}, not {v}")),
            Self::Dist { origin, .. } => match origin {
                Origin::Number(o) if !magnitude_ok(*o) => Err(format!(
                    "a Dist's origin is finite and at most {MAX_MAGNITUDE}, not {o}"
                )),
                _ => Ok(()),
            },
            Self::Saturate {
                of,
                midpoint,
                exponent,
            }
            | Self::Decay {
                of,
                midpoint,
                exponent,
            } => {
                if !(MIN_MIDPOINT..=MAX_MAGNITUDE).contains(midpoint) {
                    return Err(format!(
                        "a midpoint is from {MIN_MIDPOINT} to {MAX_MAGNITUDE}, not {midpoint}"
                    ));
                }
                if !(*exponent > 0.0 && *exponent <= MAX_EXPONENT) {
                    return Err(format!(
                        "an exponent is above 0 and at most {MAX_EXPONENT}, not {exponent}"
                    ));
                }
                of.check_at(depth + 1, nodes)
            }
            Self::Sum(all) | Self::Max(all) => {
                if all.is_empty() || all.len() > MAX_CLAUSES {
                    return Err(format!(
                        "Sum and Max take 1 to {MAX_CLAUSES} clauses, not {}",
                        all.len()
                    ));
                }
                all.iter().try_for_each(|e| e.check_at(depth + 1, nodes))
            }
            Self::Product(w, e) => {
                if !weight_ok(*w) {
                    return Err(format!(
                        "a Product's weight is 0 or from {MIN_WEIGHT} to {MAX_WEIGHT}, not {w}"
                    ));
                }
                e.check_at(depth + 1, nodes)
            }
        }
    }

    /// This expression with every filter in it bound to the index's analyzers (M156), as a
    /// query's `filters` are.
    #[must_use]
    pub fn bound_texts(
        &self,
        primary: &pstore_format::text::Analyzer,
        texts: &std::collections::BTreeMap<String, pstore_format::text::FullText>,
    ) -> Self {
        match self {
            Self::Filter(p) => Self::Filter(p.bound_texts(primary, texts)),
            Self::Sum(all) => {
                Self::Sum(all.iter().map(|e| e.bound_texts(primary, texts)).collect())
            }
            Self::Max(all) => {
                Self::Max(all.iter().map(|e| e.bound_texts(primary, texts)).collect())
            }
            Self::Product(w, e) => Self::Product(*w, Box::new(e.bound_texts(primary, texts))),
            Self::Saturate {
                of,
                midpoint,
                exponent,
            } => Self::Saturate {
                of: Box::new(of.bound_texts(primary, texts)),
                midpoint: *midpoint,
                exponent: *exponent,
            },
            Self::Decay {
                of,
                midpoint,
                exponent,
            } => Self::Decay {
                of: Box::new(of.bound_texts(primary, texts)),
                midpoint: *midpoint,
                exponent: *exponent,
            },
            leaf @ (Self::Bm25 { .. }
            | Self::Attribute(_)
            | Self::Literal(_)
            | Self::Dist { .. }) => leaf.clone(),
        }
    }

    /// The fields its `BM25` clauses read, in name order, each once.
    #[must_use]
    pub fn fields(&self) -> Vec<&str> {
        let mut out = Vec::new();
        self.walk(&mut |e| {
            if let Self::Bm25 { field, .. } = e {
                out.push(field.as_str());
            }
        });
        out.sort_unstable();
        out.dedup();
        out
    }

    /// Every filter in it, as a score.
    #[must_use]
    pub fn filters(&self) -> Vec<&Predicate> {
        let mut out = Vec::new();
        self.walk(&mut |e| {
            if let Self::Filter(p) = e {
                out.push(p);
            }
        });
        out
    }

    fn walk<'a>(&'a self, f: &mut impl FnMut(&'a Self)) {
        f(self);
        match self {
            Self::Sum(all) | Self::Max(all) => all.iter().for_each(|e| e.walk(f)),
            Self::Product(_, e) | Self::Saturate { of: e, .. } | Self::Decay { of: e, .. } => {
                e.walk(f);
            }
            Self::Bm25 { .. }
            | Self::Filter(_)
            | Self::Attribute(_)
            | Self::Literal(_)
            | Self::Dist { .. } => {}
        }
    }
}

/// A kept score as the wire's `f32` carries it: narrowed, and clamped into the smallest
/// positive `f32` to `f32::MAX` (M203), so a kept row never reports `0` nor infinity.
#[must_use]
pub fn wire_score(s: f64) -> f32 {
    #[expect(
        clippy::cast_possible_truncation,
        reason = "clamped into f32's positive range first"
    )]
    let narrowed = s.clamp(f64::from(f32::from_bits(1)), f64::from(f32::MAX)) as f32;
    narrowed
}

/// Whether `v` is finite and within [`MAX_MAGNITUDE`].
fn magnitude_ok(v: f64) -> bool {
    v.abs() <= MAX_MAGNITUDE
}

/// Whether `w` is a weight [`Expr::check`] accepts.
fn weight_ok(w: f32) -> bool {
    w == 0.0 || (MIN_WEIGHT..=MAX_WEIGHT).contains(&w)
}

/// Every row of `targets` and `unfolded` that `filter` admits, less deleted and shadowed rows,
/// scored by `expr`: those scoring above `0`, by score descending then id ascending, the first
/// `cap` of them (M182). Each score is the `f64` the rows were ordered by: the wire's `f32`
/// is narrowed from it, and two rows apart here can be equal there.
///
/// **Three rounds** with HEAD, which the caller read: each segment's footer, delete vector
/// and -- when `expr` has a `BM25` clause -- term dictionary, in one fan-out; then each
/// segment's admitted blocks, in one coalesced plan. No posting list is read.
///
/// # Errors
/// An expression [`Expr::check`] refuses; a segment, delete vector, dictionary or block that
/// cannot be read or decoded.
#[allow(
    clippy::too_many_arguments,
    reason = "a scan's scope, its expression, and its bound"
)]
pub async fn rank<S: BlobStore>(
    store: &S,
    targets: &[Target],
    filter: Option<&Predicate>,
    shadow: &HashSet<String>,
    expr: &Expr,
    texts: &Texts,
    unfolded: &[Document],
    cap: usize,
) -> Result<Vec<(Document, f64)>, QueryError> {
    expr.check().map_err(QueryError::Expression)?;
    let mut fields = Fields::of(expr, texts);
    let opened = open(store, targets, !fields.0.is_empty()).await?;
    for f in &mut fields.0 {
        f.stats = Stats::merge(
            opened
                .iter()
                .filter(|o| o.segment.text_fields().contains(&f.name))
                .filter_map(|o| o.dictionary.as_ref().map(|d| (&o.segment, d)))
                .map(|(segment, d)| {
                    TextIndex::for_field(segment, d, &f.name).map(|idx| idx.summary_for(&f.terms))
                })
                .chain(std::iter::once(Ok(f.unfolded_stats(unfolded))))
                .collect::<Result<Vec<_>, _>>()?,
        );
    }
    let node = Node::of(expr, &fields);
    let fields = &fields;
    let node = &node;
    let per_segment = visit_opened(
        store,
        targets,
        &opened,
        filter,
        shadow,
        || Top::new(cap),
        |top, doc| {
            let s = node.score(fields, &doc, &mut vec![None; fields.0.len()]);
            top.offer(s, doc);
        },
    )
    .await?;
    let mut top = Top::new(cap);
    for (s, d) in per_segment.into_iter().flat_map(|t| t.kept) {
        top.offer(s, d);
    }
    for d in unfolded {
        if filter.is_none_or(|f| f.admits(&d.id, &d.attrs)) {
            let s = node.score(fields, d, &mut vec![None; fields.0.len()]);
            // Asked first, so a row it would refuse is never copied.
            if top.wants(s) {
                // As a block's rows are: attributes, no vectors.
                top.offer(
                    s,
                    Document {
                        id: d.id.clone(),
                        vectors: std::collections::BTreeMap::new(),
                        attrs: d.attrs.clone(),
                    },
                );
            }
        }
    }
    Ok(top.into_sorted().into_iter().map(|(s, d)| (d, s)).collect())
}

/// Each text field the expression's `BM25` clauses read: how it is analyzed and scored, the
/// clauses' terms, and -- once gathered -- its statistics.
struct Fields(Vec<Field>);

struct Field {
    name: String,
    text: pstore_format::text::FullText,
    /// Every term of every clause over the field, sorted and distinct: the statistics' keys.
    terms: Vec<String>,
    stats: Stats,
}

impl Fields {
    fn of(expr: &Expr, texts: &Texts) -> Self {
        Self(
            expr.fields()
                .into_iter()
                .map(|name| {
                    let text = *texts.of(name);
                    let mut terms = Vec::new();
                    expr.walk(&mut |e| {
                        if let Expr::Bm25 {
                            field,
                            query,
                            last_as_prefix,
                        } = e
                            && field == name
                        {
                            terms.extend(leg_terms(&text.analyzer, query, *last_as_prefix).0);
                        }
                    });
                    terms.sort_unstable();
                    terms.dedup();
                    Field {
                        name: name.to_owned(),
                        text,
                        terms,
                        stats: Stats::default(),
                    }
                })
                .collect(),
        )
    }

    fn index(&self, name: &str) -> usize {
        self.0.iter().position(|f| f.name == name).unwrap_or(0)
    }
}

impl Field {
    /// The unfolded rows' statistics, by the rule a sealed segment's dictionary counts them:
    /// nothing when no row has text under the field, else every row, those lacking it at a
    /// length of 0.
    fn unfolded_stats(&self, unfolded: &[Document]) -> Stats {
        if !unfolded.iter().any(|d| {
            d.attrs
                .get(&self.name)
                .is_some_and(pstore_format::text::has_text)
        }) {
            return Stats::default();
        }
        let mut stats = Stats {
            doc_count: unfolded.len() as u64,
            ..Stats::default()
        };
        for d in unfolded {
            let row = self.row(d);
            stats.total_tokens += row.len() as u64;
            for t in &self.terms {
                if row.contains(t) {
                    *stats.df.entry(t.clone()).or_insert(0) += 1;
                }
            }
        }
        stats
    }

    /// The row's terms under the field, as the segment builder posts them.
    fn row(&self, d: &Document) -> Vec<String> {
        d.attrs
            .get(&self.name)
            .map(|v| pstore_format::text::value_terms(v, &self.text.analyzer))
            .unwrap_or_default()
    }
}

/// An expression prepared to score rows: each `BM25` clause's terms and prefixes made once.
enum Node<'a> {
    Bm25 {
        field: usize,
        terms: Vec<String>,
        prefixes: Vec<String>,
    },
    Sum(Vec<Node<'a>>),
    Max(Vec<Node<'a>>),
    Product(f64, Box<Node<'a>>),
    Filter(&'a Predicate),
    Attribute(&'a str),
    Literal(f64),
    Saturate(Box<Node<'a>>, f64, f64),
    Decay(Box<Node<'a>>, f64, f64),
    Dist(&'a str, Origin),
}

impl<'a> Node<'a> {
    fn of(e: &'a Expr, fields: &Fields) -> Self {
        match e {
            Expr::Bm25 {
                field,
                query,
                last_as_prefix,
            } => {
                let i = fields.index(field);
                let analyzer = fields.0.get(i).map(|f| f.text.analyzer).unwrap_or_default();
                let (terms, prefixes) = leg_terms(&analyzer, query, *last_as_prefix);
                Self::Bm25 {
                    field: i,
                    terms,
                    prefixes,
                }
            }
            Expr::Sum(all) => Self::Sum(all.iter().map(|e| Self::of(e, fields)).collect()),
            Expr::Max(all) => Self::Max(all.iter().map(|e| Self::of(e, fields)).collect()),
            Expr::Product(w, e) => Self::Product(f64::from(*w), Box::new(Self::of(e, fields))),
            Expr::Filter(p) => Self::Filter(p),
            Expr::Attribute(a) => Self::Attribute(a),
            Expr::Literal(v) => Self::Literal(*v),
            Expr::Saturate {
                of,
                midpoint,
                exponent,
            } => Self::Saturate(Box::new(Self::of(of, fields)), *midpoint, *exponent),
            Expr::Decay {
                of,
                midpoint,
                exponent,
            } => Self::Decay(Box::new(Self::of(of, fields)), *midpoint, *exponent),
            Expr::Dist { attr, origin } => Self::Dist(attr, *origin),
        }
    }

    /// The row's score, in `f64`: its value, no value as `0`. `rows` holds each field's terms
    /// of this row, made the first time a clause reads them.
    fn score(&self, fields: &Fields, d: &Document, rows: &mut [Option<Vec<String>>]) -> f64 {
        self.value(fields, d, rows).unwrap_or(0.0)
    }

    /// The row's value, `None` for no value: only an `Attribute` or a `Dist` has none.
    fn value(
        &self,
        fields: &Fields,
        d: &Document,
        rows: &mut [Option<Vec<String>>],
    ) -> Option<f64> {
        Some(match self {
            Self::Attribute(a) => return number(d.attrs.get(*a)?),
            Self::Literal(v) => *v,
            Self::Dist(a, origin) => return distance(d.attrs.get(*a)?, *origin),
            Self::Saturate(n, m, p) => n
                .value(fields, d, rows)
                .map_or(0.0, |x| saturate(x, *m, *p)),
            Self::Decay(n, m, p) => n.value(fields, d, rows).map_or(0.0, |x| decay(x, *m, *p)),
            Self::Bm25 { .. }
            | Self::Sum(_)
            | Self::Max(_)
            | Self::Product(..)
            | Self::Filter(_) => self.relevance(fields, d, rows),
        })
    }

    /// M182's nodes, which always have a value.
    fn relevance(&self, fields: &Fields, d: &Document, rows: &mut [Option<Vec<String>>]) -> f64 {
        match self {
            Self::Bm25 {
                field,
                terms,
                prefixes,
            } => {
                let (Some(f), Some(row)) = (fields.0.get(*field), rows.get_mut(*field)) else {
                    return 0.0;
                };
                let row = row.get_or_insert_with(|| f.row(d));
                f64::from(row_score(
                    terms, prefixes, row, &f.stats, f.text.k1, f.text.b,
                ))
            }
            Self::Sum(all) => all.iter().map(|n| n.score(fields, d, rows)).sum(),
            // From below every value, as a clause may be signed (M203).
            Self::Max(all) => all
                .iter()
                .map(|n| n.score(fields, d, rows))
                .fold(f64::NEG_INFINITY, f64::max),
            Self::Product(w, n) => w * n.score(fields, d, rows),
            Self::Filter(p) => f64::from(u8::from(p.admits(&d.id, &d.attrs))),
            Self::Attribute(_)
            | Self::Literal(_)
            | Self::Saturate(..)
            | Self::Decay(..)
            | Self::Dist(..) => self.score(fields, d, rows),
        }
    }
}

/// A numeric value as `Attribute` reads it: a float clamped to [`MAX_MAGNITUDE`], and no
/// value for any other kind.
#[allow(
    clippy::cast_precision_loss,
    reason = "an attribute is scored in f64, as a float attribute is stored"
)]
fn number(v: &Value) -> Option<f64> {
    match v {
        Value::Int(i) => Some(*i as f64),
        Value::Uint(u) => Some(*u as f64),
        Value::Float(f) => Some(f.clamp(-MAX_MAGNITUDE, MAX_MAGNITUDE)),
        _ => None,
    }
}

/// `|v - origin|`: a number's from a number, a datetime's from an instant in milliseconds.
#[allow(
    clippy::cast_precision_loss,
    reason = "a distance in milliseconds is scored in f64"
)]
fn distance(v: &Value, origin: Origin) -> Option<f64> {
    match (origin, v) {
        (Origin::Number(o), v) => number(v).map(|x| (x - o).abs()),
        // Both within `datetime::MIN..=MAX`, so the difference fits an i64.
        (Origin::Instant(o), Value::DateTime(t)) => Some((t.abs_diff(o) as f64) / 1_000.0),
        (Origin::Instant(_), _) => None,
    }
}

/// `x^p / (x^p + m^p)` of `max(x, 0)`, as `1 / (1 + (m/x)^p)`: no power overflows. At `0`,
/// `m/0` is infinite and the score `0`, so no branch is needed.
fn saturate(x: f64, m: f64, p: f64) -> f64 {
    1.0 / (1.0 + (m / x.max(0.0)).powf(p))
}

/// `m^p / (x^p + m^p)` of `max(x, 0)`, as `1 / (1 + (x/m)^p)`: `1` at `0`.
fn decay(x: f64, m: f64, p: f64) -> f64 {
    1.0 / (1.0 + (x.max(0.0) / m).powf(p))
}

/// The first `cap` rows scoring above 0, by score descending then id ascending.
///
/// ⚠️ **Bounded**: it holds at most twice `cap` rows, pruned back to `cap` each time it
/// fills, so an expression over a large index holds its answer and not its corpus.
struct Top {
    cap: usize,
    kept: Vec<(f64, Document)>,
}

impl Top {
    fn new(cap: usize) -> Self {
        Self {
            cap,
            kept: Vec::new(),
        }
    }

    /// Whether a row scoring `s` is kept, for now: above 0, and room for it.
    fn wants(&self, s: f64) -> bool {
        self.cap > 0 && s > 0.0
    }

    fn offer(&mut self, s: f64, d: Document) {
        if !self.wants(s) {
            return;
        }
        self.kept.push((s, d));
        if self.kept.len() >= self.cap.saturating_mul(2).max(64) {
            self.prune();
        }
    }

    fn prune(&mut self) {
        self.kept
            .sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.1.id.cmp(&b.1.id)));
        self.kept.truncate(self.cap);
    }

    fn into_sorted(mut self) -> Vec<(f64, Document)> {
        self.prune();
        self.kept
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(i: usize) -> Document {
        Document::new(format!("d{i:03}"), vec![0.0])
    }

    /// Pruned only when it fills, so a row costs a push and not a sort; held to the bound.
    #[test]
    fn top_prunes_when_full_and_never_holds_more() {
        let mut top = Top::new(10);
        for i in 0..63 {
            top.offer(1.0 + i as f64, doc(i));
        }
        assert_eq!(top.kept.len(), 63, "not pruned before it fills");
        top.offer(100.0, doc(63));
        assert_eq!(top.kept.len(), 10, "pruned when it fills");
        for i in 64..1_000 {
            top.offer(i as f64, doc(i));
            assert!(top.kept.len() < 64);
        }
        let ids: Vec<String> = top.into_sorted().into_iter().map(|(_, d)| d.id).collect();
        assert_eq!(
            ids,
            (990..1_000)
                .rev()
                .map(|i| format!("d{i:03}"))
                .collect::<Vec<_>>()
        );
    }

    /// M203 criterion 3: a kept score never narrows to `0` or to infinity.
    #[test]
    fn wire_scores_stay_positive() {
        assert_eq!(wire_score(1e-50).to_bits(), 1, "the smallest positive f32");
        assert!(wire_score(1e-40) > 0.0, "a subnormal f32");
        assert_eq!(wire_score(1e60).to_bits(), f32::MAX.to_bits());
        assert_eq!(wire_score(2.5).to_bits(), 2.5f32.to_bits());
        assert!(wire_score(1e-30) > 0.0);
    }

    #[test]
    fn top_refuses_zero_and_an_empty_cap() {
        let mut top = Top::new(3);
        top.offer(0.0, doc(0));
        assert!(top.kept.is_empty());
        assert!(!Top::new(0).wants(1.0));
        assert!(Top::new(1).wants(f64::MIN_POSITIVE));
    }
}
