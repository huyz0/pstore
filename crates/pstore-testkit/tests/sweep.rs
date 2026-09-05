//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism, so the
//! workspace-wide bans on unwrap/expect/panic are lifted here and only here.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! The sweep's job is to produce a curve, and to be honest that it is our curve.
use pstore_blob::MemoryStore;
use pstore_testkit::sweep;
use std::sync::Arc;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sweep_reports_a_curve_not_a_point() {
    let s = Arc::new(MemoryStore::new());
    let points = sweep::contention_sweep(Arc::clone(&s), &[1, 2, 8, 32], 4).await;
    assert_eq!(
        points.len(),
        4,
        "a sweep is several points; one number is a guess"
    );

    // Every writer must land every commit it was asked for. A protocol that livelocks
    // would hang here rather than report a low rate, which is itself the finding.
    for p in &points {
        assert_eq!(p.commits, (p.writers * p.commits_each) as u64);
        assert!(
            p.attempts >= p.commits,
            "attempts cannot be fewer than commits"
        );
    }

    // Uncontended, every attempt lands.
    let solo = points.first().unwrap();
    assert_eq!(
        solo.attempts, solo.commits,
        "one writer must never lose an attempt"
    );
    assert_eq!(solo.lost, 0);

    // Cost per commit grows with contention. This is the curve, and it is why the design
    // keeps bulk writes off the CAS path and partitions the register per tenant.
    let busy = points.last().unwrap();
    assert!(
        busy.attempts_per_commit() > solo.attempts_per_commit(),
        "32 writers cost {:.2} attempts/commit vs {:.2} for one -- contention is not \
         being exercised, so the sweep measures nothing",
        busy.attempts_per_commit(),
        solo.attempts_per_commit()
    );
    assert!(busy.lost > 0, "32 racing writers must produce some 412s");
    // The ratio is attempts/commits and nothing else: a `%` or `*` in its place would
    // still grow with contention, so the shape assertion above cannot catch it.
    let expect = busy.attempts as f64 / busy.commits as f64;
    assert!((busy.attempts_per_commit() - expect).abs() < f64::EPSILON);
    let rate = busy.commits as f64 / busy.attempts as f64;
    assert!((busy.success_rate() - rate).abs() < f64::EPSILON);
    assert!(busy.success_rate() > 0.0 && busy.success_rate() < 1.0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_single_writer_never_contends() {
    let s = Arc::new(MemoryStore::new());
    let p = sweep::contention_point(s, pstore_blob::Key::new("solo"), 1, 20).await;
    assert_eq!(p.attempts, 20);
    assert_eq!(p.attempts, p.commits, "every attempt landed");
    assert!(
        (p.success_rate() - 1.0).abs() < f64::EPSILON,
        "100%, not 20 or 400"
    );
    assert!((p.attempts_per_commit() - 1.0).abs() < f64::EPSILON);
    assert_eq!(p.lost, 0);
}

#[tokio::test]
async fn a_point_with_no_commits_is_infinite_not_a_divide_by_zero() {
    let p = sweep::Point {
        writers: 4,
        commits_each: 0,
        attempts: 9,
        commits: 0,
        lost: 9,
    };
    assert!(p.attempts_per_commit().is_infinite());
    // A ratio of exactly zero, so an epsilon comparison rather than `==` on a float.
    assert!(p.success_rate().abs() < f64::EPSILON);
    let empty = sweep::Point {
        writers: 0,
        commits_each: 0,
        attempts: 0,
        commits: 0,
        lost: 0,
    };
    assert!(
        empty.success_rate().abs() < f64::EPSILON,
        "no attempts is 0%, not NaN"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_rendered_table_carries_its_own_caveat() {
    // A number that travels without its caveat becomes a fact. This one is our protocol
    // against our own store, and the table has to say so wherever it is pasted.
    let s = Arc::new(MemoryStore::new());
    let out = sweep::render(&sweep::contention_sweep(s, &[1, 4], 2).await);
    assert!(out.contains("attempts/commit"));
    assert!(
        out.contains("PROVISIONAL"),
        "the caveat must travel with the table"
    );
    assert!(
        out.contains("M0b"),
        "it must name where the real number comes from"
    );
}
