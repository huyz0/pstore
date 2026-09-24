//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::float_cmp,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! Gray failure: an AZ degraded but alive, with every liveness check green.
//!
//! ⚠️ Per-AZ cells *caused* this blind spot — eliminating cross-AZ traffic eliminates the
//! signal that would reveal it. These decisions are the bill for D-79.

use pstore_cluster::gray::{self, Verdict, ZoneHealth};

fn zone(name: &str, success: f64, latency_ms: f64, utilization: f64) -> ZoneHealth {
    ZoneHealth {
        zone: name.to_owned(),
        success_rate: success,
        latency_ms,
        utilization,
    }
}

/// Three healthy zones, lightly loaded.
fn healthy() -> Vec<ZoneHealth> {
    vec![
        zone("az-a", 0.999, 20.0, 0.30),
        zone("az-b", 0.998, 21.0, 0.31),
        zone("az-c", 0.999, 20.5, 0.29),
    ]
}

#[test]
fn a_uniformly_slow_fleet_has_no_outlier() {
    // ⚠️ D-84's whole point: "is az-b an outlier versus az-a and az-c *right now*", never
    // "is az-b slower than 100 ms". A fixed threshold fires on a fleet that is uniformly busy
    // — a Tuesday — and drains a zone for being normal.
    let slow: Vec<ZoneHealth> = healthy()
        .into_iter()
        .map(|z| ZoneHealth {
            latency_ms: z.latency_ms * 10.0,
            ..z
        })
        .collect();
    assert!(
        gray::outliers(&slow, gray::DEFAULT_MARGIN).is_empty(),
        "a uniformly slow fleet produced an outlier: this is an absolute threshold wearing a \
         peer-relative name"
    );
    // And the healthy baseline is likewise quiet.
    assert!(gray::outliers(&healthy(), gray::DEFAULT_MARGIN).is_empty());
}

#[test]
fn a_zone_much_slower_than_its_peers_is_an_outlier() {
    let mut s = healthy();
    s[1].latency_ms = 100.0;
    assert_eq!(
        gray::outliers(&s, gray::DEFAULT_MARGIN),
        ["az-b"],
        "a zone five times slower than its peers was not detected"
    );
}

#[test]
fn a_zone_failing_requests_is_an_outlier_even_when_fast() {
    // Latency is not the only signal: a zone can answer quickly and wrongly.
    let mut s = healthy();
    s[2].success_rate = 0.80;
    assert_eq!(gray::outliers(&s, gray::DEFAULT_MARGIN), ["az-c"]);
}

#[test]
fn an_overloaded_zone_sheds_rather_than_draining() {
    // ⚠️ THE safety property. Draining an overloaded AZ moves its load onto the others and can
    // cascade — the detector causing the outage it was built to prevent. Degradation that
    // rises WITH utilization is load, and load is shed, never drained.
    let mut s = healthy();
    s[1].latency_ms = 100.0;
    s[1].utilization = 0.95;
    assert_eq!(
        gray::verdict(&s, "az-b", gray::DEFAULT_MARGIN),
        Verdict::Shed,
        "an overloaded zone was drained, which moves its load onto the survivors and cascades"
    );

    // The same degradation at low utilization is infrastructure, and drains.
    s[1].utilization = 0.25;
    assert_eq!(
        gray::verdict(&s, "az-b", gray::DEFAULT_MARGIN),
        Verdict::Drain,
        "a zone slow while doing little work is gray failure and must drain"
    );
}

#[test]
fn draining_requires_headroom_in_the_survivors() {
    // Draining into a fleet that cannot absorb the load is the cascade by another route.
    let mut s = healthy();
    s[1].latency_ms = 100.0;
    s[1].utilization = 0.20;
    // Survivors already near capacity: taking az-b's share would tip them over.
    s[0].utilization = 0.90;
    s[2].utilization = 0.88;
    assert_eq!(
        gray::verdict(&s, "az-b", gray::DEFAULT_MARGIN),
        Verdict::Shed,
        "a zone was drained into survivors with no headroom to take it"
    );
}

#[test]
fn a_majority_can_never_drain() {
    // ⚠️ Two of three zones degraded is a fleet-wide event, and the answer to a fleet-wide
    // event is not to switch the fleet off.
    let mut s = healthy();
    for i in [0usize, 1] {
        s[i].latency_ms = 100.0;
        s[i].utilization = 0.20;
    }
    for z in ["az-a", "az-b"] {
        assert_ne!(
            gray::verdict(&s, z, gray::DEFAULT_MARGIN),
            Verdict::Drain,
            "{z} drained while a majority of the fleet was degraded"
        );
    }
}

#[test]
fn a_healthy_zone_is_never_drained() {
    for z in ["az-a", "az-b", "az-c"] {
        assert_eq!(
            gray::verdict(&healthy(), z, gray::DEFAULT_MARGIN),
            Verdict::Healthy
        );
    }
}

#[test]
fn a_node_that_cannot_reach_the_store_evicts_itself() {
    // ⚠️ D-87, and the one case where self-assessment is reliable: the blob store is our only
    // dependency, so a node that cannot reach it is definitionally useless. It must not wait
    // for a bulletin it cannot fetch — which is the deadlock a naive "ask the others" rule
    // walks into precisely when it matters.
    assert!(
        gray::should_self_evict(false, Verdict::Healthy),
        "a node cut off from the blob store stayed in the load balancer"
    );
}

#[test]
fn a_node_evicts_on_others_evidence_and_not_its_own() {
    // ⚠️ D-86 inverts the usual health check: a node self-evicts because *external* observers
    // say it is bad. A node's own opinion of itself is exactly what gray failure defeats —
    // differential observability is the definition.
    assert!(
        gray::should_self_evict(true, Verdict::Drain),
        "a node ignored a drain verdict reached by its peers"
    );
    assert!(
        !gray::should_self_evict(true, Verdict::Shed),
        "a node self-evicted on a SHED verdict, turning load shedding into a drain and \
         cascading exactly as D-85 warns"
    );
    assert!(!gray::should_self_evict(true, Verdict::Healthy));
}

#[test]
fn outliers_need_three_zones_but_not_exactly_three() {
    // ⚠️ Every other test uses exactly three zones, so `samples.len() < 3` -> `>` survived: it
    // only differs above and below three. Four zones with one degraded names it; two zones
    // with one degraded names nobody, because with two, whichever is worse is always "the
    // outlier".
    let four = vec![
        zone("az-a", 0.999, 100.0, 0.20),
        zone("az-b", 0.999, 100.0, 0.20),
        zone("az-c", 0.999, 100.0, 0.20),
        zone("az-d", 0.999, 1_000.0, 0.20),
    ];
    assert_eq!(gray::outliers(&four, gray::DEFAULT_MARGIN), vec!["az-d"]);
    let two = vec![
        zone("az-a", 0.999, 100.0, 0.20),
        zone("az-b", 0.999, 1_000.0, 0.20),
    ];
    assert!(gray::outliers(&two, gray::DEFAULT_MARGIN).is_empty());
}

/// ⚠️ The three strict comparisons, pinned AT the edge -- M8h's sweep found each one's
/// non-strict twin surviving, because no test put a value exactly on a threshold. Values are
/// chosen so the arithmetic is exact in f64: `100 * (1 + 0.5) == 150`, `0.999 - 0.05 == 0.949`
/// (the success margin is private, so its 0.05 is repeated here), and `0.80` is the survivor
/// headroom.
#[test]
fn a_zone_exactly_at_the_latency_threshold_is_not_an_outlier() {
    let at = |lat: f64| {
        vec![
            zone("az-a", 0.999, 100.0, 0.20),
            zone("az-b", 0.999, 100.0, 0.20),
            zone("az-c", 0.999, lat, 0.20),
        ]
    };
    assert!(gray::outliers(&at(150.0), gray::DEFAULT_MARGIN).is_empty());
    assert_eq!(
        gray::outliers(&at(150.0f64.next_up()), gray::DEFAULT_MARGIN),
        vec!["az-c"]
    );
}

#[test]
fn a_zone_exactly_at_the_success_margin_is_not_an_outlier() {
    let at = |ok: f64| {
        vec![
            zone("az-a", 0.999, 100.0, 0.20),
            zone("az-b", 0.999, 100.0, 0.20),
            zone("az-c", ok, 100.0, 0.20),
        ]
    };
    assert!(gray::outliers(&at(0.949), gray::DEFAULT_MARGIN).is_empty());
    assert_eq!(
        gray::outliers(&at(0.949f64.next_down()), gray::DEFAULT_MARGIN),
        vec!["az-c"]
    );
}

#[test]
fn a_survivor_exactly_at_the_headroom_blocks_a_drain() {
    // One bad zone of three (so the majority guard does not fire), idle (so it is not load):
    // the survivors' headroom decides, and a survivor exactly at it cannot take the share.
    let with = |survivor: f64| {
        vec![
            zone("az-a", 0.999, 100.0, survivor),
            zone("az-b", 0.999, 100.0, 0.30),
            zone("az-c", 0.999, 1_000.0, 0.20),
        ]
    };
    assert_eq!(
        gray::verdict(&with(0.80), "az-c", gray::DEFAULT_MARGIN),
        Verdict::Shed
    );
    assert_eq!(
        gray::verdict(&with(0.80f64.next_down()), "az-c", gray::DEFAULT_MARGIN),
        Verdict::Drain
    );
}
