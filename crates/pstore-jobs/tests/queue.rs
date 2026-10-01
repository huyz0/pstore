//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! The register's operations (M22 criterion 7): `open`, `claim`, `renew`, `reconcile`.

mod common;

use common::Counting;
use pstore_jobs::{Claim, Entry, JobsError, Register, fnv1a};
use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

const TTL: u64 = 1_000;

async fn open(store: &Counting, shards: u16) -> Register<Counting> {
    Register::open(Arc::new(store.clone()), "rep", shards)
        .await
        .unwrap()
}

/// Ids that all live in one shard of `reg`, so a test can fill one shard on purpose.
fn ids_in_shard(reg: &Register<Counting>, shard: u16, n: usize) -> Vec<String> {
    (0..10_000)
        .map(|i| format!("t{i}"))
        .filter(|id| reg.shard_of(id) == shard)
        .take(n)
        .collect()
}

async fn add(reg: &Register<Counting>, id: &str, g: u64, claim: Option<Claim>) {
    let e = reg
        .reconcile(id, || async move { Ok(Some(g)) }, claim, false)
        .await
        .unwrap();
    assert_eq!(e.map(|e| e.generation), Some(g));
}

#[test]
fn fnv1a_is_the_published_function() {
    // The reference vectors: a hash that changes between builds would move every id to
    // another shard, and an upgraded fleet would stop finding its work.
    assert_eq!(fnv1a(b""), 0xcbf2_9ce4_8422_2325);
    assert_eq!(fnv1a(b"a"), 0xaf63_dc4c_8601_ec8c);
    assert_eq!(fnv1a(b"foobar"), 0x8594_4171_f739_67e8);
}

#[tokio::test]
async fn config_is_created_once_and_its_count_wins() {
    let store = Counting::new();
    assert_eq!(open(&store, 8).await.shards(), 8);
    let puts = store.cas();
    // A second process configured differently joins the first one's register.
    assert_eq!(open(&store, 16).await.shards(), 8);
    assert_eq!(store.cas(), puts, "an existing CONFIG was rewritten");
    assert_eq!(puts, 1);
}

#[tokio::test]
async fn on_another_view_is_the_same_register_at_no_cost() {
    let store = Counting::new();
    let reg = open(&store, 8).await;
    let other = store.recounted();
    let view = reg.on(Arc::new(other.clone()));
    assert_eq!(other.reads() + other.cas(), 0, "a view cost a request");
    assert_eq!(view.shards(), 8);
    assert_eq!(view.shard_key(3), reg.shard_key(3));
    // Writes through the view land where the register reads.
    view.reconcile("t1", || async { Ok(Some(4)) }, None, true)
        .await
        .unwrap();
    assert_eq!(
        reg.read(reg.shard_of("t1")).await.unwrap().entries["t1"].generation,
        4
    );
    assert!(
        other.cas() > 0 && store.cas() == 1,
        "billed to the wrong view"
    );
}

#[tokio::test]
async fn ids_spread_over_every_shard() {
    let reg = open(&Counting::new(), 8).await;
    let used: BTreeSet<u16> = (0..200).map(|i| reg.shard_of(&format!("t{i}"))).collect();
    assert_eq!(used, (0..8).collect::<BTreeSet<_>>());
    // Stable: the same id, the same shard, from any process.
    assert_eq!(reg.shard_of("t7"), u16::try_from(fnv1a(b"t7") % 8).unwrap());
}

#[tokio::test]
async fn shards_have_distinct_keys_under_the_registers_name() {
    let reg = open(&Counting::new(), 4).await;
    let keys: BTreeSet<String> = (0..4)
        .map(|s| reg.shard_key(s).as_str().to_owned())
        .collect();
    assert_eq!(keys.len(), 4);
    assert!(keys.iter().all(|k| k.contains("/jobs/rep/")), "{keys:?}");
}

#[tokio::test]
async fn reconcile_adds_updates_and_removes() {
    let store = Counting::new();
    let reg = open(&store, 4).await;
    add(&reg, "t1", 5, None).await;
    let shard = reg.shard_of("t1");
    assert_eq!(
        reg.read(shard).await.unwrap().entries["t1"],
        Entry {
            generation: 5,
            claim: None
        }
    );
    add(&reg, "t1", 6, None).await;
    assert_eq!(reg.read(shard).await.unwrap().entries["t1"].generation, 6);
    let gone = reg
        .reconcile("t1", || async { Ok(None) }, None, false)
        .await
        .unwrap();
    assert_eq!(gone, None);
    assert!(reg.read(shard).await.unwrap().entries.is_empty());
}

#[tokio::test]
async fn reconcile_that_changes_nothing_writes_nothing() {
    let store = Counting::new();
    let reg = open(&store, 4).await;
    add(&reg, "t1", 5, None).await;
    let before = store.cas();
    add(&reg, "t1", 5, None).await;
    reg.reconcile("t9", || async { Ok(None) }, None, false)
        .await
        .unwrap();
    assert_eq!(store.cas(), before);
}

#[tokio::test]
async fn a_touch_writes_when_nothing_changed_and_every_write_is_new_bytes() {
    // A control call's reconcile writes even when the shard agrees, and no two writes leave
    // the same bytes: on a content-hashed tag a rewrite of identical bytes keeps the tag, and
    // a writer holding it would land against a changed world (ABA).
    let store = Counting::with_tags(pstore_blob::TagStyle::ContentHash);
    let reg = open(&store, 4).await;
    add(&reg, "t1", 5, None).await;
    let key = reg.shard_key(reg.shard_of("t1"));
    let tag = |s: &Counting| {
        let s = s.clone();
        let key = key.clone();
        async move {
            use pstore_blob::BlobStore;
            s.inner.get_tag(&key).await.unwrap()
        }
    };
    let (before, cas) = (tag(&store).await, store.cas());
    reg.reconcile("t1", || async { Ok(Some(5)) }, None, true)
        .await
        .unwrap();
    assert_eq!(store.cas(), cas + 1, "a touch did not write");
    assert_ne!(tag(&store).await, before, "a touch left the tag as it was");
    // A touch of an id with no work and no entry writes too.
    reg.reconcile("t9", || async { Ok(None) }, None, true)
        .await
        .unwrap();
    assert_eq!(store.cas(), cas + 2);
}

#[tokio::test]
async fn reconcile_reads_the_shard_before_the_truth() {
    // The argument the register rests on. `want` runs after the shard read: by the time it
    // is asked, the shard read has happened.
    let store = Counting::new();
    let reg = open(&store, 4).await;
    let s = store.clone();
    let before = store.reads();
    reg.reconcile(
        "t1",
        || {
            let s = s.clone();
            async move {
                assert_eq!(s.reads(), before + 1, "want ran before the shard read");
                Ok(Some(1))
            }
        },
        None,
        false,
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn reconcile_claims_only_a_new_entry() {
    let store = Counting::new();
    let reg = open(&store, 4).await;
    let mine = Claim {
        owner: 1,
        expires_ms: 500,
    };
    add(&reg, "t1", 5, Some(mine)).await;
    let shard = reg.shard_of("t1");
    assert_eq!(
        reg.read(shard).await.unwrap().entries["t1"].claim,
        Some(mine)
    );
    // An existing entry keeps whoever holds it: a control call never steals work.
    let theirs = Claim {
        owner: 2,
        expires_ms: 900,
    };
    add(&reg, "t1", 6, Some(theirs)).await;
    assert_eq!(
        reg.read(shard).await.unwrap().entries["t1"],
        Entry {
            generation: 6,
            claim: Some(mine)
        }
    );
}

#[tokio::test]
async fn reconcile_surfaces_a_failing_truth_and_writes_nothing() {
    let store = Counting::new();
    let reg = open(&store, 4).await;
    let before = store.cas();
    let r = reg
        .reconcile(
            "t1",
            || async { Err(JobsError::Want("HEAD unreadable".to_owned())) },
            None,
            false,
        )
        .await;
    assert!(matches!(r, Err(JobsError::Want(_))), "{r:?}");
    assert_eq!(store.cas(), before);
}

#[tokio::test]
async fn claim_takes_unclaimed_expired_and_own_and_skips_live_foreign() {
    let store = Counting::new();
    let reg = open(&store, 1).await;
    let claim = |owner, expires_ms| Some(Claim { owner, expires_ms });
    add(&reg, "free", 1, None).await;
    add(&reg, "lapsed", 1, claim(2, 100)).await;
    add(&reg, "edge", 1, claim(2, 200)).await;
    add(&reg, "live", 1, claim(2, 201)).await;
    add(&reg, "mine", 1, claim(1, 50)).await;
    // At 200: a claim expiring AT now has lapsed, one expiring after it has not.
    let held = reg.claim(0, 1, 200, TTL, 10).await.unwrap();
    let names: BTreeSet<&str> = held.keys().map(String::as_str).collect();
    assert_eq!(names, BTreeSet::from(["free", "lapsed", "edge", "mine"]));
    for e in held.values() {
        assert_eq!(e.claim, claim(1, 200 + TTL));
    }
    let after = reg.read(0).await.unwrap();
    assert_eq!(after.entries["live"].claim, claim(2, 201));
    assert_eq!(after.entries["mine"].claim, claim(1, 200 + TTL));
}

#[tokio::test]
async fn claim_honours_room_and_never_counts_its_own() {
    let store = Counting::new();
    let reg = open(&store, 1).await;
    for i in 0..5 {
        add(&reg, &format!("t{i}"), 1, None).await;
    }
    add(
        &reg,
        "own",
        1,
        Some(Claim {
            owner: 1,
            expires_ms: 10,
        }),
    )
    .await;
    let held = reg.claim(0, 1, 0, TTL, 2).await.unwrap();
    assert_eq!(held.len(), 3, "{held:?}");
    assert!(held.contains_key("own"));
    let free = reg
        .read(0)
        .await
        .unwrap()
        .entries
        .values()
        .filter(|e| e.claim.is_none())
        .count();
    assert_eq!(free, 3);
}

#[tokio::test]
async fn claim_leaves_its_own_live_claims_alone() {
    // A scan that finds only what this worker already holds writes nothing: renewal extends
    // live claims, and a scan rewriting them would be a write per scan the spec does not
    // count (found writing M22.3's cost test).
    let store = Counting::new();
    let reg = open(&store, 1).await;
    let mine = Claim {
        owner: 1,
        expires_ms: 500,
    };
    add(&reg, "t1", 1, Some(mine)).await;
    let before = store.cas();
    let held = reg.claim(0, 1, 100, TTL, 10).await.unwrap();
    assert_eq!(held["t1"].claim, Some(mine));
    assert_eq!(store.cas(), before);
    // Expiring at now, it has lapsed: re-taken, as a foreign one would be.
    let held = reg.claim(0, 1, 500, TTL, 10).await.unwrap();
    assert_eq!(held["t1"].claim.unwrap().expires_ms, 500 + TTL);
}

#[tokio::test]
async fn claim_of_nothing_new_writes_nothing() {
    let store = Counting::new();
    let reg = open(&store, 1).await;
    add(
        &reg,
        "theirs",
        1,
        Some(Claim {
            owner: 2,
            expires_ms: 9_999,
        }),
    )
    .await;
    let before = store.cas();
    assert!(reg.claim(0, 1, 0, TTL, 10).await.unwrap().is_empty());
    assert_eq!(store.cas(), before);
    // An absent shard too: not an error, and neither claiming nor renewing it creates it.
    let other = Register::open(Arc::new(store.clone()), "other", 1)
        .await
        .unwrap();
    let before = store.cas();
    assert!(other.claim(0, 1, 0, TTL, 10).await.unwrap().is_empty());
    assert!(
        other
            .renew(0, 1, 0, TTL, &BTreeSet::new())
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(store.cas(), before);
}

#[tokio::test]
async fn claim_and_renew_never_add_an_entry() {
    let store = Counting::new();
    let reg = open(&store, 1).await;
    reg.claim(0, 1, 0, TTL, 10).await.unwrap();
    reg.renew(0, 1, 0, TTL, &BTreeSet::new()).await.unwrap();
    assert!(reg.read(0).await.unwrap().entries.is_empty());
}

#[tokio::test]
async fn renew_extends_its_own_and_drops_what_it_lost() {
    let store = Counting::new();
    let reg = open(&store, 1).await;
    let ids = ids_in_shard(&reg, 0, 3);
    for id in &ids {
        add(&reg, id, 1, None).await;
    }
    assert_eq!(reg.claim(0, 1, 0, TTL, 10).await.unwrap().len(), 3);
    // One removed, one taken after it lapsed: renew must not hand either back.
    reg.reconcile(&ids[0], || async { Ok(None) }, None, false)
        .await
        .unwrap();
    assert_eq!(reg.claim(0, 2, TTL, TTL, 1).await.unwrap().len(), 1);
    let taken: Vec<String> = reg
        .read(0)
        .await
        .unwrap()
        .entries
        .iter()
        .filter(|(_, e)| e.claim.is_some_and(|c| c.owner == 2))
        .map(|(k, _)| k.clone())
        .collect();
    assert_eq!(taken.len(), 1);
    let held = reg
        .renew(0, 1, TTL, TTL, &ids.iter().cloned().collect())
        .await
        .unwrap();
    assert_eq!(held.len(), 1, "{held:?}");
    let kept = held.keys().next().unwrap();
    assert!(kept != &ids[0] && kept != &taken[0]);
    assert_eq!(held[kept].claim.unwrap().expires_ms, 2 * TTL);
    // Renew keeps `gen`: it never overwrites what a reconcile wrote.
    assert_eq!(held[kept].generation, 1);
    assert_eq!(reg.read(0).await.unwrap().entries.len(), 2);
}

#[tokio::test]
async fn renew_releases_what_the_worker_no_longer_holds() {
    // A worker that dropped a tenant -- over its max, or told to -- must not keep it from
    // everyone else by renewing it with the rest.
    let store = Counting::new();
    let reg = open(&store, 1).await;
    for id in ["a", "b"] {
        add(
            &reg,
            id,
            1,
            Some(Claim {
                owner: 1,
                expires_ms: 100,
            }),
        )
        .await;
    }
    let held = reg
        .renew(0, 1, 50, TTL, &BTreeSet::from(["a".to_owned()]))
        .await
        .unwrap();
    assert_eq!(held.keys().collect::<Vec<_>>(), ["a"]);
    let shard = reg.read(0).await.unwrap();
    assert_eq!(shard.entries["a"].claim.unwrap().expires_ms, 50 + TTL);
    assert_eq!(shard.entries["b"].claim, None, "kept from everyone else");
    // And another worker takes it at once.
    assert!(
        reg.claim(0, 2, 60, TTL, 10)
            .await
            .unwrap()
            .contains_key("b")
    );
}

#[tokio::test]
async fn renew_still_holds_its_own_lapsed_claim_nobody_took() {
    let store = Counting::new();
    let reg = open(&store, 1).await;
    add(
        &reg,
        "t1",
        1,
        Some(Claim {
            owner: 1,
            expires_ms: 10,
        }),
    )
    .await;
    let held = reg
        .renew(0, 1, 500, TTL, &BTreeSet::from(["t1".to_owned()]))
        .await
        .unwrap();
    assert_eq!(held["t1"].claim.unwrap().expires_ms, 500 + TTL);
}

#[tokio::test]
async fn contended_writes_are_retried_unchanged() {
    let store = Counting::new();
    let reg = open(&store, 1).await;
    let (before, reads) = (store.cas(), store.reads());
    store.contend_at(&[before, before + 1]);
    add(&reg, "t1", 3, None).await;
    assert_eq!(store.cas(), before + 3);
    // Unchanged: a 409 is retried as it was, never re-read as a 412 would be.
    assert_eq!(store.reads(), reads + 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_claimants_take_each_entry_once() {
    let store = Counting::new();
    let reg = open(&store, 1).await;
    for i in 0..12 {
        add(&reg, &format!("t{i}"), 1, None).await;
    }
    let base = store.cas();
    store.contend_at(&[base, base + 2, base + 5]);
    let a = Arc::new(open(&store, 1).await);
    let b = Arc::new(open(&store, 1).await);
    let (ha, hb) = tokio::join!(
        {
            let a = Arc::clone(&a);
            async move { a.claim(0, 1, 0, TTL, 7).await.unwrap() }
        },
        {
            let b = Arc::clone(&b);
            async move { b.claim(0, 2, 0, TTL, 7).await.unwrap() }
        }
    );
    let ka: BTreeSet<_> = ha.keys().collect();
    let kb: BTreeSet<_> = hb.keys().collect();
    assert!(ka.is_disjoint(&kb), "{ka:?} {kb:?}");
    assert_eq!(ka.len() + kb.len(), 12);
    let shard = reg.read(0).await.unwrap();
    for (id, e) in &shard.entries {
        let owner = e.claim.unwrap().owner;
        assert_eq!(owner == 1, ka.contains(id), "{id}");
    }
}

#[tokio::test]
async fn a_corrupt_shard_is_an_error_not_an_empty_one() {
    use pstore_blob::BlobStore;
    let store = Counting::new();
    let reg = open(&store, 1).await;
    store
        .inner
        .put(&reg.shard_key(0), bytes::Bytes::from_static(b"not json"))
        .await
        .unwrap();
    assert!(matches!(reg.read(0).await, Err(JobsError::Corrupt(_))));
    // And nothing overwrites it: a reconcile that cannot read must not write.
    let r = reg
        .reconcile("t1", || async { Ok(Some(1)) }, None, false)
        .await;
    assert!(matches!(r, Err(JobsError::Corrupt(_))), "{r:?}");
}

#[tokio::test]
async fn a_refused_read_is_an_error_and_nothing_is_written() {
    // A shard that cannot be read is not an empty one: treating it as empty would let a
    // reconcile, a claim or a renew replace every entry in it with its own.
    let store = Counting::new();
    let reg = open(&store, 1).await;
    add(&reg, "t1", 1, None).await;
    store
        .refuse_reads
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let before = store.cas();
    assert!(matches!(reg.read(0).await, Err(JobsError::Blob(_))));
    let r = reg
        .reconcile("t2", || async { Ok(Some(1)) }, None, true)
        .await;
    assert!(matches!(r, Err(JobsError::Blob(_))), "{r:?}");
    assert!(reg.claim(0, 1, 0, TTL, 10).await.is_err());
    assert!(reg.renew(0, 1, 0, TTL, &BTreeSet::new()).await.is_err());
    assert_eq!(store.cas(), before);
}

#[tokio::test]
async fn nothing_lists() {
    let store = Counting::new();
    let reg = open(&store, 4).await;
    add(&reg, "t1", 1, None).await;
    for s in 0..4 {
        reg.claim(s, 1, 0, TTL, 10).await.unwrap();
        reg.renew(s, 1, 0, TTL, &BTreeSet::new()).await.unwrap();
    }
    assert_eq!(
        store.counts.lists.load(std::sync::atomic::Ordering::SeqCst),
        0
    );
}

#[tokio::test(start_paused = true)]
async fn every_write_gives_up_after_its_attempts_and_waits_between_them() {
    use pstore_jobs::MAX_ATTEMPTS;
    // Always contended: retried as it was, exactly MAX_ATTEMPTS times, then refused.
    let store = Counting::new();
    let reg = open(&store, 1).await;
    let base = store.cas();
    store.contend_at(&(base..base + 1_000).collect::<Vec<_>>());
    let start = tokio::time::Instant::now();
    let r = reg
        .reconcile("t1", || async { Ok(Some(1)) }, None, true)
        .await;
    assert!(matches!(r, Err(JobsError::Contended)), "{r:?}");
    assert_eq!(store.cas() - base, u64::from(MAX_ATTEMPTS));
    // And it waited, longer each time: 2 + 4 + ... ms, capped at 64.
    assert!(
        start.elapsed() >= Duration::from_millis(2 + 4 + 8 + 16 + 32),
        "{:?}",
        start.elapsed()
    );
    // Always lost: re-read and retried, exactly as many times, by every operation.
    let store = Counting::new();
    let reg = open(&store, 1).await;
    // Claimed by 1 and lapsed, so a claim and a renewal each have something to write.
    add(
        &reg,
        "t1",
        1,
        Some(Claim {
            owner: 1,
            expires_ms: 0,
        }),
    )
    .await;
    store
        .lose_all
        .store(true, std::sync::atomic::Ordering::SeqCst);
    for op in 0..3 {
        let base = store.cas();
        let r = match op {
            0 => reg
                .reconcile("t1", || async { Ok(Some(2)) }, None, false)
                .await
                .map(|_| ()),
            1 => reg.claim(0, 1, 10, TTL, 10).await.map(|_| ()),
            _ => reg
                .renew(0, 1, 10, TTL, &BTreeSet::from(["t1".to_owned()]))
                .await
                .map(|_| ()),
        };
        assert!(matches!(r, Err(JobsError::Contended)), "op {op}: {r:?}");
        assert_eq!(store.cas() - base, u64::from(MAX_ATTEMPTS), "op {op}");
    }
}

#[tokio::test]
async fn keys_carry_the_published_partition_prefix() {
    // The prefix spreads a register over a bucket's partitions; derived from the name and
    // shard alone, so every process lands on the same keys.
    let reg = open(&Counting::new(), 4).await;
    for shard in 0..4u16 {
        let want = format!(
            "{:04x}/jobs/rep/{shard:04x}/QUEUE",
            fnv1a(format!("rep/{shard}").as_bytes()) & 0xffff
        );
        assert_eq!(reg.shard_key(shard).as_str(), want);
    }
}
