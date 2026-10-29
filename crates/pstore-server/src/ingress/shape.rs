//! M143: a public body's JSON shape, checked before any parser runs.
//!
//! One pass over the collected bytes, allocating nothing: how deep arrays and objects nest,
//! how many elements one array and members one object hold, and how many bytes lie between
//! one string's quotes. It is not a JSON validator: a body it passes still goes to the
//! parsers, which refuse what they refuse today.

/// The deepest nesting any configuration may admit: version 1's default, which a
/// configuration can only lower. The scan's per-level counts are a fixed array of it.
pub(crate) const DEPTH: usize = 128;

/// The four bounds a body's shape is held to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Shape {
    pub(crate) depth: usize,
    pub(crate) elements: usize,
    pub(crate) members: usize,
    pub(crate) string_bytes: usize,
}

/// The first bound a body passes, and the nesting depth it was found at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Breach {
    Depth,
    Elements { depth: usize },
    Members { depth: usize },
    String { depth: usize },
}

/// `Err` with the first bound `body` passes; `Ok` otherwise, malformed or not.
pub(crate) fn scan(body: &[u8], shape: &Shape) -> Result<(), Breach> {
    // Per open level: whether it is an object, and how many elements or members it holds.
    let mut levels = [(false, 0usize); DEPTH + 1];
    let mut depth = 0usize;
    // The next token at this level starts an element or a member (after `[`, `{` or `,`).
    let mut expecting = false;
    let mut string: Option<usize> = None;
    let mut escaped = false;
    for &b in body {
        if let Some(len) = string.as_mut() {
            match (escaped, b) {
                (true, _) => escaped = false,
                (false, b'\\') => escaped = true,
                (false, b'"') => {
                    string = None;
                    continue;
                }
                _ => {}
            }
            *len += 1;
            if *len > shape.string_bytes {
                return Err(Breach::String { depth });
            }
            continue;
        }
        if b.is_ascii_whitespace() {
            continue;
        }
        if matches!(b, b']' | b'}') {
            depth = depth.saturating_sub(1);
            expecting = false;
            continue;
        }
        if b == b',' {
            expecting = true;
            continue;
        }
        if expecting && let Some((object, count)) = levels.get_mut(depth).filter(|_| depth > 0) {
            expecting = false;
            *count += 1;
            let (bound, breach) = if *object {
                (shape.members, Breach::Members { depth })
            } else {
                (shape.elements, Breach::Elements { depth })
            };
            if *count > bound {
                return Err(breach);
            }
        }
        match b {
            b'"' => string = Some(0),
            b'[' | b'{' => {
                if depth >= shape.depth.min(DEPTH) {
                    return Err(Breach::Depth);
                }
                depth += 1;
                if let Some(level) = levels.get_mut(depth) {
                    *level = (b == b'{', 0);
                }
                expecting = true;
            }
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
