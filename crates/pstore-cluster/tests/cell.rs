//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "assertions in tests are the reporting mechanism"
)]

//! Cells: the (cluster, zone) pair that addresses one placement ring.

use pstore_cluster::{Cell, Roster};
use std::collections::HashMap;

#[test]
fn cell_keys_do_not_collide_across_ten_thousand_cells() {
    // ⚠️ The roster key was `format!("{:04x}/clu/ROSTER", fnv(cluster) as u16)` — sixteen bits
    // of hash, with the cluster name **absent from the path entirely**. Two clusters colliding
    // in 65,536 shared one roster object and therefore one ring, which is every query crossing
    // an AZ boundary and every one of them billed. `head.rs` had it right all along:
    // `{:04x}/tnt/{}/HEAD`, a hash prefix *and* the identity.
    //
    // A generated set, not two hand-picked inequalities: a pair of examples cannot see a
    // collision, which is why the original criterion could not.
    let mut seen: HashMap<String, Cell> = HashMap::new();
    for c in 0..100 {
        for z in 0..100 {
            let cell = Cell::new(&format!("cluster-{c}"), &format!("az-{z}"));
            let key = Roster::key(&cell).as_str().to_owned();
            if let Some(prev) = seen.insert(key.clone(), cell.clone()) {
                panic!("{cell:?} and {prev:?} share the roster key {key}");
            }
        }
    }
    assert_eq!(seen.len(), 10_000);
}

#[test]
fn a_cell_key_names_both_its_cluster_and_its_zone() {
    // A key that includes one and not the other collides on the other, which criterion 1 would
    // catch — but this says which half is missing.
    let k = Roster::key(&Cell::new("prod", "az-a"));
    let s = k.as_str();
    assert!(s.contains("prod"), "the cluster is not in the key: {s}");
    assert!(s.contains("az-a"), "the zone is not in the key: {s}");

    // And the hash prefix survives, because it is what spreads keys across the store's
    // partitions — dropping it would put every cluster's roster on one prefix.
    assert!(
        s.split('/').next().is_some_and(|p| p.len() == 4),
        "the four-hex-digit spread prefix is gone: {s}"
    );
}

#[test]
fn a_cells_roster_holds_only_its_own_members() {
    // ⚠️ The criterion the first draft of this milestone lacked, and the bug it would have
    // shipped: a per-cell roster ADDRESS is worthless while its CONTENTS come from the
    // fleet-wide gossip view. A node in az-a would write az-b's members into az-a's roster and
    // place across zones anyway, with every other test passing.
    let view = [
        ("10.0.0.1:7946", "az-a"),
        ("10.0.0.2:7946", "az-b"),
        ("10.0.0.3:7946", "az-a"),
        ("10.0.0.4:7946", "az-c"),
        ("10.0.0.5:7946", "az-a"),
    ];
    let members: Vec<(String, String)> = view
        .iter()
        .map(|(a, z)| ((*a).to_owned(), (*z).to_owned()))
        .collect();

    let a = Roster::from_members(
        members.iter().map(|(a, z)| (a.as_str(), z.as_str())),
        "az-a",
    );
    assert_eq!(
        a.nodes(),
        ["10.0.0.1:7946", "10.0.0.3:7946", "10.0.0.5:7946"],
        "a cell's roster picked up members of other zones"
    );

    let b = Roster::from_members(
        members.iter().map(|(a, z)| (a.as_str(), z.as_str())),
        "az-b",
    );
    assert_eq!(b.nodes(), ["10.0.0.2:7946"]);
}

#[test]
fn a_cell_with_no_members_is_empty_not_everyone() {
    // ⚠️ The dangerous fallback: "no members in this zone, so use them all". That is one global
    // ring wearing a cell's name, and it appears exactly when a zone is new or has just lost
    // its last node — the moment it matters most.
    let members = [("10.0.0.1:7946", "az-a"), ("10.0.0.2:7946", "az-b")];
    let empty = Roster::from_members(members.iter().copied(), "az-zzz");
    assert!(
        empty.nodes().is_empty(),
        "an unknown zone got {} members instead of none",
        empty.nodes().len()
    );
}

#[test]
fn the_key_prefix_actually_spreads_across_partitions() {
    // ⚠️ Once the names are in the path, uniqueness no longer depends on the hash — so a
    // constant prefix passes the collision test above while putting **every roster in the
    // fleet on one storage prefix**. That is a hot partition, which is what the prefix exists
    // to prevent and the reason object stores document key-prefix spreading at all.
    //
    // Mutation testing found exactly this: `fnv -> 0` survived everything.
    let prefixes: std::collections::HashSet<String> = (0..500)
        .map(|i| {
            let k = Roster::key(&Cell::new(&format!("cluster-{i}"), "az-a"));
            k.as_str().split('/').next().unwrap_or_default().to_owned()
        })
        .collect();
    assert!(
        prefixes.len() > 400,
        "500 cells landed on only {} distinct prefixes: the spread is not spreading",
        prefixes.len()
    );

    // And the zone participates, or every zone of one cluster shares a partition.
    let zoned: std::collections::HashSet<String> = ["az-a", "az-b", "az-c", "az-d"]
        .iter()
        .map(|z| {
            let k = Roster::key(&Cell::new("one-cluster", z));
            k.as_str().split('/').next().unwrap_or_default().to_owned()
        })
        .collect();
    assert!(
        zoned.len() > 1,
        "every zone of one cluster hashed to the same prefix"
    );
}

#[test]
fn a_cell_reports_the_parts_it_was_built_from() {
    // The accessors are how a caller filters a gossip view down to its own cell; one that
    // returns the wrong half puts the node in the wrong ring.
    let c = Cell::new("prod-eu", "eu-west-1b");
    assert_eq!(c.cluster(), "prod-eu");
    assert_eq!(c.zone(), "eu-west-1b");
}

#[test]
fn placement_stays_inside_its_cell() {
    // ⚠️ End to end: a gossip view spanning three zones, a roster built for one of them, and
    // 1,000 keys placed. The bug this catches is a call site that builds its ring from the
    // *unfiltered* view — which the node's own `OWNS` reporting did until this phase. A
    // per-cell roster that some call sites bypass is the same bug in a smaller place.
    use pstore_cluster::Placement;

    let view: Vec<(String, String)> = (0..90)
        .map(|i| {
            (
                format!("10.0.{}.{}:7946", i / 256, i % 256),
                format!("az-{}", i % 3),
            )
        })
        .collect();

    for zone in ["az-0", "az-1", "az-2"] {
        let roster = Roster::from_members(view.iter().map(|(a, z)| (a.as_str(), z.as_str())), zone);
        assert_eq!(
            roster.nodes().len(),
            30,
            "{zone} should hold a third of the fleet"
        );

        let mine: std::collections::HashSet<&str> =
            roster.nodes().iter().map(String::as_str).collect();
        let p = Placement::new(&roster);
        for i in 0..1_000 {
            for node in p.place(&format!("tenant/idx{i}/s0"), 3) {
                assert!(
                    mine.contains(node),
                    "{zone} placed on {node}, which is in another cell — every byte of that \
                     query would cross an AZ boundary and be billed"
                );
            }
        }
    }
}

#[test]
fn imbalance_at_cell_scale_stays_bounded() {
    // ⚠️ A regression guard set FROM the measurement, not before it. `examples/az_balance.rs`
    // measures 1.31 mean and 1.50 worst at N=300 across trials; 1.6 is above that and far
    // below anything a clustering hash would produce.
    //
    // ⚠️ **Not 1.25×.** That was M4a's bound at **N=100**, and requiring it at 300 is what made
    // the first draft of this criterion unsatisfiable — a 300-node cell exceeds 1.25 in 23 of
    // 24 samples, so the only exits were to fail the milestone or tune the hash until the
    // number appeared.
    use pstore_cluster::Placement;

    for trial in 0..4u32 {
        let roster = Roster::from_nodes(
            (0..300).map(|i| format!("10.{trial}.{}.{}:7946", i / 256, i % 256)),
        );
        let p = Placement::new(&roster);
        let mut load: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
        let keys = 20_000;
        for i in 0..keys {
            for n in p.place(&format!("t/idx{i}/s0"), 3) {
                *load.entry(n).or_default() += 1;
            }
        }
        let mean = (keys * 3) as f64 / 300.0;
        let worst = load.values().copied().max().unwrap_or(0) as f64 / mean;
        assert!(
            worst < 1.6,
            "trial {trial}: imbalance {worst:.3} at a 300-node cell"
        );
    }
}
