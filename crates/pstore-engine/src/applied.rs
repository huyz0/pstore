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

/// What separates a stamp's request from its idempotency key (M132).
const KEY: char = ';';

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
    /// Operations a reject, width, replica or drop pass discarded, and by-filter operations
    /// past their row bound (M136).
    pub rejected: u64,
    /// An `allow_partial` by-filter operation left rows it would change (M137): send the
    /// write again to resume.
    pub rows_remaining: bool,
}

impl Outcome {
    fn add(&mut self, o: &Self) {
        self.upserted += o.upserted;
        self.deleted += o.deleted;
        self.patched += o.patched;
        self.skipped += o.skipped;
        self.rejected += o.rejected;
        self.rows_remaining |= o.rows_remaining;
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
    /// M132: another attempt of the request's key won it, and this one applied nothing. Its
    /// outcome, summed over the listed commits after the key's `e0`; `None` when HEAD no
    /// longer covers the key or an object was reaped.
    Replayed(Option<(Outcome, Option<Affected>)>),
    /// M132: the fold could not prove no other attempt of the key applied, and this one
    /// applied nothing.
    Expired,
    /// M132: part of the attempt was refused as expired, and part applied, or may have.
    Partial,
}

/// An idempotency key (M132): the epoch it was issued at, and 128 random bits. Spelled
/// `<e0>-<32 lowercase hex>`, `e0` in decimal without leading zeros.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct IdempotencyKey {
    e0: u64,
    random: u128,
}

impl IdempotencyKey {
    /// The key issued at `e0` with these random bits.
    #[must_use]
    pub const fn new(e0: u64, random: u128) -> Self {
        Self { e0, random }
    }

    /// The epoch the key was issued at.
    #[must_use]
    pub const fn e0(&self) -> u64 {
        self.e0
    }

    /// The key `s` spells, if it spells one exactly as [`std::fmt::Display`] writes it.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        let (e0, hex) = s.split_once('-')?;
        let digits = |t: &str, f: fn(&u8) -> bool| !t.is_empty() && t.as_bytes().iter().all(f);
        if !digits(e0, u8::is_ascii_digit) || (e0.len() > 1 && e0.starts_with('0')) {
            return None;
        }
        if hex.len() != 32 || !digits(hex, |b| b.is_ascii_digit() || (b'a'..=b'f').contains(b)) {
            return None;
        }
        Some(Self {
            e0: e0.parse().ok()?,
            random: u128::from_str_radix(hex, 16).ok()?,
        })
    }
}

impl std::fmt::Display for IdempotencyKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}-{:032x}", self.e0, self.random)
    }
}

/// What a commit decided for one attempt of a key (M132).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// The attempt holds the key: its operations proceed.
    Won,
    /// Another attempt, named, holds the key: its operations were discarded.
    Duplicate(String),
    /// No winner is listed and HEAD does not cover the key: its operations were discarded.
    Expired,
}

/// Where an applied request stood before its flush: the lower bound of its window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mark(pub(crate) u64);

tokio::task_local! {
    static APPLYING: (OperationId, bool, Option<IdempotencyKey>);
}

/// Runs `f` with every operation it buffers stamped as `op`'s (M129).
pub async fn applying<F: std::future::Future>(op: OperationId, f: F) -> F::Output {
    APPLYING.scope((op, false, None), f).await
}

/// As [`applying`], and the fold also records the ids `op` changes (M130).
pub async fn applying_with_ids<F: std::future::Future>(op: OperationId, f: F) -> F::Output {
    APPLYING.scope((op, true, None), f).await
}

/// As [`applying`] (or [`applying_with_ids`] when `ids`), for an attempt of `key` (M132): of
/// the attempts carrying one key, the fold applies at most one.
pub async fn keyed<F: std::future::Future>(
    op: OperationId,
    key: &IdempotencyKey,
    ids: bool,
    f: F,
) -> F::Output {
    APPLYING.scope((op, ids, Some(*key)), f).await
}

/// Runs `f` stamped as `op`'s when there is one, as [`keyed`] (with `ids`) does; else
/// unstamped.
pub async fn stamped<F: std::future::Future>(
    op: Option<OperationId>,
    ids: bool,
    key: Option<&IdempotencyKey>,
    f: F,
) -> F::Output {
    match op {
        Some(op) => APPLYING.scope((op, ids, key.copied()), f).await,
        None => f.await,
    }
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
    let Ok((op, ids, key)) = APPLYING.try_with(|a| *a) else {
        return;
    };
    let mut id = op.hex();
    if ids {
        id.push_str(IDS);
    }
    if let Some(k) = key {
        id.push(KEY);
        id.push_str(&k.to_string());
    }
    let id = Value::Str(id);
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

/// One commit's decisions: per request, its outcome, and, for those that asked, its ids; and
/// per keyed attempt it decided (M132), its key and the verdict.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct Decided {
    pub(crate) counts: BTreeMap<String, Outcome>,
    pub(crate) ids: BTreeMap<String, Affected>,
    pub(crate) keys: BTreeMap<String, (String, Verdict)>,
}

impl Decided {
    /// Per key, the attempt this commit names as its winner (M132): one it let win, or the
    /// one it refused a duplicate for.
    pub(crate) fn winners(&self) -> impl Iterator<Item = (&str, &str)> {
        self.keys.iter().filter_map(|(attempt, (key, v))| match v {
            Verdict::Won => Some((key.as_str(), attempt.as_str())),
            Verdict::Duplicate(w) => Some((key.as_str(), w.as_str())),
            Verdict::Expired => None,
        })
    }
}

/// One fold attempt's tally: per request, its outcome; per index and id, the stamped
/// changes `resolve` made to it, so a row the width pass sets aside takes them back.
#[derive(Debug, Default)]
pub(crate) struct Tally {
    by_req: BTreeMap<String, Outcome>,
    /// M130: only for requests whose stamp asked.
    ids: BTreeMap<String, IdCounts>,
    touched: BTreeMap<String, Vec<(String, Kind)>>,
    /// M132: the keyed attempts this fold attempt decided.
    keys: BTreeMap<String, (String, Verdict)>,
}

/// A stamp's request id, and whether it asked for ids.
fn parse(stamp: &str) -> (&str, bool) {
    let stamp = stamp.split_once(KEY).map_or(stamp, |(s, _)| s);
    match stamp.strip_suffix(IDS) {
        Some(hex) => (hex, true),
        None => (stamp, false),
    }
}

/// A stamp's request id and idempotency key, if it carries one (M132).
pub(crate) fn key_of(stamp: &str) -> Option<(&str, IdempotencyKey)> {
    let (_, key) = stamp.split_once(KEY)?;
    Some((parse(stamp).0, IdempotencyKey::parse(key)?))
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

    /// An `allow_partial` operation of `req` left rows it would change (M137).
    pub(crate) fn remaining(&mut self, req: Option<&str>) {
        if let Some(stamp) = req {
            self.entry(stamp).rows_remaining = true;
        }
    }

    /// `d` was discarded before it could change anything.
    pub(crate) fn rejected(&mut self, d: &Document) {
        if let Some(stamp) = req_of(d) {
            self.entry(stamp).rejected += 1;
        }
    }

    /// M132: what this attempt decided for `attempt` of `key`.
    pub(crate) fn decided(&mut self, attempt: &str, key: &IdempotencyKey, v: Verdict) {
        self.keys.insert(attempt.to_owned(), (key.to_string(), v));
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
            keys: std::mem::take(&mut self.keys),
        }
    }

    /// Forgets the ids `resolve` touched, between indexes: ids are per index.
    pub(crate) fn next_index(&mut self) {
        self.touched.clear();
    }
}

/// Notes each keyed attempt among `docs` not yet in `keyed`, in order (M132).
pub(crate) fn note_keys(docs: &[Document], keyed: &mut Vec<(String, IdempotencyKey)>) {
    for (a, k) in docs.iter().filter_map(|d| req_of(d).and_then(key_of)) {
        if !keyed.iter().any(|(seen, _)| seen == a) {
            keyed.push((a.to_owned(), k));
        }
    }
}

/// Decides every key a fold attempt met, in its order, against the outcome objects HEAD lists
/// and HEAD's horizon (`mutation-completion.md` §9.3), records the verdicts in `tally`, and
/// discards from `by_index` every operation of an attempt that did not win, counting it
/// `rejected`. Whether any was discarded.
pub(crate) fn decide(
    keyed: Vec<(String, IdempotencyKey)>,
    listed: &[std::sync::Arc<Decided>],
    horizon: u64,
    tally: &mut Tally,
    by_index: &mut BTreeMap<String, Vec<Document>>,
) -> bool {
    let mut winners: BTreeMap<String, String> = BTreeMap::new();
    for d in listed {
        for (k, w) in d.winners() {
            winners.insert(k.to_owned(), w.to_owned());
        }
    }
    let mut verdicts: BTreeMap<String, Verdict> = BTreeMap::new();
    for (a, k) in keyed {
        let v = match winners.get(&k.to_string()) {
            Some(w) if *w == a => Verdict::Won,
            Some(w) => Verdict::Duplicate(w.clone()),
            None if k.e0() >= horizon => {
                winners.insert(k.to_string(), a.clone());
                Verdict::Won
            }
            None => Verdict::Expired,
        };
        tally.decided(&a, &k, v.clone());
        verdicts.insert(a, v);
    }
    let mut discarded = false;
    for docs in by_index.values_mut() {
        docs.retain(|d| {
            let keep = req_of(d)
                .and_then(key_of)
                .is_none_or(|(a, _)| verdicts.get(a) == Some(&Verdict::Won));
            if !keep {
                tally.rejected(d);
                discarded = true;
            }
            keep
        });
    }
    by_index.retain(|_, docs| !docs.is_empty());
    discarded
}

/// What a keyed waiter's own records say (M132): its answer, or that it must sum the named
/// winner.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Own {
    Answer(Applied),
    Replay(String),
}

/// A keyed waiter's answer from its own window (`mutation-completion.md` §9.4): `own` are the
/// listed commits after its mark, `None` where one was reaped, and `fallen` whether the window
/// reaches past the list. ⚠️ Read even when the window fell (round-2 spec review): the
/// refusing commit is the newest, so it is still listed. `None` when the window is lost and
/// holds none of its records (M134): see [`named`].
pub(crate) fn own_answer(
    me: &str,
    fallen: bool,
    own: Vec<Option<std::sync::Arc<Decided>>>,
) -> Option<Own> {
    let lost = fallen || own.iter().any(Option::is_none);
    let own: Vec<std::sync::Arc<Decided>> = own.into_iter().flatten().collect();
    let verdicts: Vec<&Verdict> = own
        .iter()
        .filter_map(|c| c.keys.get(me).map(|(_, v)| v))
        .collect();
    if verdicts.contains(&&Verdict::Expired) {
        let (o, _) = sum(me, own.iter().map(|c| &**c));
        let applied = o.upserted + o.deleted + o.patched + o.skipped;
        return Some(Own::Answer(if lost || applied > 0 {
            Applied::Partial
        } else {
            Applied::Expired
        }));
    }
    if let Some(Verdict::Duplicate(w)) =
        verdicts.iter().find(|v| matches!(v, Verdict::Duplicate(_)))
    {
        return Some(Own::Replay(w.clone()));
    }
    if lost {
        // ⚠️ M134: `Unavailable` says "applied". Without one of its own records, that is
        // known only if a listed record names this attempt the winner.
        return (!verdicts.is_empty()).then_some(Own::Answer(Applied::Unavailable));
    }
    let (outcome, ids) = sum(me, own.iter().map(|c| &**c));
    Some(Own::Answer(Applied::Known(outcome, ids)))
}

/// A keyed waiter's answer when none of its own records are listed (M134): `listed` are all
/// of HEAD's outcome objects. The attempt a record names as `key`'s winner tells: this one
/// (`Unavailable`: applied, not readable), another (replayed), or none (`Partial`: was, or
/// may have been, applied).
pub(crate) fn named(me: &str, key: &str, listed: &[Option<std::sync::Arc<Decided>>]) -> Own {
    let winner = listed
        .iter()
        .flatten()
        .find_map(|d| d.winners().find(|(k, _)| *k == key).map(|(_, w)| w));
    match winner {
        Some(w) if w == me => Own::Answer(Applied::Unavailable),
        Some(w) => Own::Replay(w.to_owned()),
        None => Own::Answer(Applied::Partial),
    }
}

/// The winner `w`'s outcome, summed over `theirs` (the listed commits after the key's `e0`),
/// when HEAD covers the key and none was reaped (M132).
pub(crate) fn replay(
    w: &str,
    covered: bool,
    theirs: Vec<Option<std::sync::Arc<Decided>>>,
) -> Applied {
    Applied::Replayed(
        theirs
            .into_iter()
            .collect::<Option<Vec<_>>>()
            .filter(|_| covered)
            .map(|c| sum(w, c.iter().map(|c| &**c))),
    )
}

/// Encodes one commit's outcomes: M129's layout, then, only if a request asked for its ids,
/// a trailing ids section (M130) an M129 decoder never meets -- ids need every node at M130.
/// Then, only if the commit decided a key, a keys section (M132), which forces the ids
/// section, empty if need be; an M130 decoder refuses it, so keys need every node at M132.
/// Then, only if a request has rows remaining, a remaining section (M137), which forces the
/// ids and keys sections; an M136 decoder refuses it, so it needs every node at M137.
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
    let remaining: Vec<&String> = d
        .counts
        .iter()
        .filter(|(_, o)| o.rows_remaining)
        .map(|(req, _)| req)
        .collect();
    if !d.ids.is_empty() || !d.keys.is_empty() || !remaining.is_empty() {
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
    if !d.keys.is_empty() || !remaining.is_empty() {
        out.extend_from_slice(&(d.keys.len() as u32).to_le_bytes());
        for (attempt, (key, v)) in &d.keys {
            str(&mut out, attempt);
            str(&mut out, key);
            match v {
                Verdict::Won => out.push(0),
                Verdict::Duplicate(w) => {
                    out.push(1);
                    str(&mut out, w);
                }
                Verdict::Expired => out.push(2),
            }
        }
    }
    if !remaining.is_empty() {
        out.extend_from_slice(&(remaining.len() as u32).to_le_bytes());
        for req in remaining {
            str(&mut out, req);
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
            rows_remaining: false,
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
    // M132: and the end of the buffer here is no keys section.
    if !c.0.is_empty() {
        let n = c.u32().ok_or_else(bad)?;
        for _ in 0..n {
            let attempt = c.str().ok_or_else(bad)?;
            let key = c.str().ok_or_else(bad)?;
            let v = match c.take(1).ok_or_else(bad)? {
                [0] => Verdict::Won,
                [1] => Verdict::Duplicate(c.str().ok_or_else(bad)?),
                [2] => Verdict::Expired,
                _ => return Err(bad()),
            };
            out.keys.insert(attempt, (key, v));
        }
    }
    // M137: and here, no request has rows remaining. One that does has counts.
    if !c.0.is_empty() {
        let n = c.u32().ok_or_else(bad)?;
        for _ in 0..n {
            let req = c.str().ok_or_else(bad)?;
            out.counts.get_mut(&req).ok_or_else(bad)?.rows_remaining = true;
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
                rows_remaining: false,
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
                ..Decided::default()
            },
            Decided {
                counts: counts(),
                ids,
                ..Decided::default()
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
            keys: BTreeMap::new(),
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

    /// M132 criterion 10: an object with keys round-trips; it is M130's bytes and then the
    /// keys section; and every cut is refused except at a section boundary, which decodes as
    /// the older format with nothing claimed about keys.
    #[test]
    fn keys_round_trip_after_the_m130_bytes() {
        let keys = BTreeMap::from([
            ("a1".to_owned(), ("1-ab".to_owned(), Verdict::Won)),
            (
                "a2".to_owned(),
                ("1-ab".to_owned(), Verdict::Duplicate("a1".to_owned())),
            ),
            ("a3".to_owned(), ("2-cd".to_owned(), Verdict::Expired)),
        ]);
        for ids in [
            BTreeMap::new(),
            BTreeMap::from([(
                "a1".to_owned(),
                Affected {
                    upserted: vec!["x".to_owned()],
                    deleted: vec![],
                    patched: vec![],
                },
            )]),
        ] {
            let m130 = Decided {
                counts: counts(),
                ids: ids.clone(),
                ..Decided::default()
            };
            let d = Decided {
                keys: keys.clone(),
                ..m130.clone()
            };
            let body = encode(&d);
            assert_eq!(decode(&body).unwrap(), d);
            // The ids section is forced, empty if need be; the rest is M130's.
            let mut old = encode(&m130);
            if ids.is_empty() {
                old.extend_from_slice(&0u32.to_le_bytes());
            }
            assert!(body.starts_with(&old));
            let m129 = 8 + 4 + 4 + 2 + 5 * 8;
            for cut in (0..body.len()).filter(|&c| c != m129 && c != old.len()) {
                assert!(decode(&body[..cut]).is_err(), "a cut at {cut} decoded");
            }
            assert!(decode(&body[..old.len()]).unwrap().keys.is_empty());
            let mut bad = body.clone();
            // The last verdict's byte (`a3`, expired) as an unknown verdict.
            *bad.last_mut().unwrap() = 3;
            assert!(decode(&bad).is_err());
        }
    }

    /// M137: the remaining section forces empty ids and keys sections, and names only a
    /// request with counts.
    #[test]
    fn rows_remaining_round_trips_after_the_m132_bytes() {
        let mut d = Decided {
            counts: counts(),
            ..Decided::default()
        };
        let before = encode(&d);
        d.counts.values_mut().for_each(|o| o.rows_remaining = true);
        let body = encode(&d);
        assert_eq!(decode(&body).unwrap(), d);
        // M129's bytes, then two empty sections, then one request named.
        let mut want = before.clone();
        want.extend_from_slice(&0u32.to_le_bytes());
        want.extend_from_slice(&0u32.to_le_bytes());
        want.extend_from_slice(&1u32.to_le_bytes());
        want.extend_from_slice(&2u32.to_le_bytes());
        want.extend_from_slice(b"ab");
        assert_eq!(body, want);
        assert!(
            decode(&before)
                .unwrap()
                .counts
                .values()
                .all(|o| !o.rows_remaining)
        );
        // A request with no counts cannot have rows remaining.
        let mut bad = body[..body.len() - 2].to_vec();
        bad.extend_from_slice(b"zz");
        assert!(decode(&bad).is_err());
    }

    #[test]
    fn a_stamp_carries_its_key() {
        let k = IdempotencyKey::new(7, 0xab);
        let stamp = format!("{}{IDS}{KEY}{k}", OperationId(1).hex());
        assert_eq!(key_of(&stamp), Some((OperationId(1).hex().as_str(), k)));
        assert_eq!(parse(&stamp), (OperationId(1).hex().as_str(), true));
        let plain = format!("{}{KEY}{k}", OperationId(1).hex());
        assert_eq!(parse(&plain), (OperationId(1).hex().as_str(), false));
        assert_eq!(key_of(&OperationId(1).hex()), None);
        assert_eq!(key_of(&format!("{}{KEY}junk", OperationId(1).hex())), None);
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
            ..Decided::default()
        };
        assert_eq!(encode(&m129), body);
    }
}
