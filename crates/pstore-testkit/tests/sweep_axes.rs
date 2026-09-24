//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! The two axes D-101 asked for and M0a never ran — M0c.

use pstore_blob::{BlobStore, Faults, Faulty, Key, MemoryStore, Precondition};
use pstore_testkit::sweep::{self, MAX_CAS_ATTEMPTS, SweepError};
use std::sync::Arc;
use std::time::Duration;

/// A store whose key exists, wrapped in injection afterwards.
async fn seeded(key: &Key, faults: Faults) -> Arc<Faulty<MemoryStore>> {
    let raw = MemoryStore::new();
    raw.put_conditional(
        key,
        bytes::Bytes::from_static(b"0"),
        Precondition::NotExists,
    )
    .await
    .ok();
    Arc::new(Faulty::new(raw, 11, faults))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_asked_commit_is_either_landed_or_abandoned() {
    // ⚠️ The invariant that replaces `commits == writers * commits_each`. That was a product
    // of the inputs; this is a conservation law, and it fails on a miscount in either field.
    //
    // ⚠️ **0.97, not 0.6, and the reason is that 0.6 abandoned nothing.** Measured over ten
    // runs at 0.6, `abandoned == 0` every time, so the sum degenerated into exactly the
    // product it replaced and restoring that product left this test green. A conservation law
    // over a run where nothing is conserved is not a test.
    let s = seeded(
        &Key::new("k"),
        Faults {
            cas_lost: 0.97,
            ..Faults::none()
        },
    )
    .await;
    let p = sweep::contention_point(s, Key::new("k"), 4, 4)
        .await
        .unwrap();
    assert!(
        p.abandoned > 0,
        "nothing was abandoned at a 97% refusal rate, so this asserts the product it replaced"
    );
    assert!(
        p.commits > 0,
        "nothing landed either -- the fixture then measures only the budget"
    );
    assert_eq!(
        p.commits + p.abandoned,
        (p.writers * p.commits_each) as u64,
        "{} landed and {} abandoned does not account for {} asked",
        p.commits,
        p.abandoned,
        p.writers * p.commits_each
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn at_total_cas_loss_nothing_lands_and_the_budget_is_spent() {
    // ⚠️ Asserted as a SHAPE inside a timeout, not as a hang. An unbounded loop here does not
    // fail this test, it never finishes it -- and a mutation that is scored a timeout rather
    // than a kill is a mutation the gate did not catch.
    let s = seeded(
        &Key::new("k"),
        Faults {
            cas_lost: 1.0,
            ..Faults::none()
        },
    )
    .await;
    let p = tokio::time::timeout(
        Duration::from_secs(30),
        sweep::contention_point(s, Key::new("k"), 2, 3),
    )
    .await
    .expect("the sweep did not terminate: the give-up budget is not being applied")
    .unwrap();

    assert_eq!(p.commits, 0, "a commit landed at a 100% refusal rate");
    assert_eq!(p.abandoned, 6);
    assert_eq!(
        p.attempts,
        6 * u64::from(MAX_CAS_ATTEMPTS),
        "each of the 6 commits must spend exactly the budget before giving up"
    );
    assert!(p.attempts_per_commit().is_infinite());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_point_on_an_unseeded_key_is_refused_not_reported() {
    // ⚠️ Measured before this existed: seeded THROUGH an injecting store at rate 1.0, the seed
    // never lands and the point came back `attempts: 0, commits: 4` -- four commits reported
    // on zero attempts. Returning a row is worse than returning nothing, because a row travels.
    let s = Arc::new(Faulty::new(
        MemoryStore::new(),
        11,
        Faults {
            cas_lost: 1.0,
            ..Faults::none()
        },
    ));
    let err = sweep::contention_point(s, Key::new("never"), 2, 2)
        .await
        .expect_err("a point was reported for a key that was never created");
    assert!(matches!(err, SweepError::Unseeded(_)), "{err}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_point_under_read_errors_reports_the_probes_that_failed() {
    // ⚠️ **This replaces `reads_and_slow_downs_never_reach_the_commit_loop`, on that test's
    // own terms.** It pinned a gap — "the loop's only read is `get_tag`, which returns
    // `Option` and therefore has no error channel at all" — and said in as many words that
    // if the gap ever closed it should be *replaced rather than deleted*. M7b closed it:
    // `get_tag` is fallible and `Faulty` injects into it, so a read fault now reaches the
    // rebase. Backlog row 21.
    //
    // ⚠️ **One writer, deliberately**, for the reason the replaced test gave: two writers
    // compare two samples of a scheduler rather than two fault configurations. The clean arm
    // is still exactly four attempts and four commits, which is what makes the noisy arm's
    // difference attributable to the faults.
    let clean = seeded(&Key::new("k"), Faults::none()).await;
    let c = sweep::contention_point(clean, Key::new("k"), 1, 4)
        .await
        .unwrap();
    assert_eq!(
        (c.attempts, c.commits, c.abandoned, c.lost, c.probe_failed),
        (4, 4, 0, 0, 0),
        "a clean run reported a failed probe"
    );

    // ⚠️ **M0c's exact configuration now refuses the point rather than reporting it**, which
    // is the first half of the same finding: the seed probe is a read, and at 0.9 it is
    // refused. Before M7b this configuration returned a row byte-identical to the clean one.
    let hostile = seeded(
        &Key::new("k"),
        Faults {
            read_error: 0.9,
            write_error: 0.9,
            slow_down: 0.9,
            ..Faults::none()
        },
    )
    .await;
    assert!(
        matches!(
            sweep::contention_point(hostile, Key::new("k"), 1, 4).await,
            Err(SweepError::Probe { .. })
        ),
        "M0c's configuration still produced a point, so the probe has no error channel again"
    );

    // ⚠️ And at a rate the seed survives, the loop itself reports the refusals. 0.25, and the
    // ceiling is not arbitrary: the point has to be **taken** to be read, and the seed probe
    // is itself a read. At seed 11 the first probe is refused at 0.3 and above, so this test
    // would abort with `Probe` before reaching the loop it measures. `Faulty` is deterministic
    // per seed, so that is a fixed property of this fixture rather than flakiness -- and a
    // fault stream that changes turns it into a loud `unwrap` on `Probe`, which is the right
    // failure.
    const RATE: f64 = 0.25;
    let noisy = seeded(
        &Key::new("k"),
        Faults {
            read_error: RATE,
            ..Faults::none()
        },
    )
    .await;
    let p = sweep::contention_point(noisy, Key::new("k"), 1, 4)
        .await
        .unwrap();
    // ⚠️ M0c measured a point **byte-identical** to the clean one under injected reads. That
    // is the finding this milestone exists to close, so the assertion is a difference rather
    // than a threshold.
    assert!(
        p.probe_failed > 0,
        "at read_error {RATE} no probe was refused -- the point is {p:?}, and if it is still \
         identical to the clean one the error channel is being swallowed somewhere"
    );
    // ⚠️ And `probe_failed` is a **subset** of `abandoned`, not a second name for it: a
    // counter incremented in the `Ok(None)` arm would pass the assertion above and fail here,
    // because an absent key abandons without a refusal.
    assert!(
        p.probe_failed <= p.abandoned,
        "more refused probes ({}) than abandoned rebases ({})",
        p.probe_failed,
        p.abandoned
    );
    assert_eq!(
        p.commits + p.abandoned,
        4,
        "the conservation law broke under refused probes: {p:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unseeded_key_and_an_unreadable_one_are_different_errors() {
    // ⚠️ Both abort the point, so collapsing them costs nothing at the call site — which is
    // exactly why they were the same thing until M7b. "The seed did not land" and "the store
    // refused to tell me" have different remedies, and only one of them is about contention.
    let unreadable = seeded(
        &Key::new("k"),
        Faults {
            read_error: 1.0,
            ..Faults::none()
        },
    )
    .await;
    let err = sweep::contention_point(unreadable, Key::new("k"), 2, 2)
        .await
        .expect_err("a point was taken through a store that refused every read");
    assert!(
        matches!(err, SweepError::Probe { .. }),
        "a refused seed probe was reported as {err}"
    );

    // ⚠️ The key is genuinely absent here: `cas_lost` refuses the seeding write and leaves
    // every READ clean, so the probe answers "nothing is there" rather than failing. Taken
    // from the test this arm replaces, `a_point_on_an_unseeded_key_is_refused_not_reported`,
    // because the distinction only means something against a seed that really did not land.
    let absent = Arc::new(Faulty::new(
        MemoryStore::new(),
        11,
        Faults {
            cas_lost: 1.0,
            ..Faults::none()
        },
    ));
    let err = sweep::contention_point(absent, Key::new("never"), 2, 2)
        .await
        .expect_err("a point was reported for a key that was never created");
    assert!(matches!(err, SweepError::Unseeded(_)), "{err}");
}

#[tokio::test(start_paused = true)]
async fn latency_is_paid_inside_the_commit_loop() {
    // ⚠️ `start_paused` needs the current-thread flavour, so this is one writer and no
    // contention -- which is all this assertion needs, and it costs the suite no real time.
    // The multi-writer latency curve is the example's, deliberately outside `cargo test`.
    let key = Key::new("k");
    let raw = MemoryStore::new();
    raw.put_conditional(
        &key,
        bytes::Bytes::from_static(b"0"),
        Precondition::NotExists,
    )
    .await
    .ok();
    let s = Arc::new(Faulty::new(
        raw,
        11,
        Faults {
            latency_min: Duration::from_millis(30),
            latency_max: Duration::from_millis(30),
            ..Faults::none()
        },
    ));
    let p = sweep::contention_point(s, key, 1, 4).await.unwrap();
    assert_eq!(p.commits, 4);
    // 4 commits x (one get_tag + one put_conditional) x 30 ms. The seeding probe is outside
    // the window: `started` is captured after it.
    assert!(
        p.elapsed >= Duration::from_millis(8 * 30),
        "a 30 ms round trip cost the loop {:?} -- the delay is configured and never awaited",
        p.elapsed
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_table_carries_every_column_and_its_caveat() {
    let s = seeded(
        &Key::new("k"),
        Faults {
            cas_lost: 1.0,
            ..Faults::none()
        },
    )
    .await;
    // ⚠️ Bounded like its sibling. This also takes a point at a 100% refusal rate, so without
    // the budget it does not fail -- it runs forever, and a mutation scored a timeout rather
    // than a kill is a mutation nothing caught.
    let p = tokio::time::timeout(
        Duration::from_secs(30),
        sweep::contention_point(s, Key::new("k"), 1, 1),
    )
    .await
    .expect("the give-up budget is not being applied")
    .unwrap();
    let out = sweep::render(&[p]);
    for col in ["latency", "412/409", "aband", "wall", "attempts/commit"] {
        assert!(
            out.contains(col),
            "the {col} column was dropped from the table"
        );
    }
    assert!(out.contains("PROVISIONAL") && out.contains("M0b"));
    assert!(
        out.contains("<- at the give-up budget"),
        "a row at the budget must say so in the row, not only under the table"
    );
    // ⚠️ The footer, asserted on words that appear nowhere else in the output. Checking for
    // "16" matched the 16 in the attempts column of the data row, and "give-up budget" matched
    // the row flag above -- so deleting the entire footer left this test green.
    assert!(
        out.contains("attempts per commit, the same constant"),
        "the footer naming the budget and where it comes from was dropped"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn each_swept_point_records_the_configuration_it_was_taken_under() {
    // ⚠️ The two sweep entry points this milestone is named for had no test at all: they are
    // called only from the example, so the assignments that fill the latency and 412/409
    // columns were unasserted. Deleting them zeroes the two columns the results are read off.
    let spreads = [
        (Duration::ZERO, Duration::ZERO),
        (Duration::from_millis(1), Duration::from_millis(3)),
    ];
    let points = sweep::latency_sweep(&spreads, 2, 1, 5).await.unwrap();
    assert_eq!(
        points
            .iter()
            .map(|p| (p.latency_min, p.latency_max))
            .collect::<Vec<_>>(),
        spreads.to_vec(),
        "a point does not carry the spread it was taken under"
    );

    let rates = [0.0, 0.5];
    let points = sweep::cas_error_sweep(&rates, 2, 1, 5).await.unwrap();
    let got: Vec<f64> = points.iter().map(|p| p.cas_error_rate).collect();
    assert_eq!(
        got,
        rates.to_vec(),
        "a point does not carry its refusal rate"
    );
    // And these sweeps seed outside the injection, which is what makes a high rate measurable
    // rather than a row of zeroes that claims its commits landed.
    assert!(points.iter().all(|p| p.commits + p.abandoned == 2));
}

/// A correct store whose `get_tag` answers the seed probe and then reports the key missing.
///
/// The store contradicting itself: `contention_point` seeds the key and nothing deletes it,
/// so `Ok(None)` inside the loop can only come from a backend like this one.
#[derive(Debug, Default)]
struct ForgetsAfterTheProbe {
    inner: MemoryStore,
    probes: std::sync::atomic::AtomicUsize,
}

#[async_trait::async_trait]
impl BlobStore for ForgetsAfterTheProbe {
    fn capabilities(&self) -> &pstore_blob::Capabilities {
        self.inner.capabilities()
    }
    async fn get(&self, key: &Key) -> Result<bytes::Bytes, pstore_blob::BlobError> {
        self.inner.get(key).await
    }
    async fn get_range(
        &self,
        key: &Key,
        range: std::ops::Range<u64>,
    ) -> Result<bytes::Bytes, pstore_blob::BlobError> {
        self.inner.get_range(key, range).await
    }
    async fn get_suffix(&self, key: &Key, n: u64) -> Result<bytes::Bytes, pstore_blob::BlobError> {
        self.inner.get_suffix(key, n).await
    }
    async fn get_with_tag(
        &self,
        key: &Key,
    ) -> Result<(bytes::Bytes, pstore_types::CasTag), pstore_blob::BlobError> {
        self.inner.get_with_tag(key).await
    }
    async fn get_tag(
        &self,
        key: &Key,
    ) -> Result<Option<pstore_types::CasTag>, pstore_blob::BlobError> {
        let tag = self.inner.get_tag(key).await?;
        let first = self
            .probes
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            == 0;
        Ok(tag.filter(|_| first))
    }
    async fn head(&self, key: &Key) -> Result<u64, pstore_blob::BlobError> {
        self.inner.head(key).await
    }
    async fn put(
        &self,
        key: &Key,
        body: bytes::Bytes,
    ) -> Result<pstore_blob::PutOutcome, pstore_blob::BlobError> {
        self.inner.put(key, body).await
    }
    async fn put_conditional(
        &self,
        key: &Key,
        body: bytes::Bytes,
        pre: Precondition,
    ) -> Result<pstore_blob::PutOutcome, pstore_blob::CasError> {
        self.inner.put_conditional(key, body, pre).await
    }
    async fn delete_batch(&self, keys: &[Key]) -> Result<(), pstore_blob::BlobError> {
        self.inner.delete_batch(keys).await
    }
    async fn list_unrestricted(&self, prefix: &Key) -> Result<Vec<Key>, pstore_blob::BlobError> {
        self.inner.list_unrestricted(prefix).await
    }
}

#[tokio::test]
async fn each_early_exit_abandons_exactly_one_commit() {
    // The two exits no other test reaches. `abandoned` is what makes `commits` honest, so
    // each exit must count exactly one -- and only its own kind.

    // A refused CAS (`Io`). Ordinal 0 is the seed, so the writer's first CAS is refused.
    let p = sweep::contention_point(
        Arc::new(pstore_testkit::flaky::Flaky::refusing(&[1])),
        Key::new("k"),
        1,
        1,
    )
    .await
    .unwrap();
    assert_eq!(
        (p.attempts, p.commits, p.lost, p.abandoned, p.probe_failed),
        (1, 0, 0, 1, 0),
        "a refused CAS"
    );

    // A rebase that finds the seeded key missing (`Ok(None)`): abandoned, and NOT a refused
    // probe, and before any attempt is spent.
    let p = sweep::contention_point(
        Arc::new(ForgetsAfterTheProbe::default()),
        Key::new("k"),
        1,
        1,
    )
    .await
    .unwrap();
    assert_eq!(
        (p.attempts, p.commits, p.lost, p.abandoned, p.probe_failed),
        (0, 0, 0, 1, 0),
        "a key the store forgot"
    );
}

#[tokio::test(start_paused = true)]
async fn a_latency_sweep_runs_the_spread_it_records() {
    // ⚠️ A spread, not a fixed delay. `Faulty` clamps `max` up to `min`, so at `lo == hi`
    // a sweep that dropped `latency_max` would delay by `lo` anyway and pass. One writer and
    // four commits is 8 delayed calls (a rebase read and a CAS each; the seed probe is
    // outside the clock), so the loop costs more than 80 ms and at most 88 -- exactly 80 with
    // the ceiling dropped, and around half that with the floor dropped. ⚠️ AT MOST 88, not
    // under it: the paused clock rounds every timer up to a whole millisecond, so each 10.x ms
    // delay costs exactly 11 and correct code lands on 88.
    let lo = Duration::from_millis(10);
    let hi = Duration::from_millis(11);
    let points = sweep::latency_sweep(&[(lo, hi)], 1, 4, 5).await.unwrap();
    let p = &points[0];
    assert_eq!(p.commits, 4);
    assert!(
        p.elapsed > 8 * lo && p.elapsed <= 8 * hi,
        "8 calls at [10, 11) ms took {:?}",
        p.elapsed
    );
}

#[tokio::test]
async fn a_cas_error_sweep_runs_the_rate_it_records() {
    // At a 100% 412 rate nothing lands and the whole budget is spent on the one commit. A
    // sweep that recorded the rate without injecting it would land the commit first try.
    let points = sweep::cas_error_sweep(&[1.0], 1, 1, 5).await.unwrap();
    let p = &points[0];
    assert_eq!(
        (p.commits, p.lost, p.abandoned),
        (0, u64::from(MAX_CAS_ATTEMPTS), 1)
    );
}
