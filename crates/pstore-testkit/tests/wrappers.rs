//! Every wrapping store must be **semantically invisible**.
//!
//! Half the stores in this workspace wrap another one to count, delay, refuse, audit or
//! gate. Each of them re-implements the whole `BlobStore` surface just to forward most of
//! it, and a forwarding method is the easiest thing in the codebase to get quietly wrong:
//! return the wrong buffer, drop a precondition, answer a suffix read from the front. A
//! wrapper that changes semantics does not fail loudly — it makes every test that uses it
//! test something other than what it claims to.
//!
//! So the conformance suite, which exists to interrogate a *backend*, is pointed at each
//! wrapper as well. The inner store is known-good, so any divergence is the wrapper's.
//!
//! Found by mutation testing: replacing a wrapper's `get_range`, `head`, `delete_batch` or
//! `list_unrestricted` with a constant left every test in the workspace green.

#![allow(
    clippy::unwrap_used,
    reason = "assertions in tests are the reporting mechanism"
)]

use pstore_blob::{Accounted, BlobStore, Congested, Faults, Faulty, MemoryStore};
use pstore_testkit::{
    audit::Auditing, conformance, depth::DepthCounting, flaky::Flaky, gated::Gated,
};
use pstore_types::TenantId;

async fn assert_transparent<S: BlobStore>(name: &str, store: S, run: u64) {
    let report = conformance::run(&store, run).await;
    assert!(
        report.conforms(),
        "{name} changed the semantics of the store it wraps: {:?}",
        report.divergences()
    );
}

#[tokio::test]
async fn the_inner_store_is_the_baseline() {
    // If this fails, every assertion below is measuring the wrong thing.
    assert_transparent("MemoryStore", MemoryStore::new(), 0).await;
}

#[tokio::test]
async fn counting_and_measuring_wrappers_are_transparent() {
    // `Accounted` counts per tenant, so the store a caller holds is its `TenantView`.
    let acc = Accounted::new(MemoryStore::new());
    assert_transparent("Accounted::TenantView", acc.as_tenant(TenantId(1)), 1).await;
    assert_transparent("DepthCounting", DepthCounting::new(MemoryStore::new()), 2).await;
}

#[tokio::test]
async fn the_auditing_store_is_transparent_to_a_conforming_caller() {
    // It refuses an unconditional overwrite of an existing key, and the suite never does
    // one -- so a *conforming* caller cannot tell it is there. That is the contract: the
    // audit is invisible until Invariant I1 is actually broken.
    assert_transparent("Auditing", Auditing::new(MemoryStore::new()), 3).await;
}

#[tokio::test]
async fn a_fault_injector_with_no_faults_is_transparent() {
    // The default has to be "does nothing". A fault injector that perturbs the store even
    // when configured not to would make every test using it subtly non-deterministic.
    assert_transparent(
        "Faulty",
        Faulty::new(MemoryStore::new(), 7, Faults::none()),
        4,
    )
    .await;
}

#[tokio::test]
async fn congestion_control_is_transparent_when_nothing_is_congested() {
    assert_transparent("Congested", Congested::new(MemoryStore::new(), 16), 5).await;
}

#[tokio::test]
async fn a_flaky_store_at_zero_rate_is_transparent() {
    assert_transparent("Flaky", Flaky::new(99, 0.0), 6).await;
    assert_transparent("Flaky::refusing(none)", Flaky::refusing(&[]), 7).await;
}

#[tokio::test]
async fn an_unarmed_gate_is_transparent() {
    // Unarmed means "not yet racing". A gate that blocked before it was armed would
    // deadlock the setup phase of every test that uses it.
    assert_transparent("Gated", Gated::new(2), 8).await;
}
