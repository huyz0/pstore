//! The root, the per-bucket pointer, and the fold that turns pending records into a run.

use crate::keys::{self, Width};
use crate::record::{Cur, TenantRecord, decode_records, encode_records};
use crate::{CatalogError, MAX_CAS_ATTEMPTS};
use pstore_blob::{BlobError, BlobStore, CasError, Precondition};
use pstore_types::{CasTag, Epoch};
use std::collections::BTreeMap;

/// The deployment's shape.
///
/// The only thing in the system that knows how many buckets there are, which is why every
/// reader takes `width` as a parameter rather than reading a constant: on a deployment that
/// has been widened, the constant names the wrong buckets and finds nothing there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Root {
    /// Advances on a width change. Nothing else moves it.
    pub epoch: Epoch,
    /// How many buckets.
    pub width: Width,
}

/// A bucket's pointer: the run it names, and the changes not yet folded into that run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BucketHead {
    /// The run's epoch. [`Epoch::ZERO`] means the bucket has never been folded.
    pub run_epoch: Epoch,
    /// The run's content digest, which is half of its key.
    pub digest: u64,
    /// Recorded but not yet in a run. Bounded by [`crate::MAX_PENDING`].
    pub pending: Vec<TenantRecord>,
    /// Runs this bucket has superseded, **newest first**, bounded by
    /// [`crate::MAX_GRAVEYARD`].
    ///
    /// ⚠️ **A run's key is derived, not remembered.** `run_key(bucket, run_epoch, digest)`
    /// needs a content digest, and this head carries only the digest of the run it *names* —
    /// so an old run's key cannot be computed from anything that survives, and garbage nobody
    /// can name is garbage forever. This is the record that makes reaping possible at all.
    ///
    /// ⚠️ Encoded **after** `pending`, so a head written before this field existed decodes
    /// with an empty graveyard rather than failing.
    pub graveyard: Vec<(Epoch, u64)>,
}

impl BucketHead {
    /// The key of the run this head names, or `None` before the first fold.
    #[must_use]
    pub fn run(&self, bucket: u32) -> Option<pstore_blob::Key> {
        (self.run_epoch != Epoch::ZERO).then(|| keys::run_key(bucket, self.run_epoch, self.digest))
    }

    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&self.run_epoch.0.to_le_bytes());
        out.extend_from_slice(&self.digest.to_le_bytes());
        out.extend_from_slice(&encode_records(&self.pending));
        #[expect(
            clippy::cast_possible_truncation,
            reason = "bounded by MAX_GRAVEYARD, which is 8"
        )]
        out.extend_from_slice(&(self.graveyard.len() as u32).to_le_bytes());
        for (epoch, digest) in &self.graveyard {
            out.extend_from_slice(&epoch.0.to_le_bytes());
            out.extend_from_slice(&digest.to_le_bytes());
        }
        out
    }

    /// The encoding, for a test that has to build a head this version would not write.
    ///
    /// Forward compatibility that cannot be constructed cannot be tested, and heads written
    /// before the graveyard existed outlive every reader that meets them.
    #[doc(hidden)]
    #[must_use]
    pub fn encode_for_test(&self) -> Vec<u8> {
        self.encode()
    }

    /// Records `(epoch, digest)` as superseded, newest first, deduplicated and bounded.
    ///
    /// ⚠️ **Deduplicated because `publish` is retried.** Two attempts against the same head
    /// each supersede the same run, and a duplicate spends a slot inside a bound whose whole
    /// job is to keep this object small.
    fn bury(&mut self, epoch: Epoch, digest: u64) {
        if epoch == Epoch::ZERO || self.graveyard.contains(&(epoch, digest)) {
            return;
        }
        self.graveyard.insert(0, (epoch, digest));
        self.graveyard.truncate(crate::MAX_GRAVEYARD);
    }

    pub(crate) fn decode(buf: &[u8], what: &str) -> Result<Self, CatalogError> {
        let mut c = Cur { b: buf, i: 0, what };
        let run_epoch = Epoch(c.u64()?);
        let digest = c.u64()?;
        let pending = decode_records(&mut c)?;
        // ⚠️ Absent, not empty, for a head written before the field existed — and absence
        // means "nothing recorded as superseded", which is the safe reading: a reap finds
        // nothing rather than deleting something it cannot name.
        let mut graveyard = Vec::new();
        if c.i < buf.len() {
            let n = c.u32()?;
            for _ in 0..n {
                graveyard.push((Epoch(c.u64()?), c.u64()?));
            }
        }
        Ok(Self {
            run_epoch,
            digest,
            pending,
            graveyard,
        })
    }
}

fn encode_root(r: Root) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&r.epoch.0.to_le_bytes());
    out.extend_from_slice(&r.width.get().to_le_bytes());
    out
}

/// Reads the deployment shape, and the tag its next write must be conditioned on.
///
/// ⚠️ **The tag is why this returns a pair.** It did not, and `write_root`'s `Some(previous)`
/// arm was therefore unreachable through the public API: a caller could only ever pass `None`,
/// which is create-if-absent and fails the moment the root exists. So the width could be set
/// once and **never changed** — which is a concrete blocker under OQ-8's protocol question,
/// one layer below it. Found by the region floor, which is what a coverage floor is for.
///
///
/// ⚠️ **Absent means "never widened", not "broken".** A deployment that has only ever run at
/// the default width has never had a reason to write this object, so the common case is a
/// 404 — and it still costs the request, which is why a cold enumeration is one round deeper
/// than a warm one.
pub async fn read_root<S: BlobStore>(store: &S) -> Result<(Root, Option<CasTag>), CatalogError> {
    match store.get_with_tag(&keys::root_key()).await {
        Ok((bytes, tag)) => {
            let mut c = Cur {
                b: &bytes,
                i: 0,
                what: "cat/root",
            };
            let epoch = Epoch(c.u64()?);
            let raw = c.u32()?;
            let width = Width::new(raw).ok_or(CatalogError::BadWidth(raw))?;
            Ok((Root { epoch, width }, Some(tag)))
        }
        // ⚠️ No tag, and that is the create-if-absent case rather than an error: a deployment
        // at the default width has never had a reason to write this object.
        Err(BlobError::NotFound(_)) => Ok((
            Root {
                epoch: Epoch::ZERO,
                width: Width::default(),
            },
            None,
        )),
        Err(e) => Err(e.into()),
    }
}

/// Writes the deployment shape, conditioned on the version the caller observed.
///
/// ⚠️ Nothing in the serving path calls this: the split that would move `width` is unbuilt, so
/// a deployment writes this object once at most. It is public because a reader that is *told*
/// the width proves nothing about a reader that reads it, and
/// `a_reader_takes_the_width_from_the_root` needs a root to actually be there. **CAS'd rather
/// than a plain `put`**, because a width change reassigns every tenant and two writers
/// disagreeing about the new width is the one way this object can be catastrophically wrong.
///
/// ⚠️ The `Some(previous)` arm is **unexercised**: no function here hands back the root's tag,
/// so reaching it means going around this crate to `get_with_tag(&root_key())`. It is the
/// shape the split will need and it is not yet a path anything walks.
///
/// # Errors
/// If the store refuses, or another writer won.
pub async fn write_root<S: BlobStore>(
    store: &S,
    root: Root,
    previous: Option<CasTag>,
) -> Result<(), CatalogError> {
    crate::require_fencing(store)?;
    match store
        .put_conditional(
            &keys::root_key(),
            encode_root(root).into(),
            match previous {
                Some(t) => Precondition::Match(t),
                None => Precondition::NotExists,
            },
        )
        .await
    {
        Ok(_) => Ok(()),
        Err(CasError::Lost | CasError::Contended) => Err(CatalogError::RootContended),
        Err(CasError::Io(e)) => Err(BlobError::Other(e).into()),
    }
}

/// Reads a bucket's pointer and the tag its next write must be conditioned on.
pub async fn read_head<S: BlobStore>(
    store: &S,
    bucket: u32,
) -> Result<(BucketHead, Option<CasTag>), CatalogError> {
    let key = keys::head_key(bucket);
    match store.get_with_tag(&key).await {
        Ok((bytes, tag)) => Ok((BucketHead::decode(&bytes, key.as_str())?, Some(tag))),
        Err(BlobError::NotFound(_)) => Ok((BucketHead::default(), None)),
        Err(e) => Err(e.into()),
    }
}

/// Reads the records a head's run holds. Empty before the first fold.
///
/// ⚠️ A run named by a committed pointer that is **not there** is an error, never an empty
/// bucket. Derived head keys 404 by design and that branch has to swallow absence; letting
/// the same branch swallow a missing *run* is how a catalog silently drops a bucket's worth
/// of tenants.
pub(crate) async fn read_run<S: BlobStore>(
    store: &S,
    bucket: u32,
    head: &BucketHead,
) -> Result<Vec<TenantRecord>, CatalogError> {
    let Some(key) = head.run(bucket) else {
        return Ok(Vec::new());
    };
    match store.get(&key).await {
        Ok(bytes) => {
            let mut c = Cur {
                b: &bytes,
                i: 0,
                what: key.as_str(),
            };
            decode_records(&mut c)
        }
        Err(BlobError::NotFound(_)) => Err(CatalogError::MissingRun(bucket)),
        Err(e) => Err(e.into()),
    }
}

/// Whether a conditional write landed, or needs the caller to rebase and try again.
///
/// ⚠️ A separate type rather than `CasError`, because the two failures are not alike: a lost
/// CAS is a normal outcome of a leaderless design and the caller retries; a malformed run is
/// not, and folding on top of one would bake the loss in permanently.
#[derive(Debug)]
pub(crate) enum Publish {
    /// The head now names what the caller wrote.
    Landed,
    /// Someone else won. Re-read and try again.
    Rebase,
}

/// Folds `extra` into the run `head` names and publishes the result.
///
/// The run is written **before** the CAS, and the CAS is what makes it visible: a run nobody
/// points at is garbage, while a pointer to a run that is not there is a lost bucket.
pub(crate) async fn publish<S: BlobStore>(
    store: &S,
    bucket: u32,
    head: &BucketHead,
    tag: CasTag,
    extra: &[TenantRecord],
) -> Result<Publish, CatalogError> {
    let current = read_run(store, bucket, head).await?;
    let merged = merge(current, head.pending.iter().chain(extra));
    let body = encode_records(&merged);
    let mut next = BucketHead {
        run_epoch: head.run_epoch.next(),
        digest: keys::digest(&body),
        pending: Vec::new(),
        graveyard: head.graveyard.clone(),
    };
    // ⚠️ Recorded BEFORE the new head is written, and it is the only chance: once this head
    // lands, the run it superseded has no key anyone can derive.
    next.bury(head.run_epoch, head.digest);
    let run = keys::run_key(bucket, next.run_epoch, next.digest);
    // Conditional on absence: with the digest in the key, an object already there has the
    // content we were about to write, so losing this race is success.
    match store
        .put_conditional(&run, body.into(), Precondition::NotExists)
        .await
    {
        // Losing this race is success: the digest is in the key, so an object already there
        // holds exactly the bytes we were about to write.
        Ok(_) | Err(CasError::Lost) => {}
        Err(CasError::Contended) => return Ok(Publish::Rebase),
        Err(CasError::Io(e)) => return Err(BlobError::Other(e).into()),
    }
    match store
        .put_conditional(
            &keys::head_key(bucket),
            next.encode().into(),
            Precondition::Match(tag),
        )
        .await
    {
        Ok(_) => Ok(Publish::Landed),
        Err(CasError::Lost | CasError::Contended) => Ok(Publish::Rebase),
        Err(CasError::Io(e)) => Err(BlobError::Other(e).into()),
    }
}

/// Merges records by tenant, newest `epoch` winning.
///
/// ⚠️ **The only place records combine**, deliberately: the run merges against pending here,
/// and `Appender::record` merges a new observation into pending with the same call. A second
/// rule written out at the second call site is a second rule to get wrong, and it was — an
/// earlier `retain`-by-tenant in the appender let a stale writer overwrite a newer *pending*
/// record and resurrect a pending tombstone.
///
/// ⚠️ **`>=`, not `>`.** Later observations win at an equal epoch, so re-recording a tenant
/// is idempotent rather than a no-op — which matters because an appender that restarts
/// re-observes what it already recorded.
///
/// Tombstones stay. They are filtered when a caller reads, not when a folder writes; see
/// [`crate::State::Deleted`].
pub(crate) fn merge<'a, I>(current: Vec<TenantRecord>, incoming: I) -> Vec<TenantRecord>
where
    I: IntoIterator<Item = &'a TenantRecord>,
{
    let mut by_tenant: BTreeMap<_, TenantRecord> =
        current.into_iter().map(|r| (r.tenant, r)).collect();
    for r in incoming {
        match by_tenant.get(&r.tenant) {
            Some(have) if have.epoch > r.epoch => {}
            _ => {
                by_tenant.insert(r.tenant, r.clone());
            }
        }
    }
    by_tenant.into_values().collect()
}

pub(crate) async fn write_head<S: BlobStore>(
    store: &S,
    bucket: u32,
    head: &crate::BucketHead,
    tag: Option<pstore_types::CasTag>,
) -> Result<Publish, CatalogError> {
    match store
        .put_conditional(
            &crate::keys::head_key(bucket),
            head.encode().into(),
            match tag {
                Some(t) => pstore_blob::Precondition::Match(t),
                None => pstore_blob::Precondition::NotExists,
            },
        )
        .await
    {
        Ok(_) => Ok(Publish::Landed),
        Err(CasError::Lost | CasError::Contended) => Ok(Publish::Rebase),
        Err(CasError::Io(e)) => Err(pstore_blob::BlobError::Other(e).into()),
    }
}

/// Deletes the runs this bucket superseded, keeping the newest `retention` of them.
///
/// ⚠️ **`retention` is a count of kept superseded runs, not epoch arithmetic.** The graveyard
/// is newest-first, so "keep the newest `retention`" needs no comparison — and the arithmetic
/// form invites an off-by-one whose failure mode is reaping a run a reader is on.
///
/// ⚠️ **Refused above [`crate::MAX_GRAVEYARD`].** A window of 20 against a record of 8 evicts
/// runs 9 through 20 from the graveyard *while they are still inside the window they were
/// promised*, turning them into permanently unreachable garbage. A promise larger than the
/// record is refused rather than silently broken.
///
/// ⚠️ **Deletes before it commits**, which is the mirror of `Engine::gc`'s ordering argument.
/// Deleting and then losing the head CAS leaves entries naming absent objects, and the next
/// reap re-deletes them harmlessly — `delete_batch` on a missing key is not an error.
/// CAS-then-delete leaves objects with **no record**, and their keys cannot be derived from
/// anything that survives.
///
/// Returns how many objects it reaped.
pub async fn reap<S: BlobStore>(
    store: &S,
    bucket: u32,
    retention: usize,
) -> Result<usize, CatalogError> {
    crate::require_fencing(store)?;
    if retention > crate::MAX_GRAVEYARD {
        return Err(CatalogError::Corrupt(format!(
            "a retention of {retention} superseded runs is wider than the graveyard's \
             {} entries, so the oldest would be evicted while still inside its window",
            crate::MAX_GRAVEYARD
        )));
    }
    for _ in 0..MAX_CAS_ATTEMPTS {
        let (head, tag) = read_head(store, bucket).await?;
        let Some(tag) = tag else { return Ok(0) };
        if head.graveyard.len() <= retention {
            return Ok(0);
        }
        let (keep, doomed) = head.graveyard.split_at(retention);
        let keys: Vec<pstore_blob::Key> = doomed
            .iter()
            .map(|(epoch, digest)| keys::run_key(bucket, *epoch, *digest))
            .collect();
        let n = keys.len();
        // Chunked at the backend's cap, the way `Engine::gc` learned to: a batch wider than
        // the profile allows is refused by the backend, not silently truncated.
        let cap = store.capabilities().max_batch_delete.max(1);
        for chunk in keys.chunks(cap) {
            store.delete_batch(chunk).await?;
        }
        let next = BucketHead {
            graveyard: keep.to_vec(),
            ..head
        };
        if matches!(
            write_head(store, bucket, &next, Some(tag)).await?,
            Publish::Landed
        ) {
            return Ok(n);
        }
    }
    Err(CatalogError::Contended(bucket, MAX_CAS_ATTEMPTS))
}

/// Deletes runs in this bucket that **nothing names** — the objects a fold leaves behind when
/// it writes its run and then loses the head CAS.
///
/// ⚠️ **The only LIST in the catalog, and the only path the corpus permits one on.**
/// `list_unrestricted` is priced like a PUT and returns at most 1000 keys; a sweeper is GC,
/// which is exactly what that API is reserved for. The cost is one LIST and one batched delete
/// **per bucket**, and it never scales with tenants.
///
/// ⚠️ **An orphan and a run in flight look identical**, and confusing them is far worse than
/// leaving garbage: an orphan is garbage, and a run a fold is *about to* commit is a bucket's
/// worth of tenants. The epoch in the key separates them with no clock — a run in flight was
/// written against the head its writer read, so its `run_epoch` is strictly **greater** than
/// the head's. Only what the head has already moved past is garbage.
///
/// ⚠️ Two folders racing at one epoch produce two runs at `head.run_epoch + 1`; after the
/// winner commits, the loser's epoch **equals** the head's rather than being less, so it
/// survives this sweep and the next one collects it. Late, and never early.
///
/// ⚠️ **Anything it cannot positively identify is kept** — the bucket's own `HEAD` shares this
/// prefix, and so would any sibling a later milestone adds. The opposite default, "delete what
/// I do not recognise", deletes the pointer.
///
/// Records nothing and writes no head: an orphan is defined by *absence* from the head, so
/// there is nothing to write down. That makes it idempotent and safe beside a concurrent fold,
/// where the worst case is a head one epoch stale and one run fewer reaped.
///
/// Returns how many objects it swept.
///
/// # Errors
/// If the store refuses, the head is malformed, or the backend cannot fence.
pub async fn sweep<S: BlobStore>(store: &S, bucket: u32) -> Result<usize, CatalogError> {
    crate::require_fencing(store)?;
    let (head, _) = read_head(store, bucket).await?;
    // ⚠️ The graveyard only. The head's OWN run needs no entry here: its epoch equals
    // `head.run_epoch`, and the filter below keeps anything not strictly below that. A push
    // for it was written first and removed — a mutation sweep could not distinguish it from
    // its own absence, and `a_run_in_flight_survives_the_sweep` is what actually protects the
    // live run by pinning the comparison as strict. The graveyard's entries ARE below the
    // head's epoch, which is why they need naming and this does not.
    let named: Vec<(Epoch, u64)> = head.graveyard.clone();

    let prefix = keys::bucket_prefix(bucket);
    let doomed: Vec<pstore_blob::Key> = store
        .list_unrestricted(&prefix)
        .await?
        .into_iter()
        .filter(|k| {
            // Keep anything that is not a run key this version can read back.
            let Some((epoch, digest)) = keys::parse_run_key(bucket, k) else {
                return false;
            };
            epoch < head.run_epoch && !named.contains(&(epoch, digest))
        })
        .collect();

    let n = doomed.len();
    let cap = store.capabilities().max_batch_delete.max(1);
    for chunk in doomed.chunks(cap) {
        store.delete_batch(chunk).await?;
    }
    Ok(n)
}

/// Drains a bucket's pending records into a new immutable run.
///
/// Optimistic and leaderless: any node may call it, the CAS decides, and a loser rebases onto
/// the winner's head and folds again. Returns whether it published anything.
///
/// # Errors
/// If the store refuses, an object is malformed, or the bucket stays contended.
pub async fn fold<S: BlobStore>(store: &S, bucket: u32) -> Result<bool, CatalogError> {
    crate::require_fencing(store)?;
    for _ in 0..MAX_CAS_ATTEMPTS {
        let (head, tag) = read_head(store, bucket).await?;
        // A bucket with nothing pending has nothing to publish -- and a bucket whose pointer
        // does not exist yet has nothing pending, so one guard covers both.
        let Some(tag) = tag.filter(|_| !head.pending.is_empty()) else {
            return Ok(false);
        };
        // Someone else folding or appending between our read and our write makes their head
        // the world; re-read it and fold what is pending there.
        if matches!(
            publish(store, bucket, &head, tag, &[]).await?,
            Publish::Landed
        ) {
            return Ok(true);
        }
    }
    Err(CatalogError::Contended(bucket, MAX_CAS_ATTEMPTS))
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]
mod tests {
    use super::*;
    use pstore_types::TenantId;

    fn live(t: u128, e: u64) -> TenantRecord {
        TenantRecord::live(TenantId(t), Epoch(e), &[format!("i{e}")])
    }

    #[test]
    fn heads_round_trip_and_a_truncated_one_is_an_error() {
        let h = BucketHead {
            run_epoch: Epoch(3),
            digest: 0xdead_beef,
            pending: vec![live(1, 2)],
            graveyard: vec![(Epoch(2), 0xf00d), (Epoch(1), 0xbeef)],
        };
        let buf = h.encode();
        assert_eq!(BucketHead::decode(&buf, "t").unwrap(), h);
        // ⚠️ Every cut except the boundary where `pending` ends and the graveyard begins:
        // that one is a pre-M6d head, which decodes with an empty graveyard rather than
        // failing, and it is the whole of the additive promise.
        let old = BucketHead {
            graveyard: Vec::new(),
            ..h.clone()
        };
        let seam = old.encode().len() - 4;
        for cut in 0..buf.len() {
            if cut == seam {
                assert_eq!(
                    BucketHead::decode(&buf[..cut], "t").unwrap(),
                    old,
                    "a head with no graveyard field must decode as an empty graveyard"
                );
                continue;
            }
            assert!(BucketHead::decode(&buf[..cut], "t").is_err(), "cut {cut}");
        }
    }

    #[test]
    fn a_head_before_its_first_fold_names_no_run() {
        assert!(BucketHead::default().run(3).is_none());
        let h = BucketHead {
            run_epoch: Epoch(1),
            digest: 0,
            pending: vec![],
            graveyard: vec![],
        };
        assert!(h.run(3).is_some());
    }

    #[test]
    fn the_newest_epoch_wins() {
        let run = vec![live(1, 5), live(2, 5)];
        // Older loses, newer wins, and equal-epoch is the later observation -- `>` here
        // instead of `>=` makes a re-record a silent no-op.
        let out = merge(run, [&live(1, 4), &live(2, 6), &live(3, 5)]);
        assert_eq!(out.len(), 3);
        assert_eq!(out[0].epoch, Epoch(5));
        assert_eq!(out[1].epoch, Epoch(6));
        let same = merge(
            vec![live(1, 5)],
            [&TenantRecord::deleted(TenantId(1), Epoch(5))],
        );
        assert_eq!(same[0].state, crate::State::Deleted);
    }

    /// Criterion 9. ⚠️ **The append between the two head reads is the whole fixture.** Two
    /// folders draining an identical head write byte-identical runs to the same key, so the
    /// interleaving that loses a record cannot happen and the test proves nothing. Here A
    /// holds a head with one record while B publishes two, so A's run is a *different* object
    /// with the same `run_epoch` — which is exactly what the digest in the key is for.
    #[tokio::test]
    async fn racing_folders_lose_no_record() {
        use pstore_blob::MemoryStore;
        use std::sync::Arc;
        let store = Arc::new(MemoryStore::new());
        let width = Width::new(1).unwrap();
        let app = crate::Appender::new(Arc::clone(&store), width);

        app.record(&live(1, 1)).await.unwrap();
        // Folder A reads the world as it is now.
        let (head_a, tag_a) = read_head(store.as_ref(), 0).await.unwrap();
        let tag_a = tag_a.unwrap();

        // An append lands between the two folders' reads.
        app.record(&live(2, 1)).await.unwrap();
        let (head_b, tag_b) = read_head(store.as_ref(), 0).await.unwrap();
        let tag_b = tag_b.unwrap();

        // B publishes first and wins the head.
        assert!(matches!(
            publish(store.as_ref(), 0, &head_b, tag_b, &[])
                .await
                .unwrap(),
            Publish::Landed
        ));
        // A publishes second: its run PUT must not land on top of B's, and its CAS must lose.
        assert!(matches!(
            publish(store.as_ref(), 0, &head_a, tag_a, &[])
                .await
                .unwrap(),
            Publish::Rebase
        ));

        let out = crate::enumerate(store.as_ref(), width).await.unwrap();
        let ids: Vec<_> = out.records.iter().map(|r| r.tenant.0).collect();
        assert_eq!(ids, vec![1, 2], "the loser's run overwrote the winner's");
    }

    #[test]
    fn merge_keeps_tombstones() {
        let out = merge(
            vec![live(1, 1)],
            [&TenantRecord::deleted(TenantId(1), Epoch(2))],
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].state, crate::State::Deleted);
    }
}
