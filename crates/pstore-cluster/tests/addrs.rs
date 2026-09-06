#![allow(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    reason = "assertions report"
)]
//! Placement over the strings a real fleet actually uses.

use pstore_cluster::{Placement, Roster};

#[test]
fn every_member_owns_roughly_its_share_of_keys() {
    // ⚠️ Node ids in the fleet are `IP:port`, which share long prefixes — `172.19.0.2:7946`
    // through `172.19.0.101:7946`. A hash that keys on a prefix, or a comparison that never
    // matches, shows up as a node owning exactly ZERO of a thousand keys while holding a
    // full view: measured in the 100-node run, which is what this reproduces in-process.
    let addrs: Vec<String> = (2..88).map(|i| format!("172.19.0.{i}:7946")).collect();
    let r = Roster::from_nodes(addrs.clone());
    let p = Placement::new(&r);
    let mut owned = vec![0usize; addrs.len()];
    for i in 0..1000 {
        for n in p.place(&format!("idx{i}/s0"), 3) {
            let at = addrs.iter().position(|a| a == n).unwrap();
            owned[at] += 1;
        }
    }
    let zero = owned.iter().filter(|c| **c == 0).count();
    assert_eq!(zero, 0, "{zero} of {} members own nothing", addrs.len());
    let expected = 3.0 * 1000.0 / addrs.len() as f64;
    let min = *owned.iter().min().unwrap() as f64;
    assert!(
        min > expected * 0.3,
        "the coldest member owns {min} against an expected {expected:.0}"
    );
}
