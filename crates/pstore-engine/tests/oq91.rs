//! **OQ-91.** Does recovery find every acknowledged record?
//!
//! Cross-index bundling rests on one claim: a bundle written to a lane is recoverable by
//! *any* node, without being told the lane exists or how far it got. If that claim fails,
//! the whole write path fails with it — so this file tries to break it rather than to
//! confirm it, across many seeds, with writers dying, successors taking over, and writes
//! falling back to a different lane.
//!
//! The contract under test is precise, and the precision is what makes it falsifiable:
//!
//! - **Acknowledged** means `flush()` returned `Ok`. A refused flush is not acknowledged
//!   and may be lost; asserting otherwise would be asserting that the store never fails.
//! - **Found** means a fresh engine, holding no memory of any writer, that discovers the
//!   lanes from the registry and their tails by probing, folds, and sees the row.
//! - **Exactly once.** A duplicate is as much a failure as a loss: folding a bundle twice
//!   double-counts on every aggregate built above it.

#![allow(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

use pstore_blob::BlobStore;
use pstore_engine::Engine;
use pstore_format::Document;
use pstore_testkit::{flaky::Flaky, sim::Sim};
use pstore_types::{LaneId, TenantId};
use std::collections::BTreeSet;
use std::sync::Arc;

fn doc(id: &str) -> Document {
    Document {
        id: id.to_owned(),
        vector: vec![0.0; 4],
        attrs: Default::default(),
    }
}

/// One writer's turn in the scenario.
#[derive(Debug, Clone, Copy)]
enum Act {
    Write,
    Flush,
    Fold,
    /// The writer stops, permanently, without cleaning up. Its lane keeps whatever it
    /// managed to acknowledge, and nobody is told.
    Die,
}

struct Outcome {
    acked: BTreeSet<String>,
    deaths: usize,
    lanes_used: usize,
}

/// Runs one seeded scenario and returns what was acknowledged.
async fn scenario<S: BlobStore>(store: &Arc<S>, tenant: TenantId, seed: u64) -> Outcome {
    let mut sim = Sim::new(seed);
    let mut acked: BTreeSet<String> = BTreeSet::new();
    let mut next_lane: u64 = 0;
    let mut deaths = 0usize;
    let mut lanes_used = 0usize;

    // Three logical writers. Each is an Engine bound to a lane; when one dies its
    // successor gets a NEW lane, which is the fallback path -- the same logical stream
    // continuing on a different lane, with the old one abandoned mid-flight.
    let mut live: Vec<(usize, Engine<S>)> = Vec::new();
    for w in 0..3usize {
        live.push((w, Engine::new(Arc::clone(store), tenant, LaneId(next_lane))));
        next_lane += 1;
        lanes_used += 1;
    }

    let mut doc_no = 0u64;
    for _ in 0..60 {
        if live.is_empty() {
            break;
        }
        let i = sim.choose(live.len());
        let act = match sim.choose(10) {
            0..=3 => Act::Write,
            4..=6 => Act::Flush,
            7..=8 => Act::Fold,
            _ => Act::Die,
        };
        match act {
            Act::Write => {
                let id = format!("w{}-d{}", live[i].0, doc_no);
                doc_no += 1;
                // A write only enters the memtable; nothing is acknowledged yet.
                live[i].1.write("idx", vec![doc(&id)]).await.unwrap();
            }
            Act::Flush => {
                let before = live[i].1.pending_for_test().await;
                // ⚠️ The acknowledgement rule, and the only place `acked` grows. A flush
                // that returns `Err` acknowledges NOTHING, so its rows are not claimed
                // and their loss is not a failure of this test.
                if live[i].1.flush().await.is_ok() {
                    acked.extend(before);
                }
            }
            Act::Fold => {
                // Folding may fail on an injected write; that is a retry, not a loss.
                let _ = live[i].1.fold().await;
            }
            Act::Die => {
                let (w, _) = live.swap_remove(i);
                deaths += 1;
                // The successor: same logical writer, brand new lane, no memory of what
                // its predecessor buffered or how far its lane got.
                live.push((w, Engine::new(Arc::clone(store), tenant, LaneId(next_lane))));
                next_lane += 1;
                lanes_used += 1;
            }
        }
    }
    Outcome {
        acked,
        deaths,
        lanes_used,
    }
}

#[tokio::test]
async fn recovery_finds_every_unfolded_record_across_seeds() {
    let mut total_acked = 0usize;
    let mut total_deaths = 0usize;
    let mut total_injected = 0u64;

    for seed in 0..64u64 {
        let store = Arc::new(Flaky::new(seed, 0.15));
        let tenant = TenantId(u128::from(seed) + 1);
        let out = scenario(&store, tenant, seed).await;
        total_acked += out.acked.len();
        total_deaths += out.deaths;
        total_injected += store.failures();

        // Recovery: a node that was never part of the scenario. It is told the tenant and
        // nothing else -- not which lanes exist, not how far any of them got.
        let recovered = Engine::new(Arc::clone(&store), tenant, LaneId(9_999));
        // Retried because the store is still refusing writes; a fold that cannot commit
        // is a retry, and only a fold that never succeeds is a failure.
        let mut folded = false;
        for _ in 0..50 {
            if recovered.fold().await.is_ok() {
                folded = true;
                break;
            }
        }
        assert!(folded, "seed {seed}: recovery could not fold at all");

        let seen = recovered.scan("idx", None).await.unwrap();
        let ids: Vec<String> = seen.iter().map(|d| d.id.clone()).collect();
        let distinct: BTreeSet<String> = ids.iter().cloned().collect();

        let lost: Vec<&String> = out.acked.difference(&distinct).collect();
        assert!(
            lost.is_empty(),
            "seed {seed}: {} acknowledged rows were LOST ({} lanes, {} deaths): {lost:?}",
            lost.len(),
            out.lanes_used,
            out.deaths
        );
        assert_eq!(
            ids.len(),
            distinct.len(),
            "seed {seed}: a row was folded TWICE ({} rows, {} distinct)",
            ids.len(),
            distinct.len()
        );
    }

    // The scenario has to have actually exercised what it claims to. Without this a
    // future change that stops injecting faults, or stops killing writers, would leave
    // every assertion above passing vacuously.
    assert!(total_acked > 200, "only {total_acked} rows acknowledged");
    assert!(total_deaths > 20, "only {total_deaths} writer deaths");
    assert!(total_injected > 50, "only {total_injected} faults injected");
}

#[tokio::test]
async fn a_refused_flush_does_not_punch_a_hole_in_the_lane() {
    // The specific bug OQ-91 found, pinned so it cannot come back quietly. Ordinal 2 is
    // the third write: lane registration, bundle 0, then the refused bundle 1.
    let store = Arc::new(Flaky::refusing(&[2]));
    let t = TenantId(7);
    let e = Engine::new(Arc::clone(&store), t, LaneId(0));

    e.write("idx", vec![doc("a")]).await.unwrap();
    e.flush().await.unwrap();

    e.write("idx", vec![doc("b")]).await.unwrap();
    assert!(
        e.flush().await.is_err(),
        "the injected refusal must surface"
    );
    // Rows from a refused flush are still pending, not silently dropped: the caller was
    // never told they were durable, so they must still be there to retry.
    assert_eq!(e.pending_for_test().await, vec!["b".to_owned()]);

    e.write("idx", vec![doc("c")]).await.unwrap();
    e.flush().await.unwrap();

    // A fresh node, told nothing. If the refusal had consumed sequence 1, the tail probe
    // would stop there and `c` -- acknowledged and durable -- would be invisible forever.
    let recovered = Engine::new(Arc::clone(&store), t, LaneId(9_999));
    recovered.fold().await.unwrap();
    let ids: BTreeSet<String> = recovered
        .scan("idx", None)
        .await
        .unwrap()
        .iter()
        .map(|d| d.id.clone())
        .collect();
    assert!(ids.contains("a"), "rows before the refusal must survive");
    assert!(
        ids.contains("b") && ids.contains("c"),
        "rows after the refusal are invisible: the lane was truncated by a hole, got {ids:?}"
    );
}

#[tokio::test]
async fn recovery_after_a_fallback_write_to_another_lane() {
    // The mutation this exists to catch is a successor that probes only ITS OWN lane. The
    // seeded scenario above would find it eventually; this finds it immediately, and says
    // what is wrong when it does.
    let store = Arc::new(pstore_blob::MemoryStore::new());
    let t = TenantId(11);

    // A writer acknowledges a row on lane 0, then stops -- process gone, nothing folded.
    let dead = Engine::new(Arc::clone(&store), t, LaneId(0));
    dead.write("idx", vec![doc("before-fallback")])
        .await
        .unwrap();
    dead.flush().await.unwrap();
    drop(dead);

    // The same logical writer resumes on a different lane, as it would after a restart
    // that could not prove the old lane was free.
    let fallback = Engine::new(Arc::clone(&store), t, LaneId(1));
    fallback
        .write("idx", vec![doc("after-fallback")])
        .await
        .unwrap();
    fallback.flush().await.unwrap();

    // A third node, which owns neither lane, folds on the tenant's behalf.
    let third = Engine::new(Arc::clone(&store), t, LaneId(2));
    third.fold().await.unwrap();
    let ids: BTreeSet<String> = third
        .scan("idx", None)
        .await
        .unwrap()
        .iter()
        .map(|d| d.id.clone())
        .collect();
    assert!(
        ids.contains("before-fallback"),
        "the abandoned lane was not folded: a successor is only probing its own lane, got {ids:?}"
    );
    assert!(
        ids.contains("after-fallback"),
        "the fallback lane was not folded, got {ids:?}"
    );
}
