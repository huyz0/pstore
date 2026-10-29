//! A result's text with the query's terms marked (M63).
//!
//! ⚠️ **Marks exactly the tokens BM25 matched.** The tokens and their terms come from
//! `pstore_format::text::tokens`, the pipeline `analyze` is built on, under the analyzer the
//! query was answered with. It costs no request: the text is an attribute the row round
//! already returned.
//!
//! Characters are Unicode scalar values throughout. The text is **not escaped**: `pre` and
//! `post` are the client's, and so is keeping the output safe for where it is shown.
//!
//! M159: naming `fragment_by`, `fragment_limit` or `include_offsets` makes a highlight
//! **structured**: a list of fragments with their offsets into the value, and string arrays
//! read element by element.

use pstore_format::text::{Analyzer, extend_over_marks, tokens};
use std::collections::HashSet;

/// Bytes `pre` and `post` may each hold.
const MAX_MARK: usize = 16;
/// Characters a fragment may hold at most.
const MAX_CHARS: usize = 2000;
/// Characters a fragment holds when the query names none.
const DEFAULT_CHARS: usize = 200;
/// The ellipsis of a cut fragment: one character, no space.
const ELLIPSIS: char = '…';
/// Fragments a structured highlight returns at most (M159).
const MAX_FRAGMENTS: usize = 16;

/// What a structured highlight splits a value into before it seeks fragments (M159).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FragmentBy {
    /// The whole value, windowed over its tokens.
    Word,
    /// Sentences, within paragraphs.
    Sentence,
    /// Runs separated by whitespace holding a line feed.
    Paragraph,
    /// The whole value's first `max_chars` characters.
    None,
}

/// What a structured highlight's offsets count (M159).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Units {
    /// UTF-8 bytes.
    Utf8,
    /// UTF-16 code units.
    Utf16,
    /// Unicode scalar values.
    Codepoints,
}

/// A structured highlight's configuration (M159).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Structure {
    by: FragmentBy,
    limit: usize,
    offsets: Option<Units>,
}

/// One fragment of a structured highlight (M159). Ranges are `[start, end)` into the value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Fragment {
    /// The fragment, its marked tokens wrapped; no ellipsis.
    pub(crate) text: String,
    /// Where it sits in the value, under `include_offsets`.
    pub(crate) range: Option<[usize; 2]>,
    /// Where its marks sit in the value, under `include_offsets`.
    pub(crate) marks: Option<Vec<[usize; 2]>>,
    /// The array element it came from, for an array.
    pub(crate) array_index: Option<usize>,
}

/// A parsed `highlight`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Highlight {
    /// The string attribute to mark.
    pub(crate) attr: String,
    pre: String,
    post: String,
    max_chars: usize,
    /// Present when the highlight is structured (M159).
    pub(crate) structure: Option<Structure>,
}

/// `raw` as a highlight, or why it is not one.
///
/// # Errors
/// Every refusal of M63's rule 6, as a message for a `400`.
pub(crate) fn parse(raw: &serde_json::Value) -> Result<Highlight, String> {
    let o = raw
        .as_object()
        .ok_or("highlight is {\"attr\", \"pre\", \"post\", \"max_chars\"}")?;
    // `rank_fragments_by` (M159) is refused here by name: its value has no published shape.
    if let Some(k) = o.keys().find(|k| {
        !matches!(
            k.as_str(),
            "attr"
                | "pre"
                | "post"
                | "max_chars"
                | "fragment_by"
                | "fragment_limit"
                | "include_offsets"
        )
    }) {
        return Err(format!(
            "{k:?} is not a highlight field; attr, pre, post, max_chars, fragment_by, \
             fragment_limit and include_offsets are"
        ));
    }
    let string = |k: &str, default: &str, limit: usize| -> Result<String, String> {
        match o.get(k) {
            None => Ok(default.to_owned()),
            Some(serde_json::Value::String(s)) if s.len() <= limit => Ok(s.clone()),
            Some(_) => Err(format!("{k} is a string of at most {limit} bytes")),
        }
    };
    let attr = string("attr", pstore_format::text::DEFAULT_TEXT_FIELD, 1024)?;
    if attr.is_empty() || attr == pstore_query::ID_ATTRIBUTE || attr.starts_with('$') {
        return Err(format!(
            "attr {attr:?}: a highlight reads an attribute, never empty, `id` or one beginning \
             with `$`"
        ));
    }
    let max_chars = match o.get("max_chars") {
        None => DEFAULT_CHARS,
        Some(v) => v
            .as_u64()
            .and_then(|n| usize::try_from(n).ok())
            .filter(|n| (1..=MAX_CHARS).contains(n))
            .ok_or(format!("max_chars is a whole number from 1 to {MAX_CHARS}"))?,
    };
    Ok(Highlight {
        attr,
        pre: string("pre", "<em>", MAX_MARK)?,
        post: string("post", "</em>", MAX_MARK)?,
        max_chars,
        structure: structure(o)?,
    })
}

/// M159: the structured keys of `o`, or `None` when it names none of them.
fn structure(o: &serde_json::Map<String, serde_json::Value>) -> Result<Option<Structure>, String> {
    if !["fragment_by", "fragment_limit", "include_offsets"]
        .iter()
        .any(|k| o.contains_key(*k))
    {
        return Ok(None);
    }
    let by = match o.get("fragment_by").map(serde_json::Value::as_str) {
        None => FragmentBy::Word,
        Some(Some("word")) => FragmentBy::Word,
        Some(Some("sentence")) => FragmentBy::Sentence,
        Some(Some("paragraph")) => FragmentBy::Paragraph,
        Some(Some("none")) => FragmentBy::None,
        Some(_) => return Err("fragment_by is word, sentence, paragraph or none".into()),
    };
    let limit = match o.get("fragment_limit") {
        None => 1,
        Some(v) => v
            .as_u64()
            .and_then(|n| usize::try_from(n).ok())
            .filter(|n| *n <= MAX_FRAGMENTS)
            .ok_or(format!(
                "fragment_limit is a whole number from 0 to {MAX_FRAGMENTS}"
            ))?,
    };
    let offsets = match o.get("include_offsets").map(serde_json::Value::as_str) {
        None => None,
        Some(Some("utf-8")) => Some(Units::Utf8),
        Some(Some("utf-16")) => Some(Units::Utf16),
        Some(Some("codepoints")) => Some(Units::Codepoints),
        Some(_) => return Err("include_offsets is utf-8, utf-16 or codepoints".into()),
    };
    Ok(Some(Structure { by, limit, offsets }))
}

/// One token of the text: its bytes (marks that follow it included), its characters, and
/// whether its term is one the query asked for.
struct Span {
    start: usize,
    end: usize,
    c_start: usize,
    c_end: usize,
    marked: bool,
}

/// `text[from..to]`, or nothing for a range off a character boundary: every range here comes
/// from `tokens`, so it is never taken.
fn piece(text: &str, from: usize, to: usize) -> &str {
    text.get(from..to).unwrap_or_default()
}

impl Highlight {
    /// `text`, or the fragment of it holding the most marked tokens, with every token whose
    /// term is in `terms` wrapped in `pre` and `post`.
    pub(crate) fn render(
        &self,
        text: &str,
        analyzer: &Analyzer,
        terms: &HashSet<String>,
    ) -> String {
        let (spans, total) = spans(text, analyzer, terms);
        if total <= self.max_chars {
            return self.wrap(text, 0, text.len(), &spans);
        }
        let Some((i, j, first, last, window)) = self
            .window(&spans)
            .and_then(|(i, j)| Some((i, j, spans.get(i)?, spans.get(j)?, spans.get(i..=j)?)))
        else {
            // No token to build a fragment of: the first characters.
            let mut out: String = text.chars().take(self.max_chars).collect();
            out.push(ELLIPSIS);
            return out;
        };
        let mut out = String::new();
        if i > 0 {
            out.push(ELLIPSIS);
        }
        if first.c_end - first.c_start > self.max_chars {
            // A token longer than the fragment: cut, and followed by what it lost.
            let cut: String = piece(text, first.start, first.end)
                .chars()
                .take(self.max_chars)
                .collect();
            if first.marked {
                out.push_str(&self.pre);
                out.push_str(&cut);
                out.push_str(&self.post);
            } else {
                out.push_str(&cut);
            }
            out.push(ELLIPSIS);
            return out;
        }
        out.push_str(&self.wrap(text, first.start, last.end, window));
        if j + 1 < spans.len() {
            out.push(ELLIPSIS);
        }
        out
    }

    /// The token range `(i, j)` of the window with the most marked tokens: it starts at a
    /// token, spans from its first character to the last of token `j`, separators included,
    /// and holds at most `max_chars` characters, `j` the largest that fits. The earliest
    /// window among equals. A token longer than `max_chars` is a window of its own. `None`
    /// for a text with no token.
    fn window(&self, spans: &[Span]) -> Option<(usize, usize)> {
        best(windows(spans, self.max_chars)).map(|(i, j, _)| (i, j))
    }

    /// `text[from..to]` with the marked tokens of `spans` wrapped.
    fn wrap(&self, text: &str, from: usize, to: usize, spans: &[Span]) -> String {
        let mut out = String::with_capacity(to.saturating_sub(from) + 16);
        let mut cursor = from;
        for s in spans.iter().filter(|s| s.marked) {
            out.push_str(piece(text, cursor, s.start));
            out.push_str(&self.pre);
            out.push_str(piece(text, s.start, s.end));
            out.push_str(&self.post);
            cursor = s.end;
        }
        out.push_str(piece(text, cursor, to));
        out
    }
}

/// The tokens of `text` as spans, and its length in characters.
fn spans(text: &str, analyzer: &Analyzer, terms: &HashSet<String>) -> (Vec<Span>, usize) {
    let mut spans: Vec<Span> = Vec::new();
    let (mut at, mut chars) = (0usize, 0usize);
    for t in tokens(analyzer, text) {
        let end = extend_over_marks(text, t.span.end);
        chars += piece(text, at, t.span.start).chars().count();
        let c_start = chars;
        chars += piece(text, t.span.start, end).chars().count();
        at = end;
        spans.push(Span {
            start: t.span.start,
            end,
            c_start,
            c_end: chars,
            marked: t.term.as_ref().is_some_and(|x| terms.contains(x)),
        });
    }
    (spans, chars + piece(text, at, text.len()).chars().count())
}

/// Every window `(i, j, marked tokens)`, one per starting token `i`: it runs to the last
/// token `j` that keeps it within `max_chars` characters, and a token longer than that is a
/// window of its own.
///
/// One pass: a window's end only moves forward as its start does.
fn windows(spans: &[Span], max_chars: usize) -> Vec<(usize, usize, usize)> {
    let mut before = Vec::with_capacity(spans.len() + 1);
    before.push(0usize);
    for s in spans {
        before.push(before.last().copied().unwrap_or_default() + usize::from(s.marked));
    }
    let marked = |i: usize, j: usize| {
        before.get(j + 1).copied().unwrap_or_default() - before.get(i).copied().unwrap_or_default()
    };
    let mut out = Vec::with_capacity(spans.len());
    let mut hi = 0usize;
    for (i, s) in spans.iter().enumerate() {
        hi = hi.max(i);
        if s.c_end - s.c_start <= max_chars {
            while spans
                .get(hi + 1)
                .is_some_and(|n| n.c_end - s.c_start <= max_chars)
            {
                hi += 1;
            }
        }
        out.push((i, hi, marked(i, hi)));
    }
    out
}

/// The window with the most marked tokens, the earliest among equals.
fn best(windows: Vec<(usize, usize, usize)>) -> Option<(usize, usize, usize)> {
    windows.into_iter().fold(None, |b, w| {
        if b.is_none_or(|(.., n)| w.2 > n) {
            Some(w)
        } else {
            b
        }
    })
}

/// The units of `v` under `by`, as byte ranges, trimmed, empty ones dropped (M159).
fn units(v: &str, by: FragmentBy) -> Vec<std::ops::Range<usize>> {
    let mut cuts = Vec::new();
    if matches!(by, FragmentBy::Sentence | FragmentBy::Paragraph) {
        let mut chars = v.char_indices().peekable();
        while let Some((at, c)) = chars.next() {
            let after = at + c.len_utf8();
            if c.is_whitespace() {
                // A whitespace run: a paragraph break when it holds a line feed.
                let mut end = after;
                let mut feed = c == '\n';
                while let Some((a, n)) = chars.next_if(|(_, n)| n.is_whitespace()) {
                    feed |= n == '\n';
                    end = a + n.len_utf8();
                }
                if feed {
                    cuts.push((at, end));
                }
            } else if by == FragmentBy::Sentence
                && (matches!(c, '。' | '！' | '？')
                    || (matches!(c, '.' | '!' | '?')
                        && chars.peek().is_some_and(|(_, n)| n.is_whitespace())))
            {
                cuts.push((after, after));
            }
        }
    }
    let mut out = Vec::new();
    let mut from = 0usize;
    for (end, next) in cuts.into_iter().chain([(v.len(), v.len())]) {
        let unit = piece(v, from, end);
        let lead = unit.len() - unit.trim_start().len();
        let trail = unit.len() - unit.trim_end().len();
        if lead < unit.len() {
            out.push(from + lead..end - trail);
        }
        from = next;
    }
    out
}

/// The byte just past the first `n` characters of `text[from..to]`.
fn cut(text: &str, from: usize, to: usize, n: usize) -> usize {
    from + piece(text, from, to)
        .char_indices()
        .nth(n)
        .map_or(to - from, |(at, _)| at)
}

/// One fragment a value offers, before ranking.
struct Candidate {
    /// Its value's position among the strings read.
    element: usize,
    from: usize,
    to: usize,
    /// The marked byte ranges inside `from..to`.
    marks: Vec<(usize, usize)>,
}

impl Highlight {
    /// M159: the structured fragments of `value`, or `None` when it offers none: not a string
    /// or an array holding one, or nothing but whitespace.
    pub(crate) fn fragments(
        &self,
        value: &pstore_format::Value,
        analyzer: &Analyzer,
        terms: &HashSet<String>,
    ) -> Option<Vec<Fragment>> {
        use pstore_format::Value;
        let s = self.structure.as_ref()?;
        let texts: Vec<(Option<usize>, &str)> = match value {
            Value::Str(t) => vec![(None, t.as_str())],
            Value::Array(a) => a
                .iter()
                .enumerate()
                .filter_map(|(i, v)| match v {
                    Value::Str(t) => Some((Some(i), t.as_str())),
                    _ => None,
                })
                .collect(),
            _ => return None,
        };
        let mut all = Vec::new();
        for (element, (_, text)) in texts.iter().enumerate() {
            for unit in units(text, s.by) {
                self.candidates(text, unit, element, s.by, analyzer, terms, &mut all);
            }
        }
        if all.is_empty() {
            return None;
        }
        // Ranked by marks, then the earliest: a stable sort over the order they were found.
        all.sort_by(|a, b| b.marks.len().cmp(&a.marks.len()));
        let mut taken: Vec<&Candidate> = Vec::new();
        for c in all.iter().filter(|c| !c.marks.is_empty()) {
            if taken.len() == s.limit {
                break;
            }
            // Windows sharing a token share bytes; windows that do not, and units, never do.
            let overlaps =
                |t: &&Candidate| t.element == c.element && t.from < c.to && c.from < t.to;
            if !taken.iter().any(overlaps) {
                taken.push(c);
            }
        }
        if taken.is_empty() && s.limit > 0 {
            // Nothing marked: every candidate scores 0, so the first is the earliest.
            taken.extend(all.first());
        }
        Some(
            taken
                .into_iter()
                .map(|c| {
                    let (array_index, text) = texts.get(c.element).copied().unwrap_or((None, ""));
                    let at = |b: usize| match s.offsets {
                        Some(Units::Utf16) => piece(text, 0, b).encode_utf16().count(),
                        Some(Units::Codepoints) => piece(text, 0, b).chars().count(),
                        _ => b,
                    };
                    let mut out = String::new();
                    let mut cursor = c.from;
                    for (a, b) in &c.marks {
                        out.push_str(piece(text, cursor, *a));
                        out.push_str(&self.pre);
                        out.push_str(piece(text, *a, *b));
                        out.push_str(&self.post);
                        cursor = *b;
                    }
                    out.push_str(piece(text, cursor, c.to));
                    Fragment {
                        text: out,
                        range: s.offsets.map(|_| [at(c.from), at(c.to)]),
                        marks: s
                            .offsets
                            .map(|_| c.marks.iter().map(|(a, b)| [at(*a), at(*b)]).collect()),
                        array_index,
                    }
                })
                .collect(),
        )
    }

    /// The candidates of one unit of `text`, pushed in order of their start.
    #[allow(
        clippy::too_many_arguments,
        reason = "one call site, each argument distinct"
    )]
    fn candidates(
        &self,
        text: &str,
        unit: std::ops::Range<usize>,
        element: usize,
        by: FragmentBy,
        analyzer: &Analyzer,
        terms: &HashSet<String>,
        out: &mut Vec<Candidate>,
    ) {
        let body = piece(text, unit.start, unit.end);
        let (spans, total) = spans(body, analyzer, terms);
        let shift = |s: &Span| (unit.start + s.start, unit.start + s.end);
        // The marks of `spans` that fall inside `from..to`, clipped at `to`.
        let marks = |from: usize, to: usize| -> Vec<(usize, usize)> {
            spans
                .iter()
                .filter(|s| s.marked)
                .map(shift)
                .filter(|(a, _)| (from..to).contains(a))
                .map(|(a, b)| (a, b.min(to)))
                .collect()
        };
        // A unit with no token is cut like `none`'s: `cut` leaves it whole when it fits.
        if by == FragmentBy::None || spans.is_empty() {
            let to = cut(text, unit.start, unit.end, self.max_chars);
            out.push(Candidate {
                element,
                from: unit.start,
                to,
                marks: marks(unit.start, to),
            });
            return;
        }
        if total <= self.max_chars {
            out.push(Candidate {
                element,
                from: unit.start,
                to: unit.end,
                marks: marks(unit.start, unit.end),
            });
            return;
        }
        let all = windows(&spans, self.max_chars);
        let chosen = if by == FragmentBy::Word {
            all
        } else {
            best(all).into_iter().collect()
        };
        for (i, j, _) in chosen {
            let (Some(first), Some(last)) = (spans.get(i), spans.get(j)) else {
                continue;
            };
            let (from, end) = (shift(first).0, shift(last).1);
            // Only a token longer than `max_chars`, a window of its own, is cut: any other
            // window fits, and `cut` leaves it whole.
            let to = cut(text, from, end, self.max_chars);
            out.push(Candidate {
                element,
                from,
                to,
                marks: marks(from, to),
            });
        }
    }
}

impl From<Fragment> for crate::types::HighlightMatch {
    fn from(f: Fragment) -> Self {
        Self {
            text: f.text,
            fragment_range: f.range,
            match_ranges: f.marks,
            array_index: f.array_index,
        }
    }
}
