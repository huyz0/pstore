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
    /// Sent last period, delivered this one.
    in_flight: Vec<(String, String, Message)>,
}

impl Sim {
    fn new(n: u16, seeded: bool) -> Self {
        let mut nodes = Vec::new();
        let mut by_addr = BTreeMap::new();
        for i in 0..n {
            let mut c = Cluster::new(id(i), addr(i), "az-a".to_owned());
            if seeded {
                for j in 0..n {
                    c.join(id(j), addr(j), "az-a".to_owned());
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
            in_flight: Vec::new(),
        }
    }

    /// One period for every node. Messages produced this period are delivered **next**.
    ///
    /// ⚠️ A first version delivered replies within the same round, up to a depth of four. That
    /// made every ack instantaneous, so a probe timeout could never elapse and no amount of
    /// injected loss ever produced a suspicion — a lossy-network test passed against a
    /// protocol that, on a real fleet under 10% loss, flapped indefinitely. A simulation more
    /// generous than the network is worse than none: it certifies the bug.
    fn round(&mut self, seed: u64) {
        // Deliver what was sent last period, collecting whatever it provokes.
        let mut produced: Vec<(String, String, Message)> = Vec::new();
        for (from, to, msg) in std::mem::take(&mut self.in_flight) {
            self.sent += 1;
            self.bytes += msg.encode().len() as u64;
            if self.drop_every > 0 && self.sent.is_multiple_of(self.drop_every) {
                continue;
            }
            if let Some(&i) = self.by_addr.get(&to)
                && let Some(n) = self.nodes.get_mut(i)
            {
                let here = to.clone();
                produced.extend(
                    n.receive(&from, &msg)
                        .into_iter()
                        .map(|(t, m)| (here.clone(), t, m)),
                );
            }
        }
        // Then let every node take its period.
        for (i, n) in self.nodes.iter_mut().enumerate() {
            let from = addr(i as u16);
            produced.extend(
                n.tick(seed.wrapping_add(i as u64))
                    .into_iter()
                    .map(|(to, m)| (from.clone(), to, m)),
            );
        }
        self.in_flight = produced;
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
    sim.nodes[0]
        .cluster_mut()
        .join(id(900), addr(900), "az-a".to_owned());
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
fn two_nodes_each_holding_news_both_converge() {
    // ⚠️ A SYMMETRIC difference, which is the case a one-directional reconciliation misses.
    // Found on a real fleet, not here: every node saw all 100 members and the views were
    // stable, yet traffic sat at 6,000 bytes/s/node instead of the predicted 370 — because
    // the member *sets* agreed while their incarnations did not, and a sync that replies only
    // when the member COUNTS differ never sends back what the peer is missing. Each side kept
    // resyncing forever, and a checksum that never matches is a checksum that costs bytes and
    // buys nothing.
    let mut sim = Sim::new(6, true);
    sim.nodes[0]
        .cluster_mut()
        .join(id(700), addr(700), "az-a".to_owned());
    sim.nodes[1]
        .cluster_mut()
        .join(id(800), addr(800), "az-a".to_owned());

    for r in 0..80 {
        sim.round(r);
    }
    let sums: Vec<u64> = sim.nodes.iter().map(|p| p.cluster().checksum()).collect();
    assert!(
        sums.windows(2).all(|w| w[0] == w[1]),
        "nodes settled on different checksums, so they will reconcile forever: {sums:?}"
    );

    // And having agreed, they must go quiet.
    let before = sim.bytes;
    sim.round(999);
    let per_node = (sim.bytes - before) / 6;
    assert!(
        per_node <= 200,
        "an agreed fleet still spent {per_node} bytes per node in a round"
    );
}

#[test]
fn a_cold_fleet_converges_from_a_seed() {
    // Nobody knows anybody except through one seed, which is what a real join looks like.
    let mut sim = Sim::new(30, false);
    for i in 1..30u16 {
        sim.nodes[usize::from(i)]
            .cluster_mut()
            .join(id(0), addr(0), "az-a".to_owned());
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
fn a_lossy_network_does_not_manufacture_deaths() {
    // ⚠️ Nothing is actually dead here. A protocol that suspects on one lost datagram turns
    // packet loss into membership churn, and churn is exactly what defeats a checksum: the
    // cluster state never settles, so nodes reconcile forever and the steady state is never
    // reached. Measured on a real 100-node fleet under 10% loss before this was fixed: the
    // views FLAPPED, 88 nodes at 100 members, 11 at 99, one at 98, indefinitely.
    // ⚠️ 100 nodes, not a dozen. At a dozen, an every-Nth drop pattern happens to miss the
    // interleavings that matter and the test passes against a protocol that flaps on a real
    // fleet -- which is exactly what it did.
    let mut sim = Sim::new(100, true);
    sim.drop_every = 10; // 10% of datagrams discarded, every node healthy
    for r in 0..300 {
        sim.round(r);
    }
    let worst = sim
        .nodes
        .iter()
        .map(|p| p.cluster().alive().len())
        .min()
        .unwrap_or(0);
    assert_eq!(
        worst, 100,
        "with every node alive and 10% loss, the worst view fell to {worst} of 100: lost \
         datagrams are being read as deaths"
    );
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

#[test]
fn a_change_spreads_by_piggyback_without_a_full_reconciliation() {
    // ⚠️ Dissemination is what keeps reconciliation RARE. If changes only ever travelled by
    // full sync, every membership event would cost O(N) to everyone and the checksum would be
    // saving nothing. Mutation testing found `piggyback -> vec![]` survived: the fleet still
    // converged, by syncing — which is the expensive path the design exists to avoid.
    let mut sim = Sim::new(8, true);
    for r in 0..3 {
        sim.round(r); // settle, so the only news below is the one we introduce
    }
    sim.nodes[0].cluster_mut().suspect(&id(7));

    let before = sim.bytes;
    let mut rounds = 0;
    for r in 10..60 {
        sim.round(r);
        rounds += 1;
        if sim
            .nodes
            .iter()
            .all(|p| p.cluster().state(&id(7)).is_some())
        {
            break;
        }
    }
    let per_node_per_round = (sim.bytes - before) / 8 / rounds.max(1);
    assert!(
        per_node_per_round <= 400,
        "spreading one suspicion cost {per_node_per_round} bytes per node per round, which is \
         a reconciliation rather than a piggyback"
    );
}

#[test]
fn a_node_never_probes_itself() {
    // ⚠️ A node probing itself always succeeds, so it would never suspect anyone and never
    // detect anything, while looking perfectly healthy. Mutation testing reached the filter
    // that excludes self (`&&` to `||`) and no test noticed.
    let mut sim = Sim::new(4, true);
    for r in 0..20u64 {
        let out = sim.nodes[0].tick(r);
        for (to, _) in &out {
            assert_ne!(*to, addr(0), "a node addressed a probe to itself");
        }
    }
}

#[test]
fn a_peer_that_believes_us_dead_is_told_so_it_can_refute() {
    // ⚠️ The mechanism that heals a partition. A node must NOT revive a peer by inventing an
    // incarnation for it; it hands the peer its own record instead, and only the peer may
    // raise it. Mutation testing found `evidence_for -> vec![]` survived, meaning nothing
    // tested the mechanism directly -- the heal test passed through some other path.
    let mut a = Protocol::new({
        let mut c = Cluster::new(id(0), addr(0), "az-a".to_owned());
        c.join(id(1), addr(1), "az-a".to_owned());
        c
    });
    a.cluster_mut().suspect(&id(1));
    a.cluster_mut().declare_dead(&id(1));
    assert_eq!(a.cluster().state(&id(1)), Some(State::Dead));

    // Node 1 speaks to A. A must reply with a record saying "you are dead", so 1 can refute.
    let ping = Message::Ping {
        from: id(1),
        seq: 1,
        checksum: 0,
        updates: vec![],
    };
    let out = a.receive(&addr(1), &ping);
    let told = out.iter().any(|(_, m)| match m {
        Message::Ack { updates, .. } => updates
            .iter()
            .any(|u| u.id == id(1) && u.state == State::Dead),
        _ => false,
    });
    assert!(
        told,
        "a node believed dead spoke to us and was not told, so it can never refute and the \
         partition never heals"
    );
    assert_eq!(
        a.cluster().incarnation(&id(1)),
        Some(0),
        "we invented an incarnation for another node, which makes two nodes disagree about \
         its version forever"
    );
}

#[test]
fn a_probe_is_not_abandoned_before_its_timeout() {
    // ⚠️ Suspecting on the first missed period turns every scheduling hiccup into an eviction
    // -- OQ-12's failure, in the timeouts. Mutation testing reached the comparison
    // (`>=` to `<`) and nothing failed.
    let mut sim = Sim::new(6, true);
    sim.by_addr.remove(&addr(5)); // stops answering immediately
    // ⚠️ Two rounds, not one: a probe is only registered at the END of a tick, so after one
    // round there is nothing outstanding and the assertion would hold against any timeout at
    // all. An earlier version of this test did exactly that and proved nothing.
    sim.round(1);
    sim.round(2);
    assert!(
        sim.nodes
            .iter()
            .take(5)
            .all(|p| p.cluster().state(&id(5)) == Some(State::Alive)),
        "a peer was suspected after fewer periods than the probe timeout"
    );
}

#[test]
fn probes_rotate_across_peers() {
    // ⚠️ One probe per period is only O(1) *and* useful if it lands on different peers. A
    // node that probes the same peer forever detects one failure and is blind to the rest,
    // while looking perfectly healthy. Mutation testing reached the peer-selection mixing and
    // nothing noticed -- a degenerate hash picks index 0 every time.
    let mut sim = Sim::new(12, true);
    let mut targets = std::collections::HashSet::new();
    for r in 0..60u64 {
        for (to, _) in sim.nodes[0].tick(r) {
            targets.insert(to);
        }
    }
    assert!(
        targets.len() >= 5,
        "60 probes reached only {} distinct peers of 11",
        targets.len()
    );
}

#[test]
fn a_failed_probe_asks_other_peers_before_concluding() {
    // ⚠️ The indirect probe. A direct probe failing means WE could not reach it, which is not
    // the same claim as its being gone -- treating them as the same lets one node's bad link
    // evict a healthy peer. Mutation testing found `helpers` could return nothing, or the
    // target itself, with every test still green.
    // ⚠️ Every LIVE peer is answered; only node 5 stays silent. A first draft of this
    // delivered nothing at all, so every peer looked unreachable and node 0 was quite
    // correctly asking about all of them — the test was wrong, not the protocol.
    let mut sim = Sim::new(6, true);
    let mut asked: Vec<String> = Vec::new();
    for r in 0..30u64 {
        for (to, msg) in sim.nodes[0].tick(r) {
            match &msg {
                Message::Ping { seq, .. } if to != addr(5) => {
                    // Whoever it probed answers, so only node 5 ever goes quiet.
                    let who = sim.by_addr.get(&to).copied().unwrap_or(0);
                    let ack = Message::Ack {
                        from: id(who as u16),
                        seq: *seq,
                        checksum: sim.nodes[0].cluster().checksum(),
                        updates: vec![],
                    };
                    sim.nodes[0].receive(&to, &ack);
                }
                Message::PingReq { target, .. } => {
                    assert_eq!(*target, id(5), "asked about a peer that was answering");
                    assert_ne!(to, addr(5), "asked the unreachable peer to probe itself");
                    asked.push(to.clone());
                }
                _ => {}
            }
        }
    }
    assert!(
        !asked.is_empty(),
        "a probe went unanswered and no other peer was ever asked to check"
    );
}

#[test]
fn a_change_is_retransmitted_a_bounded_number_of_times() {
    // ⚠️ Dissemination must STOP. An update that rides along forever is a permanent tax on
    // every probe -- measured on a real fleet as 6,000 bytes/s/node against a predicted 370,
    // with nothing changing at all. Mutation testing reached the retirement comparison and
    // every variant survived.
    let mut proto = {
        let mut c = Cluster::new(id(0), addr(0), "az-a".to_owned());
        for i in 1..5 {
            c.join(id(i), addr(i), "az-a".to_owned());
        }
        Protocol::new(c)
    };
    proto.cluster_mut().suspect(&id(3));

    let mut carried = 0;
    for r in 0..40u64 {
        for (_, msg) in proto.tick(r) {
            if let Message::Ping { updates, .. } = msg {
                carried += updates.len();
            }
        }
    }
    assert!(
        carried > 0,
        "a change was never disseminated, so nothing propagates without a full sync"
    );
    assert!(
        carried <= 24,
        "a single change rode along {carried} times in 40 rounds; it is never retired"
    );
}
