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
use pstore_testkit::{
    audit::Auditing, conformance, depth::DepthCounting, flaky::Flaky, gated::Gated,
};
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

    // The measuring decorators. `DepthCounting` in particular reimplements the whole
    // trait to time each call, so every method is a hand-written forward.
    let depth = DepthCounting::new(MemoryStore::new());
    assert!(conformance::run(&depth, 7).await.conforms());

    // The auditing store refuses an unconditional overwrite of an existing key, and the
    // suite never does one -- so a CONFORMING caller cannot tell it is there. That is its
    // contract: invisible until Invariant I1 is actually broken.
    let audit = Auditing::new(MemoryStore::new());
    assert!(conformance::run(&audit, 8).await.conforms());

    // The fault injectors, configured to inject nothing. A "no faults" setting that still
    // perturbs the store would make every test built on it quietly non-deterministic.
    assert!(conformance::run(&Flaky::new(99, 0.0), 9).await.conforms());
    assert!(conformance::run(&Flaky::refusing(&[]), 10).await.conforms());

    // Unarmed means "not yet racing". A gate that blocked before it was armed would
    // deadlock the setup phase of every test that uses it.
    assert!(conformance::run(&Gated::new(2), 11).await.conforms());
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
    assert_eq!(r.probes.len(), 9);
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

#[tokio::test]
async fn the_suite_detects_each_defect_a_real_backend_actually_has() {
    use pstore_testkit::broken::{Broken, Defect};

    // Until something is deliberately divergent, the suite's DETECTION is untested: it
    // could report Supported unconditionally and every other test would still pass.
    let cases = [
        (Defect::IgnoresCreateIfAbsent, "create_if_absent"),
        (Defect::ShortReadsPastTheEnd, "range_past_end_is_an_error"),
        (Defect::SuffixReturnsEverything, "suffix_read"),
        (Defect::IgnoresDeletes, "batch_delete"),
    ];
    for (i, (defect, probe)) in cases.into_iter().enumerate() {
        let s = Broken::new(defect);
        let r = conformance::run(&s, 900 + i as u64).await;
        assert!(!r.conforms(), "{defect:?} was reported as conforming");
        let p = r.probes.iter().find(|p| p.name == probe).unwrap();
        assert!(
            matches!(p.outcome, Support::Divergent(_)),
            "{defect:?} should have shown up on {probe}, got {:?}",
            p.outcome
        );
        // And exactly one probe should fail: a defect that trips several means the probes
        // are not independent, and the report would not say what is actually wrong.
        assert_eq!(
            r.divergences().len(),
            1,
            "{defect:?} tripped {:?}",
            r.divergences()
        );
    }
}

#[tokio::test]
async fn a_broken_backend_still_declares_itself_healthy() {
    use pstore_testkit::broken::{Broken, Defect};
    // The gap the whole module exists to demonstrate.
    let s = Broken::new(Defect::IgnoresCreateIfAbsent);
    assert_eq!(s.capabilities().create_if_absent, Support::Supported);
    let r = conformance::run(&s, 950).await;
    assert!(matches!(r.observed.create_if_absent, Support::Divergent(_)));
}

#[tokio::test]
async fn a_broken_backend_forwards_everything_it_does_not_break() {
    use pstore_blob::Key;
    use pstore_testkit::broken::{Broken, Defect};
    // One defect means ONE defect. A stand-in that quietly broke a second method would
    // make the "exactly one divergence" assertion above meaningless.
    let s = Broken::new(Defect::IgnoresDeletes);
    let k = Key::new("a/b");
    s.put(&k, bytes::Bytes::from_static(b"0123456789"))
        .await
        .unwrap();
    assert_eq!(&s.get(&k).await.unwrap()[..], b"0123456789");
    assert_eq!(&s.get_range(&k, 2..5).await.unwrap()[..], b"234");
    assert_eq!(&s.get_suffix(&k, 3).await.unwrap()[..], b"789");
    assert_eq!(s.head(&k).await.unwrap(), 10);
    assert!(s.get_tag(&k).await.is_some());
    assert_eq!(s.get_with_tag(&k).await.unwrap().0.len(), 10);
    assert_eq!(s.list_unrestricted(&Key::new("a")).await.unwrap().len(), 1);
    // ...and the one it does break.
    s.delete_batch(std::slice::from_ref(&k)).await.unwrap();
    assert!(
        s.get(&k).await.is_ok(),
        "the defect is that deletes do nothing"
    );
}
