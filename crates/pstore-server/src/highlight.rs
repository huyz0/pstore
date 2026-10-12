//! A result's text with the query's terms marked (M63).
//!
//! ⚠️ **Marks exactly the tokens BM25 matched.** The tokens and their terms come from
//! `pstore_format::text::tokens`, the pipeline `analyze` is built on, under the analyzer the
//! query was answered with. It costs no request: the text is an attribute the row round
//! already returned.
//!
//! Characters are Unicode scalar values throughout. The text is **not escaped**: `pre` and
//! `post` are the client's, and so is keeping the output safe for where it is shown.

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

/// A parsed `highlight`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Highlight {
    /// The string attribute to mark.
    pub(crate) attr: String,
    pre: String,
    post: String,
    max_chars: usize,
}

/// `raw` as a highlight, or why it is not one.
///
/// # Errors
/// Every refusal of M63's rule 6, as a message for a `400`.
pub(crate) fn parse(raw: &serde_json::Value) -> Result<Highlight, String> {
    let o = raw
        .as_object()
        .ok_or("highlight is {\"attr\", \"pre\", \"post\", \"max_chars\"}")?;
    if let Some(k) = o
        .keys()
        .find(|k| !matches!(k.as_str(), "attr" | "pre" | "post" | "max_chars"))
    {
        return Err(format!(
            "{k:?} is not a highlight field; attr, pre, post and max_chars are"
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
    })
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
        let total = chars + piece(text, at, text.len()).chars().count();
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
    ///
    /// One pass: the window's end only moves forward, and its marks are counted as it moves.
    fn window(&self, spans: &[Span]) -> Option<(usize, usize)> {
        let mut best: Option<(usize, usize, usize)> = None;
        // The window is `i..hi`; `count` is its marked tokens; `prev` is the last start's mark.
        let (mut hi, mut count, mut prev) = (0usize, 0usize, 0usize);
        for (i, s) in spans.iter().enumerate() {
            if hi <= i {
                hi = i;
                count = 0;
            } else {
                count -= prev;
            }
            if s.c_end - s.c_start > self.max_chars {
                hi = i + 1;
                count = usize::from(s.marked);
            } else {
                while let Some(next) = spans
                    .get(hi)
                    .filter(|n| n.c_end - s.c_start <= self.max_chars)
                {
                    count += usize::from(next.marked);
                    hi += 1;
                }
            }
            prev = usize::from(s.marked);
            if best.is_none_or(|(b, ..)| count > b) {
                best = Some((count, i, hi.saturating_sub(1)));
            }
        }
        best.map(|(_, i, j)| (i, j))
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
