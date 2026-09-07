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
