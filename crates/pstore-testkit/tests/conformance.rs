//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism, so the
//! workspace-wide bans on unwrap/expect/panic are lifted here and only here.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! The same contract, run against every backend and every decorator.
//!
//! One suite rather than per-decorator delegation tests: a decorator that forgets to
//! forward `head` is caught here, and the same suite is what will be pointed at a real
//! cloud in M0b — where a divergence between the two IS the finding.
use pstore_blob::{
    Accounted, BlobStore, Congested, Faults, Faulty, MemoryStore, Support, TagStyle,
};
use pstore_testkit::conformance;
use pstore_types::TenantId;

#[tokio::test]
async fn the_memory_store_conforms() {
    let r = conformance::run(&MemoryStore::new(), 1).await;
    assert!(r.conforms(), "divergences: {:?}", r.divergences());
    assert_eq!(r.observed.cas, Support::Supported);
    assert_eq!(r.observed.create_if_absent, Support::Supported);
}

#[tokio::test]
async fn every_decorator_conforms() {
    // A decorator that drops or mangles a method is invisible until something above it
    // depends on the method. This is that something.
    let acc = Accounted::new(MemoryStore::new());
    assert!(
        conformance::run(&acc.as_tenant(TenantId(1)), 2)
            .await
            .conforms()
    );

    let quiet = Faulty::new(MemoryStore::new(), 1, Faults::none());
    assert!(conformance::run(&quiet, 3).await.conforms());

    let calm = Congested::new(MemoryStore::new(), 8);
    assert!(conformance::run(&calm, 4).await.conforms());

    // Stacked, in the order production would use them.
    let stack = Congested::new(Faulty::new(MemoryStore::new(), 9, Faults::none()), 8);
    assert!(conformance::run(&stack, 5).await.conforms());
}

#[tokio::test]
async fn the_suite_detects_a_backend_that_is_not_fenced() {
    // The finding the suite exists to produce. A content-hash backend accepts a stale tag
    // after v1 -> v2 -> v1, so `compare_and_swap` must come back Divergent -- recorded,
    // not assumed, and a backend recorded this way must refuse `durable` writes.
    let r = conformance::run(&MemoryStore::with_tag_style(TagStyle::ContentHash), 6).await;
    assert!(
        !r.conforms(),
        "an ABA-prone backend must not be reported as conforming"
    );
    // Basic fencing still passes -- a stale tag against DIFFERENT content is refused.
    // It is ABA resistance that fails, and separating the two probes is what makes the
    // difference between an S3 ETag and a GCS generation visible in the profile.
    let cas = r
        .probes
        .iter()
        .find(|p| p.name == "compare_and_swap")
        .unwrap();
    assert_eq!(cas.outcome, Support::Supported);
    let aba = r
        .probes
        .iter()
        .find(|p| p.name == "aba_resistance")
        .unwrap();
    assert!(
        matches!(&aba.outcome, Support::Divergent(m) if m.contains("ABA")),
        "got {:?}",
        aba.outcome
    );
    // ⚠️ And the backend still DECLARES support. Measured beats declared, which is the
    // whole reason capabilities are probed rather than read.
    assert_eq!(
        MemoryStore::with_tag_style(TagStyle::ContentHash)
            .capabilities()
            .cas,
        Support::Supported
    );
}

#[tokio::test]
async fn a_report_names_what_diverged() {
    let r = conformance::run(&MemoryStore::with_tag_style(TagStyle::ContentHash), 7).await;
    let d = r.divergences();
    assert_eq!(d.len(), 1, "expected exactly one divergence, got {d:?}");
    assert!(!r.backend.is_empty());
    assert_eq!(r.probes.len(), 8);
}

#[tokio::test]
async fn conformance_costs_a_bounded_number_of_requests() {
    // The suite runs against a real cloud in M0b, where every probe is billed.
    use pstore_blob::OpClass;
    let acc = Accounted::new(MemoryStore::new());
    let t = TenantId(42);
    conformance::run(&acc.as_tenant(t), 8).await;
    let total =
        acc.count(t, OpClass::Read) + acc.count(t, OpClass::Write) + acc.count(t, OpClass::Delete);
    assert!(total < 40, "the suite issued {total} requests");
    assert_eq!(
        acc.count(t, OpClass::List),
        0,
        "conformance must never LIST"
    );
}
