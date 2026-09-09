//! Criteria 7 and 8: a backend that cannot fence is refused, before it is written to.
//!
//! ⚠️ Three documents say this must happen and, until this milestone, nothing did it. The
//! failure is not subtle once it lands — two writers both believe they created the tenant —
//! but it is completely invisible beforehand, because the in-process store's CAS is correct
//! and every functional test runs on it.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "assertions in tests are the reporting mechanism"
)]

use pstore_blob::{Accounted, OpClass};
use pstore_engine::{Engine, EngineError};
use pstore_format::Document;
use pstore_testkit::claims::Claims;
use pstore_types::{LaneId, TenantId};
use std::sync::Arc;

const T: TenantId = TenantId(1);

fn doc(id: u64) -> Document {
    Document::new(id.to_string(), vec![1.0, 0.0])
}

fn refusal(e: EngineError) -> String {
    let s = e.to_string();
    assert!(
        matches!(e, EngineError::BackendCannotFence { .. }),
        "expected a fencing refusal, got {s}"
    );
    s
}

#[tokio::test]
async fn a_divergent_backend_is_refused_before_anything_is_written() {
    let acc = Accounted::new(Claims::divergent_cas("wildcard ignored"));
    let e = Engine::new(Arc::new(acc.as_tenant(T)), T, LaneId(0));

    // `write` is buffered and visible, never durable, so it is deliberately NOT refused.
    e.write("i", vec![doc(1)]).await.unwrap();

    // ⚠️ Every door that acknowledges durability, not just the obvious one. `fold` and
    // `compact` reach `head::commit` without ever flushing, and `gc` DELETES before it
    // commits -- so a guard at the CAS alone would let it destroy objects and then refuse.
    refusal(e.flush().await.expect_err("flush must refuse"));
    refusal(e.fold().await.expect_err("fold must refuse"));
    refusal(e.compact("i").await.expect_err("compact must refuse"));
    refusal(e.gc(0).await.expect_err("gc must refuse"));

    for class in [
        OpClass::Write,
        OpClass::Read,
        OpClass::Delete,
        OpClass::List,
    ] {
        assert_eq!(
            acc.total(class),
            0,
            "{class:?} requests were issued by a refused operation"
        );
    }
}

#[tokio::test]
async fn a_refused_gc_deletes_nothing() {
    // ⚠️ `gc` is the door whose guard cannot be inferred from the others: it `delete_batch`es
    // BEFORE it commits, so a guard only at the CAS lets it destroy objects and then refuse.
    // The fixture has to give it something to reap, which means a world some conforming
    // writer built and a profile that has since been re-probed as divergent.
    let good = Claims::conforming();
    let inner = good.store();
    let e = Engine::new(Arc::new(good), T, LaneId(0));
    e.write("i", vec![doc(1)]).await.unwrap();
    e.flush().await.unwrap();
    e.fold().await.unwrap();
    e.write("i", vec![doc(2)]).await.unwrap();
    e.flush().await.unwrap();
    e.fold().await.unwrap();
    let before = e.head_for_test().await;
    assert!(
        !before.graveyard.is_empty(),
        "the fixture must actually have something to reap"
    );

    let acc = Accounted::new(Claims::degraded(inner, "wildcard ignored"));
    let degraded = Engine::new(Arc::new(acc.as_tenant(T)), T, LaneId(0));
    refusal(degraded.gc(0).await.expect_err("gc must refuse"));
    assert_eq!(
        acc.total(OpClass::Delete),
        0,
        "a refused gc deleted objects"
    );
    assert_eq!(acc.total(OpClass::Write), 0);
    assert_eq!(acc.total(OpClass::Read), 0);
}

#[tokio::test]
async fn the_refusal_names_the_backend_and_the_primitive() {
    let e = Engine::new(
        Arc::new(Claims::divergent_cas("accepts the wildcard and ignores it")),
        T,
        LaneId(0),
    );
    let msg = refusal(e.flush().await.expect_err("must refuse"));
    // A message that says only "unsupported backend" leaves an operator with nothing to do.
    assert!(msg.contains("emulator-under-test"), "{msg}");
    assert!(msg.contains("compare_and_swap"), "{msg}");
}

#[tokio::test]
async fn create_if_absent_alone_is_enough_to_refuse() {
    // Lane objects and the tenant create both rest on it, and MinIO's documented divergence
    // is on this primitive rather than on `If-Match`.
    let e = Engine::new(
        Arc::new(Claims::divergent_create("wildcard ignored")),
        T,
        LaneId(0),
    );
    e.write("i", vec![doc(1)]).await.unwrap();
    let msg = refusal(e.flush().await.expect_err("must refuse"));
    assert!(msg.contains("create_if_absent"), "{msg}");
}

#[tokio::test]
async fn a_commit_reached_past_the_doors_is_refused() {
    // ⚠️ The doors are what make the refusal free; this is what stops a path added later
    // from committing around them. `commit_head_for_test` is exactly such a path.
    let e = Engine::new(
        Arc::new(Claims::divergent_cas("wildcard ignored")),
        T,
        LaneId(0),
    );
    refusal(
        e.commit_head_for_test(|_| {})
            .await
            .expect_err("a direct commit must refuse"),
    );
    refusal(
        e.commit_stale_for_test()
            .await
            .expect_err("a stale commit must refuse"),
    );
}

#[tokio::test]
async fn a_lane_registration_on_a_divergent_backend_is_refused() {
    // ⚠️ **The only test that can see this guard.** `lanes::register`'s one caller in the
    // engine is `flush`, which has already refused at its own door — so deleting the guard
    // leaves the whole suite green, and a mutation sweep cannot see it either, because its
    // mutants replace the body and the conforming-store tests kill those. Reached directly,
    // the way a second caller added later would reach it.
    let store = Claims::divergent_cas("wildcard ignored");
    let err = pstore_engine::lanes::register(&store, T, LaneId(3))
        .await
        .expect_err("a lane registration must refuse");
    let msg = refusal(err);
    assert!(msg.contains("emulator-under-test"), "{msg}");

    // And it wrote nothing: the registry is a CAS'd object, so a half-registered lane is a
    // lane whose writes are unrecoverable.
    assert!(
        pstore_engine::lanes::live(&Claims::conforming(), T)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn a_conforming_backend_is_untouched() {
    // Criterion 9 in miniature; the real defence is the rest of the workspace suite.
    let e = Engine::new(Arc::new(Claims::conforming()), T, LaneId(0));
    e.write("i", vec![doc(1)]).await.unwrap();
    assert!(e.flush().await.unwrap().is_some());
    e.fold().await.unwrap();
    e.gc(0).await.unwrap();
}
