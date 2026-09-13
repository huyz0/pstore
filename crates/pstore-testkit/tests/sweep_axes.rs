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
async fn reads_and_slow_downs_never_reach_the_commit_loop() {
    // ⚠️ Pins a gap rather than a behaviour, so a later claim that the axis covers "errors"
    // has to face it. The loop's only read is `get_tag`, which returns `Option` and therefore
    // has no error channel at all; `put_conditional` consults only the CAS classes.
    //
    // ⚠️ **One writer, deliberately.** The first version of this test raced two writers and
    // compared the totals against a clean run -- which compares two samples of a scheduler,
    // not two fault configurations, and failed against correct code. Uncontended, the
    // expected point is exact: four attempts, four commits, nothing abandoned.
    let noisy = seeded(
        &Key::new("k"),
        Faults {
            read_error: 0.9,
            write_error: 0.9,
            slow_down: 0.9,
            ..Faults::none()
        },
    )
    .await;
    let p = sweep::contention_point(noisy, Key::new("k"), 1, 4)
        .await
        .unwrap();
    assert_eq!(
        (p.attempts, p.commits, p.abandoned, p.lost),
        (4, 4, 0, 0),
        "a read, write or 503 fault reached the loop -- if that is now true, the axis can be \
         widened and this test should be replaced rather than deleted"
    );
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
