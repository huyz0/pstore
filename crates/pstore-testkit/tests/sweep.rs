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

/// A zeroed point, so a test naming three fields is not also naming the six it does not care
/// about. M0c added the configuration a point was taken under; none of it matters here.
fn empty_point() -> sweep::Point {
    sweep::Point {
        writers: 0,
        commits_each: 0,
        attempts: 0,
        commits: 0,
        lost: 0,
        abandoned: 0,
        probe_failed: 0,
        latency_min: std::time::Duration::ZERO,
        latency_max: std::time::Duration::ZERO,
        cas_error_rate: 0.0,
        elapsed: std::time::Duration::ZERO,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sweep_reports_a_curve_not_a_point() {
    let s = Arc::new(MemoryStore::new());
    let points = sweep::contention_sweep(Arc::clone(&s), &[1, 2, 8, 32], 4)
        .await
        .unwrap();
    assert_eq!(
        points.len(),
        4,
        "a sweep is several points; one number is a guess"
    );

    // Every commit asked for is accounted: landed, or given up on after the budget.
    //
    // ⚠️ **This asserted `commits == writers * commits_each` until M0c**, which was a product
    // of the inputs rather than a fact about the run — true only while nothing could abandon
    // a commit. Since the loop stops at `MAX_CAS_ATTEMPTS`, as its caller does, a tail commit
    // at 32 writers can legitimately be abandoned and that assertion would fail
    // nondeterministically. The conservation law is what is actually true, and it is
    // **stronger**: a miscount in either field breaks it, and the product could not see one.
    for p in &points {
        assert_eq!(
            p.commits + p.abandoned,
            (p.writers * p.commits_each) as u64,
            "{} landed + {} abandoned != {} asked",
            p.commits,
            p.abandoned,
            p.writers * p.commits_each
        );
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
    let p = sweep::contention_point(s, pstore_blob::Key::new("solo"), 1, 20)
        .await
        .unwrap();
    assert_eq!(p.attempts, 20);
    assert_eq!(p.attempts, p.commits, "every attempt landed");
    // ⚠️ The uncontended case is where the give-up budget must change nothing at all.
    assert_eq!(p.abandoned, 0, "a solo writer was given up on");
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
        ..empty_point()
    };
    assert!(p.attempts_per_commit().is_infinite());
    // A ratio of exactly zero, so an epsilon comparison rather than `==` on a float.
    assert!(p.success_rate().abs() < f64::EPSILON);
    let empty = empty_point();
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
    let out = sweep::render(&sweep::contention_sweep(s, &[1, 4], 2).await.unwrap());
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
