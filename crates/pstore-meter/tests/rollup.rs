//! A tenant's error budget, rolled up across the fleet — M109.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

use bytes::Bytes;
use pstore_blob::{
    Accounted, BlobError, BlobStore, Capabilities, CasError, Key, MemoryStore, OpClass,
    Precondition, PutOutcome,
};
use pstore_meter::{
    Answer, Budget, INDEX_SHARDS, Meter, Month, Quota, ReportError, Reporter, rollup,
};
use pstore_types::{CasTag, TenantId};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

const A: TenantId = TenantId(1);
const B: TenantId = TenantId(2);
const MARCH: Month = Month {
    year: 2026,
    month: 3,
};
const APRIL: Month = Month {
    year: 2026,
    month: 4,
};

fn meter() -> Meter {
    Meter::new(Quota::unlimited())
}

/// `ok` successful answers and `failed` failed ones to `tenant`.
fn serve(m: &Meter, tenant: TenantId, ok: u64, failed: u64) {
    for _ in 0..ok {
        m.answer(tenant, Answer::Ok);
    }
    for _ in 0..failed {
        m.answer(tenant, Answer::Failed);
    }
}

fn answers(store: &pstore_meter::Rollup, t: TenantId) -> (u64, u64) {
    let u = store.usage(t);
    (u.answered, u.failed)
}

/// Criterion 1: the fleet's counts are the sum of the nodes'.
#[tokio::test]
async fn the_fleets_counts_are_the_sum_of_the_nodes() {
    let store = MemoryStore::new();
    for (i, (a_ok, a_bad, b_ok)) in [(10, 1, 5), (20, 0, 0), (30, 2, 7)].into_iter().enumerate() {
        let m = meter();
        let mut r = Reporter::resume(&store, &format!("n{i}"), MARCH, &m)
            .await
            .unwrap();
        serve(&m, A, a_ok, a_bad);
        serve(&m, B, b_ok, 0);
        m.settle(A, OpClass::Read, 3, 100, Duration::ZERO);
        r.publish(&store, &m, MARCH).await.unwrap();
    }

    let up = rollup(&store, MARCH).await.unwrap();

    assert_eq!(up.nodes, ["n0", "n1", "n2"]);
    assert_eq!(answers(&up, A), (60 + 3, 3));
    assert_eq!(answers(&up, B), (12, 0));
    assert_eq!(up.usage(A).requests_of(OpClass::Read), 9);
    assert_eq!(up.usage(A).bytes_of(OpClass::Read), 300);
}

/// Criterion 2: a node that left is still counted, with no member list passed.
#[tokio::test]
async fn a_node_that_left_is_still_counted() {
    let store = MemoryStore::new();
    let gone = meter();
    let mut r = Reporter::resume(&store, "gone", MARCH, &gone)
        .await
        .unwrap();
    serve(&gone, A, 4, 4);
    r.publish(&store, &gone, MARCH).await.unwrap();
    drop((r, gone));

    let up = rollup(&store, MARCH).await.unwrap();

    assert_eq!(up.nodes, ["gone"], "the index lost a node that left");
    assert_eq!(answers(&up, A), (8, 4));
}

/// Criterion 3: a restart continues the month.
#[tokio::test]
async fn a_restart_continues_the_month() {
    let store = MemoryStore::new();
    let first = meter();
    let mut r = Reporter::resume(&store, "n1", MARCH, &first).await.unwrap();
    serve(&first, A, 10, 1);
    r.publish(&store, &first, MARCH).await.unwrap();

    // The process is replaced: a fresh meter, counting from zero.
    let second = meter();
    let mut r = Reporter::resume(&store, "n1", MARCH, &second)
        .await
        .unwrap();
    serve(&second, A, 5, 2);
    r.publish(&store, &second, MARCH).await.unwrap();

    let up = rollup(&store, MARCH).await.unwrap();
    assert_eq!(
        answers(&up, A),
        (11 + 7, 3),
        "the restarted process lost or doubled the first half of the month"
    );
}

/// Criterion 4: a new month starts at zero, and the old one is unchanged.
#[tokio::test]
async fn a_new_month_starts_at_zero() {
    let store = MemoryStore::new();
    let m = meter();
    let mut r = Reporter::resume(&store, "n1", MARCH, &m).await.unwrap();
    serve(&m, A, 10, 1);
    r.publish(&store, &m, MARCH).await.unwrap();
    serve(&m, A, 3, 1);
    r.publish(&store, &m, APRIL).await.unwrap();

    assert_eq!(answers(&rollup(&store, MARCH).await.unwrap(), A), (11, 1));
    assert_eq!(answers(&rollup(&store, APRIL).await.unwrap(), A), (4, 1));
    // And April continues: a second April publish adds, never restarts.
    serve(&m, A, 1, 0);
    r.publish(&store, &m, APRIL).await.unwrap();
    assert_eq!(answers(&rollup(&store, APRIL).await.unwrap(), A), (5, 1));
}

/// Criterion 5: two processes with one node id are refused, and the first is kept.
#[tokio::test]
async fn two_processes_with_one_node_id_are_refused() {
    let store = MemoryStore::new();
    let (m1, m2) = (meter(), meter());
    let mut first = Reporter::resume(&store, "n1", MARCH, &m1).await.unwrap();
    let mut second = Reporter::resume(&store, "n1", MARCH, &m2).await.unwrap();
    serve(&m1, A, 10, 0);
    first.publish(&store, &m1, MARCH).await.unwrap();
    serve(&m2, A, 1, 1);

    let err = second
        .publish(&store, &m2, MARCH)
        .await
        .expect_err("a second process overwrote the first one's report");

    assert!(
        matches!(&err, ReportError::Contended(n) if n == "n1"),
        "{err}"
    );
    assert_eq!(answers(&rollup(&store, MARCH).await.unwrap(), A), (10, 0));
}

/// Criterion 6: what a publish and a rollup cost.
#[tokio::test]
async fn a_publish_is_one_write_and_a_rollup_is_one_read_per_shard_and_per_node() {
    let t = TenantId(0);
    let acct = Accounted::new(MemoryStore::new());
    let view = acct.as_tenant(t);
    let n = 5u64;
    let mut reporters = Vec::new();
    for i in 0..n {
        let m = meter();
        let r = Reporter::resume(&view, &format!("n{i}"), MARCH, &m)
            .await
            .unwrap();
        reporters.push((r, m));
    }
    let count = |c| acct.count(t, c);

    // The month's first publish: the report, then the shard's read and CAS.
    let (r, m) = &mut reporters[0];
    serve(m, A, 1, 0);
    let (reads, writes) = (count(OpClass::Read), count(OpClass::Write));
    r.publish(&view, m, MARCH).await.unwrap();
    assert_eq!(
        (count(OpClass::Read) - reads, count(OpClass::Write) - writes),
        (1, 2)
    );
    // Every later publish: one write.
    serve(m, A, 1, 0);
    let (reads, writes) = (count(OpClass::Read), count(OpClass::Write));
    r.publish(&view, m, MARCH).await.unwrap();
    assert_eq!(
        (count(OpClass::Read) - reads, count(OpClass::Write) - writes),
        (0, 1)
    );
    for (r, m) in reporters.iter_mut().skip(1) {
        serve(m, A, 1, 0);
        r.publish(&view, m, MARCH).await.unwrap();
    }

    let (reads, writes) = (count(OpClass::Read), count(OpClass::Write));
    let up = rollup(&view, MARCH).await.unwrap();
    assert_eq!(count(OpClass::Read) - reads, u64::from(INDEX_SHARDS) + n);
    assert_eq!(count(OpClass::Write) - writes, 0);
    assert_eq!(answers(&up, A), (n + 1, 0));
    assert_eq!(count(OpClass::List), 0, "the rollup listed");
}

/// Criterion 7: the budget arithmetic.
#[tokio::test]
async fn the_budget_allows_the_target_and_not_one_failure_more() {
    let store = MemoryStore::new();
    for (node, failed) in [("ten", 10u64), ("eleven", 11)] {
        let m = meter();
        let mut r = Reporter::resume(&store, node, MARCH, &m).await.unwrap();
        // One tenant per node, so each rollup reads one budget.
        let t = TenantId(u128::from(failed));
        serve(&m, t, 10_000 - failed, failed);
        r.publish(&store, &m, MARCH).await.unwrap();
    }
    let up = rollup(&store, MARCH).await.unwrap();

    assert_eq!(
        up.budget(TenantId(10), 999_000),
        Budget {
            answered: 10_000,
            failed: 10,
            allowed: 10,
            remaining: 0,
            exhausted: false,
        }
    );
    assert_eq!(
        up.budget(TenantId(11), 999_000),
        Budget {
            answered: 10_000,
            failed: 11,
            allowed: 10,
            remaining: -1,
            exhausted: true,
        }
    );
    // A tenant nobody served has spent nothing and is owed nothing.
    let none = up.budget(TenantId(99), 999_000);
    assert_eq!(
        (none.allowed, none.remaining, none.exhausted),
        (0, 0, false)
    );
}

/// Criterion 8: month arithmetic.
#[test]
fn a_month_is_the_utc_calendar_month() {
    let at = |s: u64| Month::of(Duration::from_secs(s));
    assert_eq!(
        at(0),
        Month {
            year: 1970,
            month: 1
        }
    );
    // 2024-02-29T23:59:59Z and the second after it.
    assert_eq!(
        at(1_709_251_199),
        Month {
            year: 2024,
            month: 2
        }
    );
    assert_eq!(
        at(1_709_251_200),
        Month {
            year: 2024,
            month: 3
        }
    );
    // 1999-12-31T23:59:59Z and the second after it: a year boundary.
    assert_eq!(
        at(946_684_799),
        Month {
            year: 1999,
            month: 12
        }
    );
    assert_eq!(
        at(946_684_800),
        Month {
            year: 2000,
            month: 1
        }
    );
}

/// Review round 1: four nines over 10,000 answers allows exactly one failure. A target taken
/// as a float floors `1 − 0.9999` to just under one.
#[tokio::test]
async fn four_nines_over_ten_thousand_answers_allows_one_failure() {
    let store = MemoryStore::new();
    let m = meter();
    let mut r = Reporter::resume(&store, "n1", MARCH, &m).await.unwrap();
    serve(&m, A, 9_999, 1);
    r.publish(&store, &m, MARCH).await.unwrap();
    let up = rollup(&store, MARCH).await.unwrap();

    assert_eq!(
        up.budget(A, 999_900),
        Budget {
            answered: 10_000,
            failed: 1,
            allowed: 1,
            remaining: 0,
            exhausted: false,
        }
    );
}

/// A store whose next report write answers `Io`, having landed (a timeout after the store
/// committed it) or not (refused before writing), as `lands` says.
struct LandsThenFails {
    inner: MemoryStore,
    armed: AtomicBool,
    lands: bool,
}

#[async_trait::async_trait]
impl BlobStore for LandsThenFails {
    fn capabilities(&self) -> &Capabilities {
        self.inner.capabilities()
    }
    async fn get(&self, key: &Key) -> Result<Bytes, BlobError> {
        self.inner.get(key).await
    }
    async fn get_range(&self, key: &Key, range: std::ops::Range<u64>) -> Result<Bytes, BlobError> {
        self.inner.get_range(key, range).await
    }
    async fn get_suffix(&self, key: &Key, n: u64) -> Result<Bytes, BlobError> {
        self.inner.get_suffix(key, n).await
    }
    async fn get_with_tag(&self, key: &Key) -> Result<(Bytes, CasTag), BlobError> {
        self.inner.get_with_tag(key).await
    }
    async fn get_tag(&self, key: &Key) -> Result<Option<CasTag>, BlobError> {
        self.inner.get_tag(key).await
    }
    async fn head(&self, key: &Key) -> Result<u64, BlobError> {
        self.inner.head(key).await
    }
    async fn put(&self, key: &Key, body: Bytes) -> Result<PutOutcome, BlobError> {
        self.inner.put(key, body).await
    }
    async fn put_conditional(
        &self,
        key: &Key,
        body: Bytes,
        pre: Precondition,
    ) -> Result<PutOutcome, CasError> {
        let fail = key.as_str().contains("/node/") && self.armed.swap(false, Ordering::SeqCst);
        if fail && !self.lands {
            return Err(CasError::Io("refused before writing".to_owned()));
        }
        let out = self.inner.put_conditional(key, body, pre).await?;
        if fail {
            return Err(CasError::Io("timed out after the write landed".to_owned()));
        }
        Ok(out)
    }
    async fn delete_batch(&self, keys: &[Key]) -> Result<(), BlobError> {
        self.inner.delete_batch(keys).await
    }
    async fn list_unrestricted(&self, prefix: &Key) -> Result<Vec<Key>, BlobError> {
        self.inner.list_unrestricted(prefix).await
    }
}

/// Review round 1: a report write that landed and answered `Io` does not wedge the reporter,
/// and what it counted is reported exactly once.
#[tokio::test]
async fn a_write_that_landed_and_timed_out_does_not_wedge_the_reporter() {
    let store = LandsThenFails {
        inner: MemoryStore::new(),
        armed: AtomicBool::new(false),
        lands: true,
    };
    let m = meter();
    let mut r = Reporter::resume(&store, "n1", MARCH, &m).await.unwrap();
    serve(&m, A, 3, 1);
    r.publish(&store, &m, MARCH).await.unwrap();

    serve(&m, A, 2, 1);
    store.armed.store(true, Ordering::SeqCst);
    r.publish(&store, &m, MARCH)
        .await
        .expect_err("the write answered Io");

    serve(&m, A, 1, 0);
    r.publish(&store, &m, MARCH)
        .await
        .expect("the landed write was not recognised");
    assert_eq!(answers(&rollup(&store, MARCH).await.unwrap(), A), (8, 2));
    serve(&m, A, 1, 1);
    r.publish(&store, &m, MARCH).await.unwrap();
    assert_eq!(answers(&rollup(&store, MARCH).await.unwrap(), A), (10, 3));
}

/// After a write that answered `Io` without landing, the next one counts it, once. And a
/// report another process wrote in between is not taken for this one's: adopting its tag
/// would overwrite it, which criterion 5 refuses.
#[tokio::test]
async fn a_write_that_never_landed_is_counted_once_and_never_taken_for_anothers() {
    let store = LandsThenFails {
        inner: MemoryStore::new(),
        armed: AtomicBool::new(false),
        lands: false,
    };
    let m = meter();
    let mut r = Reporter::resume(&store, "n1", MARCH, &m).await.unwrap();
    serve(&m, A, 3, 1);
    r.publish(&store, &m, MARCH).await.unwrap();

    serve(&m, A, 2, 1);
    store.armed.store(true, Ordering::SeqCst);
    r.publish(&store, &m, MARCH)
        .await
        .expect_err("the write answered Io");
    assert_eq!(answers(&rollup(&store, MARCH).await.unwrap(), A), (4, 1));
    serve(&m, A, 1, 0);
    r.publish(&store, &m, MARCH).await.unwrap();
    assert_eq!(answers(&rollup(&store, MARCH).await.unwrap(), A), (8, 2));

    serve(&m, A, 1, 1);
    store.armed.store(true, Ordering::SeqCst);
    r.publish(&store, &m, MARCH)
        .await
        .expect_err("the write answered Io");
    let other = meter();
    let mut second = Reporter::resume(&store, "n1", MARCH, &other).await.unwrap();
    serve(&other, A, 100, 0);
    second.publish(&store, &other, MARCH).await.unwrap();

    let err = r
        .publish(&store, &m, MARCH)
        .await
        .expect_err("another process's report was taken for this one's");
    assert!(
        matches!(&err, ReportError::Contended(n) if n == "n1"),
        "{err}"
    );
    assert_eq!(answers(&rollup(&store, MARCH).await.unwrap(), A), (108, 2));
}
