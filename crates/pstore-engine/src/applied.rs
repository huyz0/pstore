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

/// What a stamp ends with when its request asked for its ids (M130). An M129 fold strips
/// `$req` whatever its value, so it never seals one.
const IDS: &str = ":ids";

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

/// The distinct ids an applied request's operations changed (M130), each list sorted.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Affected {
    /// Ids an upsert wrote.
    pub upserted: Vec<String>,
    /// Ids a delete removed.
    pub deleted: Vec<String>,
    /// Ids a patch changed.
    pub patched: Vec<String>,
}

/// An applied request's answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Applied {
    /// The fold's decisions, summed over every commit that folded the request; and, when it
    /// asked for them (M130), the ids it changed, unioned over those commits.
    Known(Outcome, Option<Affected>),
    /// A commit in the request's window fell off HEAD's list or was reaped. The request
    /// was applied; what it did cannot be reported.
    Unavailable,
}

/// Where an applied request stood before its flush: the lower bound of its window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mark(pub(crate) u64);

tokio::task_local! {
    static APPLYING: (OperationId, bool);
}

/// Runs `f` with every operation it buffers stamped as `op`'s (M129).
pub async fn applying<F: std::future::Future>(op: OperationId, f: F) -> F::Output {
    APPLYING.scope((op, false), f).await
}

/// As [`applying`], and the fold also records the ids `op` changes (M130).
pub async fn applying_with_ids<F: std::future::Future>(op: OperationId, f: F) -> F::Output {
    APPLYING.scope((op, true), f).await
}

/// Which kind of change an operation made, for the tally.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Upserted,
    Deleted,
    Patched,
}

impl Kind {
    /// This kind's id counts, in [`Affected`]'s order.
    fn of(self, c: &mut IdCounts) -> &mut BTreeMap<String, u64> {
        let [u, d, p] = c;
        match self {
            Self::Upserted => u,
            Self::Deleted => d,
            Self::Patched => p,
        }
    }
}

/// Stamps `docs` when the current task is applying (M129): each carries the request's id,
/// and an unconditional upsert or delete becomes the deferred kind with no condition, so
/// the fold resolves it against the current version. That is what makes `deleted` mean a
/// version existed.
pub(crate) fn stamp(docs: &mut [Document], tombstone: impl Fn(&Document) -> bool) {
    let Ok((op, ids)) = APPLYING.try_with(|a| *a) else {
        return;
    };
    let id = Value::Str(if ids {
        format!("{}{IDS}", op.hex())
    } else {
        op.hex()
    });
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

/// Per kind, how many of this fold's changes touched each id (M130): a count, so a set-aside
/// in one index takes back only that index's changes.
type IdCounts = [BTreeMap<String, u64>; 3];

/// One commit's decisions: per request, its outcome, and, for those that asked, its ids.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct Decided {
    pub(crate) counts: BTreeMap<String, Outcome>,
    pub(crate) ids: BTreeMap<String, Affected>,
}

/// One fold attempt's tally: per request, its outcome; per index and id, the stamped
/// changes `resolve` made to it, so a row the width pass sets aside takes them back.
#[derive(Debug, Default)]
pub(crate) struct Tally {
    by_req: BTreeMap<String, Outcome>,
    /// M130: only for requests whose stamp asked.
    ids: BTreeMap<String, IdCounts>,
    touched: BTreeMap<String, Vec<(String, Kind)>>,
}

/// A stamp's request id, and whether it asked for ids.
fn parse(stamp: &str) -> (&str, bool) {
    match stamp.strip_suffix(IDS) {
        Some(hex) => (hex, true),
        None => (stamp, false),
    }
}

impl Tally {
    /// `stamp`'s outcome, with its id counts made to exist if it asked for them: a request
    /// that changed nothing still answers with empty lists.
    fn entry(&mut self, stamp: &str) -> &mut Outcome {
        let (req, ids) = parse(stamp);
        if ids {
            self.ids.entry(req.to_owned()).or_default();
        }
        self.by_req.entry(req.to_owned()).or_default()
    }

    /// An operation of `req` changed `id`'s row as `kind` does.
    pub(crate) fn changed(&mut self, req: Option<&str>, id: &str, kind: Kind) {
        let Some(stamp) = req else {
            return;
        };
        let o = self.entry(stamp);
        match kind {
            Kind::Upserted => o.upserted += 1,
            Kind::Deleted => o.deleted += 1,
            Kind::Patched => o.patched += 1,
        }
        let req = parse(stamp).0;
        if let Some(c) = self.ids.get_mut(req) {
            *kind.of(c).entry(id.to_owned()).or_default() += 1;
        }
        self.touched
            .entry(id.to_owned())
            .or_default()
            .push((req.to_owned(), kind));
    }

    /// An operation of `req` changed nothing.
    pub(crate) fn skipped(&mut self, req: Option<&str>) {
        if let Some(stamp) = req {
            self.entry(stamp).skipped += 1;
        }
    }

    /// `d` was discarded before it could change anything.
    pub(crate) fn rejected(&mut self, d: &Document) {
        if let Some(stamp) = req_of(d) {
            self.entry(stamp).rejected += 1;
        }
    }

    /// The row `id` resolved to was set aside, so its id is left untouched: every stamped
    /// change this fold made to it is undone, and counts as rejected.
    pub(crate) fn set_aside(&mut self, id: &str) {
        for (req, kind) in self.touched.remove(id).unwrap_or_default() {
            if let Some(c) = self.ids.get_mut(&req) {
                let m = kind.of(c);
                if let Some(n) = m.get_mut(id) {
                    *n -= 1;
                    if *n == 0 {
                        m.remove(id);
                    }
                }
            }
            let o = self.by_req.entry(req).or_default();
            match kind {
                Kind::Upserted => o.upserted -= 1,
                Kind::Deleted => o.deleted -= 1,
                Kind::Patched => o.patched -= 1,
            }
            o.rejected += 1;
        }
    }

    /// What this attempt decided, for its commit's outcome object; the tally is left empty.
    pub(crate) fn take(&mut self) -> Decided {
        let ids = std::mem::take(&mut self.ids)
            .into_iter()
            .map(|(req, [u, d, p])| {
                let keys = |m: BTreeMap<String, u64>| m.into_keys().collect();
                (
                    req,
                    Affected {
                        upserted: keys(u),
                        deleted: keys(d),
                        patched: keys(p),
                    },
                )
            })
            .collect();
        Decided {
            counts: std::mem::take(&mut self.by_req),
            ids,
        }
    }

    /// Forgets the ids `resolve` touched, between indexes: ids are per index.
    pub(crate) fn next_index(&mut self) {
        self.touched.clear();
    }
}

/// Encodes one commit's outcomes: M129's layout, then, only if a request asked for its ids,
/// a trailing ids section (M130) an M129 decoder never meets -- ids need every node at M130.
pub(crate) fn encode(d: &Decided) -> Vec<u8> {
    fn str(out: &mut Vec<u8>, s: &str) {
        out.extend_from_slice(&(s.len() as u32).to_le_bytes());
        out.extend_from_slice(s.as_bytes());
    }
    let mut out = MAGIC.to_vec();
    out.extend_from_slice(&(d.counts.len() as u32).to_le_bytes());
    for (req, o) in &d.counts {
        str(&mut out, req);
        for n in [o.upserted, o.deleted, o.patched, o.skipped, o.rejected] {
            out.extend_from_slice(&n.to_le_bytes());
        }
    }
    if !d.ids.is_empty() {
        out.extend_from_slice(&(d.ids.len() as u32).to_le_bytes());
        for (req, a) in &d.ids {
            str(&mut out, req);
            for l in [&a.upserted, &a.deleted, &a.patched] {
                out.extend_from_slice(&(l.len() as u32).to_le_bytes());
                for id in l {
                    str(&mut out, id);
                }
            }
        }
    }
    out
}

/// Decodes what [`encode`] wrote.
///
/// # Errors
/// A body that is not an outcome object.
pub(crate) fn decode(body: &[u8]) -> Result<Decided, EngineError> {
    let bad = || EngineError::Format("an outcome object that does not decode".to_owned());
    let mut c = Cursor(body.strip_prefix(MAGIC.as_slice()).ok_or_else(bad)?);
    let n = c.u32().ok_or_else(bad)?;
    let mut out = Decided::default();
    for _ in 0..n {
        let req = c.str().ok_or_else(bad)?;
        let mut word = || c.u64().ok_or_else(bad);
        let o = Outcome {
            upserted: word()?,
            deleted: word()?,
            patched: word()?,
            skipped: word()?,
            rejected: word()?,
        };
        out.counts.insert(req, o);
    }
    // M130: the end of the buffer is no ids section.
    if !c.0.is_empty() {
        let n = c.u32().ok_or_else(bad)?;
        for _ in 0..n {
            let req = c.str().ok_or_else(bad)?;
            let mut list = || -> Result<Vec<String>, EngineError> {
                let n = c.u32().ok_or_else(bad)?;
                (0..n).map(|_| c.str().ok_or_else(bad)).collect()
            };
            let a = Affected {
                upserted: list()?,
                deleted: list()?,
                patched: list()?,
            };
            out.ids.insert(req, a);
        }
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

    fn str(&mut self) -> Option<String> {
        let len = self.u32()? as usize;
        String::from_utf8(self.take(len)?.to_vec()).ok()
    }
}

/// Sums `req`'s counts over several commits' outcomes, and unions its ids if any commit
/// recorded them (M130).
pub(crate) fn sum<'a>(
    req: &str,
    commits: impl IntoIterator<Item = &'a Decided>,
) -> (Outcome, Option<Affected>) {
    let mut total = Outcome::default();
    let mut ids: Option<[std::collections::BTreeSet<String>; 3]> = None;
    for c in commits {
        if let Some(o) = c.counts.get(req) {
            total.add(o);
        }
        if let Some(a) = c.ids.get(req) {
            let sets = ids.get_or_insert_default();
            for (set, l) in sets.iter_mut().zip([&a.upserted, &a.deleted, &a.patched]) {
                set.extend(l.iter().cloned());
            }
        }
    }
    let ids = ids.map(|[u, d, p]| Affected {
        upserted: u.into_iter().collect(),
        deleted: d.into_iter().collect(),
        patched: p.into_iter().collect(),
    });
    (total, ids)
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::indexing_slicing,
        reason = "test assertions"
    )]
    use super::*;

    fn counts() -> BTreeMap<String, Outcome> {
        BTreeMap::from([(
            "ab".to_owned(),
            Outcome {
                upserted: 1,
                deleted: 2,
                patched: 3,
                skipped: 4,
                rejected: 5,
            },
        )])
    }

    #[test]
    fn an_outcome_object_round_trips_and_refuses_trailing_bytes() {
        let ids = BTreeMap::from([(
            "ab".to_owned(),
            Affected {
                upserted: vec!["x".to_owned(), "y".to_owned()],
                deleted: vec![],
                patched: vec!["x".to_owned()],
            },
        )]);
        for d in [
            Decided {
                counts: counts(),
                ids: BTreeMap::new(),
            },
            Decided {
                counts: counts(),
                ids,
            },
        ] {
            let body = encode(&d);
            assert_eq!(decode(&body).unwrap(), d);
            let mut longer = body.clone();
            longer.push(0);
            assert!(decode(&longer).is_err());
            assert!(decode(&body[..body.len() - 1]).is_err());
        }
    }

    /// M130: a set-aside takes back every kind's count and id, leaving another index's.
    #[test]
    fn a_set_aside_takes_back_every_kind() {
        let stamp = format!("{}{IDS}", OperationId(1).hex());
        let mut t = Tally::default();
        t.changed(Some(&stamp), "x", Kind::Upserted);
        t.next_index();
        // The same id upserted in a second index, then set aside there: the first stands.
        t.changed(Some(&stamp), "x", Kind::Upserted);
        t.changed(Some(&stamp), "x", Kind::Patched);
        t.changed(Some(&stamp), "x", Kind::Deleted);
        t.set_aside("x");
        let d = t.take();
        let o = d.counts[&OperationId(1).hex()];
        assert_eq!((o.upserted, o.patched, o.deleted, o.rejected), (1, 0, 0, 3));
        assert_eq!(
            d.ids[&OperationId(1).hex()],
            Affected {
                upserted: vec!["x".to_owned()],
                deleted: vec![],
                patched: vec![],
            }
        );
        // And a request that did not ask has no ids, whatever its stamp's id.
        let mut t = Tally::default();
        t.changed(Some(&OperationId(2).hex()), "y", Kind::Upserted);
        assert!(t.take().ids.is_empty());
    }

    /// Every truncation of an object with ids is refused, never read as a shorter answer.
    #[test]
    fn every_truncation_is_refused() {
        let d = Decided {
            counts: counts(),
            ids: BTreeMap::from([(
                "ab".to_owned(),
                Affected {
                    upserted: vec!["x".to_owned()],
                    deleted: vec!["y".to_owned()],
                    patched: vec!["z".to_owned()],
                },
            )]),
        };
        let body = encode(&d);
        // The end of the counts is a whole M129 object, and decodes as one.
        let m129 = 8 + 4 + 4 + 2 + 5 * 8;
        for cut in (0..body.len()).filter(|&c| c != m129) {
            assert!(decode(&body[..cut]).is_err(), "a cut at {cut} decoded");
        }
        assert!(decode(b"NOTOUTCM").is_err());
    }

    /// M130 criterion 3: an object an M129 node wrote, spelled out byte by byte.
    #[test]
    fn an_m129_outcome_object_decodes() {
        let mut body = b"PSTOREOC".to_vec();
        body.extend_from_slice(&1u32.to_le_bytes());
        body.extend_from_slice(&2u32.to_le_bytes());
        body.extend_from_slice(b"ab");
        for n in 1u64..=5 {
            body.extend_from_slice(&n.to_le_bytes());
        }
        let d = decode(&body).unwrap();
        assert_eq!(d.counts, counts());
        assert!(d.ids.is_empty());
        // And a commit with no id-asking request is written exactly so.
        let m129 = Decided {
            counts: counts(),
            ids: BTreeMap::new(),
        };
        assert_eq!(encode(&m129), body);
    }
}
