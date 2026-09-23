//! Gossip membership.
//!
//! ## ⚠️ Not SWIM+Lifeguard, and the reason is a licence
//!
//! D-4 specifies **SWIM + Lifeguard**. The maintained Rust implementation of exactly that is
//! `foca`, which is **MPL-2.0** — outside this project's licence allow-list
//! (`deny.toml`: Apache-2.0, MIT, BSD, ISC, Unicode-3.0, Zlib). So this uses **`chitchat`**
//! (MIT, Quickwit), which is **Scuttlebutt reconciliation with phi-accrual failure
//! detection**, not SWIM with Lifeguard.
//!
//! What D-4 actually needs survives the substitution:
//! * gossip membership with no external service discovery — yes;
//! * the blob store as seed and backstop — that is ours, not the library's;
//! * **false-positive suppression when a node is busy** — Lifeguard's contribution, and
//!   phi-accrual's whole design: it adapts its threshold to observed inter-arrival times
//!   rather than using a fixed timeout.
//!
//! ⚠️ So **OQ-12 becomes a question about phi-accrual, not Lifeguard** — same property, other
//! mechanism, and still worth asking, because our nodes are CPU-pinned by scan work and that
//! is SWIM's pathological case. Recorded as a correction rather than absorbed silently.

use crate::transport::{Metered, Stats};
use chitchat::transport::UdpTransport;
use chitchat::{ChitchatConfig, ChitchatHandle, ChitchatId, FailureDetectorConfig};
use std::net::SocketAddr;
use std::sync::Arc;

/// A running gossip member.
pub struct Member {
    handle: ChitchatHandle,
    stats: Arc<Stats>,
}

impl Member {
    /// The address this node advertises to peers, as they see it.
    pub fn self_addr(&self) -> String {
        self.handle.chitchat_id().gossip_advertise_addr.to_string()
    }

    /// Gossip bytes sent, received, and datagrams deliberately dropped, since start.
    ///
    /// Counted at the socket, so it is gossip alone — Docker's counters would include the
    /// blob store and the runtime's own traffic (M4b criterion 5).
    pub fn traffic(&self) -> (u64, u64, u64) {
        self.stats.read()
    }

    /// Dial a peer directly, outside the normal gossip schedule.
    ///
    /// ⚠️ This is the **partition-healing backstop** D-4 asks the blob store to be, and it
    /// is not optional. chitchat takes its seed list **once, at startup**: a node whose
    /// seeds were all unreachable in that instant never contacts anyone again, and no
    /// amount of gossip from the other side reaches it, because no peer knows its address
    /// either. It is isolated permanently, by a transient. Measured on a 100-node fleet:
    /// 96 nodes converged on a 96-member view while one sat at `members=1` for 493 ticks.
    ///
    /// Returns whether the address parsed and the dial was accepted — never whether the
    /// peer answered, which only the next view can say.
    pub fn dial(&self, addr: &str) -> bool {
        addr.parse::<SocketAddr>()
            .is_ok_and(|a| self.handle.gossip(a).is_ok())
    }

    /// Every node this one currently believes is live, by advertised address.
    pub async fn members(&self) -> Vec<String> {
        // ⚠️ `live_nodes` excludes self, which would leave every node's view one short and
        // make convergence look permanently incomplete — a measurement bug that reads as a
        // protocol bug.
        let mut out: Vec<String> = self
            .handle
            .with_chitchat(|c| {
                c.live_nodes()
                    .map(|id| id.gossip_advertise_addr.to_string())
                    .collect::<Vec<_>>()
            })
            .await;
        out.push(self.handle.chitchat_id().gossip_advertise_addr.to_string());
        out.sort();
        out.dedup();
        out
    }
}

/// Joins the mesh, seeded from the roster.
pub async fn start(
    node_id: &str,
    listen: &str,
    advertise: &str,
    seeds: &[String],
    loss: f64,
    period: std::time::Duration,
) -> Result<Member, Box<dyn std::error::Error>> {
    let listen: SocketAddr = listen.parse()?;
    // ⚠️ Resolved, not parsed. The advertised address is how peers reach this node, and in a
    // container fleet that is a service NAME — `pstore-n5:7946`. `SocketAddr::parse` wants a
    // literal IP and fails on it, which surfaces as every node exiting at startup and looks
    // like a crash loop rather than a name that was never resolved.
    let resolved: SocketAddr = tokio::net::lookup_host(advertise)
        .await?
        .next()
        .ok_or_else(|| format!("{advertise} resolved to nothing"))?;
    let config = ChitchatConfig {
        chitchat_id: ChitchatId::new(node_id.to_owned(), 0, resolved),
        cluster_id: "pstore".to_owned(),
        gossip_interval: period,
        listen_addr: listen,
        seed_nodes: seeds.to_vec(),
        failure_detector_config: FailureDetectorConfig::default(),
        marked_for_deletion_grace_period: std::time::Duration::from_secs(60),
        protocol_version: chitchat::ProtocolVersion::V1,
        catchup_callback: None,
        extra_liveness_predicate: None,
    };
    // ⚠️ Seeded from the node id, not from a clock or a constant. A constant would give a
    // hundred nodes one drop pattern — every node dropping the same probe in the same
    // period is a synchronised partition, not 10% loss — and a clock would make a failure
    // unreplayable.
    let seed = crate::fnv1a(node_id.as_bytes());
    let transport = Metered::new(UdpTransport, loss, seed);
    let stats = transport.stats();
    let handle = chitchat::spawn_chitchat(config, Vec::new(), &transport).await?;
    Ok(Member { handle, stats })
}
