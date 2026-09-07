//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! The protocol, driven as a deterministic simulation.
//!
//! ⚠️ No sockets. M4b learned this the expensive way: its gossip behaviour was only
//! observable by running a hundred containers, so five bugs were found that way instead of
//! here. A protocol that is a pure function of (state, message) can be tested at any fleet
//! size in milliseconds — including the **1,000-node** case, which is the one that matters
//! and the one a Docker run measures least well.

use pstore_gossip::{Cluster, Message, Protocol, State};
use std::collections::BTreeMap;

fn id(n: u16) -> [u8; 16] {
    let mut b = [0u8; 16];
    b[..2].copy_from_slice(&n.to_le_bytes());
    b
}

fn addr(n: u16) -> String {
    format!("10.0.0.{}:{}", n / 256, 7000 + (n % 256))
}

/// A fleet of protocols exchanging messages in lockstep, with no network.
struct Sim {
    nodes: Vec<Protocol>,
    by_addr: BTreeMap<String, usize>,
    /// Bytes actually put on the simulated wire, so cost is measured rather than asserted.
    bytes: u64,
    /// Fraction of datagrams dropped, as a deterministic every-Nth rather than a random draw.
    drop_every: u64,
    sent: u64,
}

impl Sim {
    fn new(n: u16, seeded: bool) -> Self {
        let mut nodes = Vec::new();
        let mut by_addr = BTreeMap::new();
        for i in 0..n {
            let mut c = Cluster::new(id(i), addr(i));
            if seeded {
                for j in 0..n {
                    c.join(id(j), addr(j));
                }
            }
            by_addr.insert(addr(i), usize::from(i));
            nodes.push(Protocol::new(c));
        }
        Self {
            nodes,
            by_addr,
            bytes: 0,
            drop_every: 0,
            sent: 0,
        }
    }

    /// One period for every node, then delivery of everything they produced.
    fn round(&mut self, seed: u64) {
        // (from_addr, to_addr, msg) — the transport knows who sent a datagram, so the
        // simulation has to as well, or it tests a protocol nobody can deploy.
        let mut queue: Vec<(String, String, Message)> = Vec::new();
        for (i, n) in self.nodes.iter_mut().enumerate() {
            let from = addr(i as u16);
            queue.extend(
                n.tick(seed.wrapping_add(i as u64))
                    .into_iter()
                    .map(|(to, m)| (from.clone(), to, m)),
            );
        }
        // Deliver to a fixed depth so a reply-to-a-reply cannot loop forever unnoticed.
        for _ in 0..4 {
            let mut next: Vec<(String, String, Message)> = Vec::new();
            for (from, to, msg) in queue.drain(..) {
                self.sent += 1;
                self.bytes += msg.encode().len() as u64;
                if self.drop_every > 0 && self.sent.is_multiple_of(self.drop_every) {
                    continue;
                }
                if let Some(&i) = self.by_addr.get(&to)
                    && let Some(n) = self.nodes.get_mut(i)
                {
                    let here = to.clone();
                    next.extend(
                        n.receive(&from, &msg)
                            .into_iter()
                            .map(|(t, m)| (here.clone(), t, m)),
                    );
                }
            }
            if next.is_empty() {
                break;
            }
            queue = next;
        }
    }

    fn all_see(&self, n: usize) -> bool {
        self.nodes.iter().all(|p| p.cluster().alive().len() == n)
    }
}

#[test]
fn a_converged_cluster_exchanges_a_checksum_not_a_member_list() {
    // ⚠️ THE criterion. A stable 100-node fleet must cost a probe and an ack per node per
    // round -- nothing proportional to the fleet. `chitchat` cannot do this at any period,
    // because its heartbeat is part of the state its digest covers.
    let mut sim = Sim::new(100, true);
    sim.round(1); // let any first-round chatter settle
    let before = sim.bytes;
    sim.round(2);
    let per_node = (sim.bytes - before) / 100;
    assert!(
        per_node <= 200,
        "a converged round cost {per_node} bytes per node; chitchat's measured cost was ~13,000"
    );
}

#[test]
fn the_steady_state_cost_does_not_grow_with_the_fleet() {
    // ⚠️ O(1), stated as a comparison rather than an absolute, because an absolute can be met
    // by a protocol that is merely small and still linear.
    let cost = |n: u16| {
        let mut sim = Sim::new(n, true);
        sim.round(1);
        let before = sim.bytes;
        sim.round(2);
        (sim.bytes - before) / u64::from(n)
    };
    let (small, large) = (cost(100), cost(1_000));
    assert!(
        large <= small * 2,
        "per-node cost went from {small} bytes at 100 nodes to {large} at 1,000, which is growth"
    );
}

#[test]
fn a_disagreement_still_reconciles() {
    // The other half: cheap when agreeing is worthless if it never notices a disagreement.
    // One node knows a member the rest do not, and the fleet must converge on it.
    let mut sim = Sim::new(20, true);
    sim.nodes[0].cluster_mut().join(id(900), addr(900));
    assert!(!sim.all_see(21));
    for r in 0..40 {
        sim.round(r);
        if sim.all_see(21) {
            return;
        }
    }
    panic!("a new member never reached the whole fleet");
}

#[test]
fn a_cold_fleet_converges_from_a_seed() {
    // Nobody knows anybody except through one seed, which is what a real join looks like.
    let mut sim = Sim::new(30, false);
    for i in 1..30u16 {
        sim.nodes[usize::from(i)].cluster_mut().join(id(0), addr(0));
    }
    for r in 0..200 {
        sim.round(r);
        if sim.all_see(30) {
            return;
        }
    }
    let worst = sim
        .nodes
        .iter()
        .map(|p| p.cluster().alive().len())
        .min()
        .unwrap_or(0);
    panic!("a cold fleet did not converge; the worst node saw {worst} of 30");
}

#[test]
fn a_silent_node_is_suspected_then_declared_dead() {
    // ⚠️ Both transitions, and in order. A protocol that jumps straight to dead evicts a node
    // that missed one packet; one that never leaves suspect never evicts at all.
    let mut sim = Sim::new(10, true);
    let victim = addr(9);
    sim.by_addr.remove(&victim); // it stops answering, without telling anyone

    let mut saw_suspect = false;
    for r in 0..400 {
        sim.round(r);
        let states: Vec<Option<State>> = sim
            .nodes
            .iter()
            .take(9)
            .map(|p| p.cluster().state(&id(9)))
            .collect();
        if states.contains(&Some(State::Suspect)) {
            saw_suspect = true;
        }
        if states.iter().all(|s| *s == Some(State::Dead)) {
            assert!(
                saw_suspect,
                "a node went straight to dead without being suspected"
            );
            return;
        }
    }
    panic!("a silent node was never declared dead by every survivor");
}

#[test]
fn a_dead_node_is_detected_under_loss() {
    // Criterion 6's shape: detection must not need a clean network.
    let mut sim = Sim::new(10, true);
    sim.drop_every = 10; // 10% of datagrams discarded
    sim.by_addr.remove(&addr(9));
    for r in 0..800 {
        sim.round(r);
        if (0..9).all(|i| sim.nodes[i].cluster().state(&id(9)) == Some(State::Dead)) {
            return;
        }
    }
    panic!("under 10% loss, a dead node was never detected by every survivor");
}

#[test]
fn a_healed_partition_converges_to_one_member_set() {
    // ⚠️ Two halves that each converged separately must merge, not settle into two clusters
    // that are each internally consistent -- which is what a checksum comparison rewards if
    // the halves never speak.
    let mut sim = Sim::new(20, true);
    let held: BTreeMap<String, usize> = sim.by_addr.clone();
    sim.by_addr
        .retain(|a, _| held.get(a).is_some_and(|i| *i < 10usize));
    for r in 0..60 {
        sim.round(r);
    }
    sim.by_addr = held;
    for r in 100..400 {
        sim.round(r);
        if sim.nodes.iter().all(|p| p.cluster().alive().len() == 20) {
            return;
        }
    }
    let worst = sim
        .nodes
        .iter()
        .map(|p| p.cluster().alive().len())
        .min()
        .unwrap_or(0);
    panic!("a healed partition left the fleet split; the worst node saw {worst} of 20");
}
