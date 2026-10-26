//! Applied write completion (M129): what a fold decided for each stamped request, and where
//! a waiter finds it. See `docs/research/11-design/mutation-completion.md`.

use crate::EngineError;
use pstore_format::{Document, Value};
use std::collections::BTreeMap;

/// The reserved attribute an applied request's operations carry its id in. `$`-names are
/// refused at the door, so no client can forge one; the fold strips it before sealing.
pub(crate) const REQ_ATTR: &str = "$req";

/// How many tallying commits HEAD lists (M129). A waiter whose window reaches past the
/// oldest is told its outcome is unavailable, never given invented counts.
pub const OUTCOMES_KEPT: usize = 32;

const MAGIC: &[u8; 8] = b"PSTOREOC";

/// An applied request's id: 128 random bits, drawn by the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct OperationId(pub u128);

impl OperationId {
    /// As the response and `$req` spell it: 32 lowercase hex digits.
    #[must_use]
    pub fn hex(self) -> String {
        format!("{:032x}", self.0)
    }
}

/// What a fold decided for one request's operations, in rows (M129).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Outcome {
    /// Rows an upsert wrote.
    pub upserted: u64,
    /// Rows a delete removed: a version existed.
    pub deleted: u64,
    /// Rows a patch changed.
    pub patched: u64,
    /// Operations that changed nothing: a refused condition, a patch of nothing or of no
    /// change, a delete of nothing, a filter that admitted no row.
    pub skipped: u64,
    /// Operations a reject, width, replica or drop pass discarded.
    pub rejected: u64,
}

impl Outcome {
    fn add(&mut self, o: &Self) {
        self.upserted += o.upserted;
        self.deleted += o.deleted;
        self.patched += o.patched;
        self.skipped += o.skipped;
        self.rejected += o.rejected;
    }
}

/// An applied request's answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Applied {
    /// The fold's decisions, summed over every commit that folded the request.
    Known(Outcome),
    /// A commit in the request's window fell off HEAD's list or was reaped. The request
    /// was applied; what it did cannot be reported.
    Unavailable,
}

/// Where an applied request stood before its flush: the lower bound of its window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mark(pub(crate) u64);

tokio::task_local! {
    static APPLYING: OperationId;
}

/// Runs `f` with every operation it buffers stamped as `op`'s (M129).
pub async fn applying<F: std::future::Future>(op: OperationId, f: F) -> F::Output {
    APPLYING.scope(op, f).await
}

/// Which kind of change an operation made, for the tally.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Upserted,
    Deleted,
    Patched,
}

/// Stamps `docs` when the current task is applying (M129): each carries the request's id,
/// and an unconditional upsert or delete becomes the deferred kind with no condition, so
/// the fold resolves it against the current version. That is what makes `deleted` mean a
/// version existed.
pub(crate) fn stamp(docs: &mut [Document], tombstone: impl Fn(&Document) -> bool) {
    let Ok(op) = APPLYING.try_with(|op| *op) else {
        return;
    };
    let id = Value::Str(op.hex());
    for d in docs {
        if !d.attrs.contains_key(crate::OP_ATTR) {
            let code = if tombstone(d) {
                crate::OP_COND_DELETE
            } else {
                crate::OP_COND_UPSERT
            };
            d.attrs.insert(crate::OP_ATTR.to_owned(), Value::Int(code));
        }
        d.attrs.insert(REQ_ATTR.to_owned(), id.clone());
    }
}

/// The request id an operation is stamped with, if any.
pub(crate) fn req_of(d: &Document) -> Option<&str> {
    match d.attrs.get(REQ_ATTR) {
        Some(Value::Str(s)) => Some(s.as_str()),
        _ => None,
    }
}

/// One fold attempt's tally: per request, its outcome; per index and id, the stamped
/// changes `resolve` made to it, so a row the width pass sets aside takes them back.
#[derive(Debug, Default)]
pub(crate) struct Tally {
    pub(crate) by_req: BTreeMap<String, Outcome>,
    touched: BTreeMap<String, Vec<(String, Kind)>>,
}

impl Tally {
    /// An operation of `req` changed `id`'s row as `kind` does.
    pub(crate) fn changed(&mut self, req: Option<&str>, id: &str, kind: Kind) {
        let Some(req) = req else {
            return;
        };
        let o = self.by_req.entry(req.to_owned()).or_default();
        match kind {
            Kind::Upserted => o.upserted += 1,
            Kind::Deleted => o.deleted += 1,
            Kind::Patched => o.patched += 1,
        }
        self.touched
            .entry(id.to_owned())
            .or_default()
            .push((req.to_owned(), kind));
    }

    /// An operation of `req` changed nothing.
    pub(crate) fn skipped(&mut self, req: Option<&str>) {
        if let Some(req) = req {
            self.by_req.entry(req.to_owned()).or_default().skipped += 1;
        }
    }

    /// `d` was discarded before it could change anything.
    pub(crate) fn rejected(&mut self, d: &Document) {
        if let Some(req) = req_of(d) {
            self.by_req.entry(req.to_owned()).or_default().rejected += 1;
        }
    }

    /// The row `id` resolved to was set aside, so its id is left untouched: every stamped
    /// change this fold made to it is undone, and counts as rejected.
    pub(crate) fn set_aside(&mut self, id: &str) {
        for (req, kind) in self.touched.remove(id).unwrap_or_default() {
            let o = self.by_req.entry(req).or_default();
            match kind {
                Kind::Upserted => o.upserted -= 1,
                Kind::Deleted => o.deleted -= 1,
                Kind::Patched => o.patched -= 1,
            }
            o.rejected += 1;
        }
    }

    /// Forgets the ids `resolve` touched, between indexes: ids are per index.
    pub(crate) fn next_index(&mut self) {
        self.touched.clear();
    }
}

/// Encodes one commit's outcomes.
pub(crate) fn encode(by_req: &BTreeMap<String, Outcome>) -> Vec<u8> {
    let mut out = MAGIC.to_vec();
    out.extend_from_slice(&(by_req.len() as u32).to_le_bytes());
    for (req, o) in by_req {
        out.extend_from_slice(&(req.len() as u32).to_le_bytes());
        out.extend_from_slice(req.as_bytes());
        for n in [o.upserted, o.deleted, o.patched, o.skipped, o.rejected] {
            out.extend_from_slice(&n.to_le_bytes());
        }
    }
    out
}

/// Decodes what [`encode`] wrote.
///
/// # Errors
/// A body that is not an outcome object.
pub(crate) fn decode(body: &[u8]) -> Result<BTreeMap<String, Outcome>, EngineError> {
    let bad = || EngineError::Format("an outcome object that does not decode".to_owned());
    let mut c = Cursor(body.strip_prefix(MAGIC.as_slice()).ok_or_else(bad)?);
    let n = c.u32().ok_or_else(bad)?;
    let mut out = BTreeMap::new();
    for _ in 0..n {
        let len = c.u32().ok_or_else(bad)? as usize;
        let req = String::from_utf8(c.take(len).ok_or_else(bad)?.to_vec()).map_err(|_| bad())?;
        let mut word = || c.u64().ok_or_else(bad);
        let o = Outcome {
            upserted: word()?,
            deleted: word()?,
            patched: word()?,
            skipped: word()?,
            rejected: word()?,
        };
        out.insert(req, o);
    }
    if !c.0.is_empty() {
        return Err(bad());
    }
    Ok(out)
}

/// The unread rest of an outcome object.
struct Cursor<'a>(&'a [u8]);

impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        if self.0.len() < n {
            return None;
        }
        let (head, rest) = self.0.split_at(n);
        self.0 = rest;
        Some(head)
    }

    fn u32(&mut self) -> Option<u32> {
        self.take(4)?.try_into().ok().map(u32::from_le_bytes)
    }

    fn u64(&mut self) -> Option<u64> {
        self.take(8)?.try_into().ok().map(u64::from_le_bytes)
    }
}

/// Sums `req`'s entries over several commits' outcomes.
pub(crate) fn sum<'a>(
    req: &str,
    commits: impl IntoIterator<Item = &'a BTreeMap<String, Outcome>>,
) -> Outcome {
    let mut total = Outcome::default();
    for c in commits {
        if let Some(o) = c.get(req) {
            total.add(o);
        }
    }
    total
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::indexing_slicing,
        reason = "test assertions"
    )]
    use super::*;

    #[test]
    fn an_outcome_object_round_trips_and_refuses_trailing_bytes() {
        let m = BTreeMap::from([(
            "ab".to_owned(),
            Outcome {
                upserted: 1,
                deleted: 2,
                patched: 3,
                skipped: 4,
                rejected: 5,
            },
        )]);
        let body = encode(&m);
        assert_eq!(decode(&body).unwrap(), m);
        let mut longer = body.clone();
        longer.push(0);
        assert!(decode(&longer).is_err());
        assert!(decode(&body[..body.len() - 1]).is_err());
    }
}
