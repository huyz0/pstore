//! A UDP driver for `pstore_gossip::Protocol`.
//!
//! ⚠️ The protocol itself has no I/O and is tested without a socket; this is the thin part
//! that a test cannot reach, and it is deliberately thin for that reason. Anything with a
//! branch worth being wrong about belongs in `pstore-gossip`, not here.

use pstore_gossip::{Cluster, Message, NodeId, Protocol};
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::net::UdpSocket;
use tokio::sync::Mutex;

/// Gossip traffic, counted at the socket — the same accounting the chitchat path reports, so
/// the two are comparable on one harness.
#[derive(Debug, Default)]
pub struct Stats {
    sent: AtomicU64,
    recvd: AtomicU64,
    dropped: AtomicU64,
}

impl Stats {
    /// Bytes out, bytes in, and datagrams deliberately discarded.
    #[must_use]
    pub fn read(&self) -> (u64, u64, u64) {
        (
            self.sent.load(Ordering::Relaxed),
            self.recvd.load(Ordering::Relaxed),
            self.dropped.load(Ordering::Relaxed),
        )
    }
}

/// A running member.
pub struct Member {
    proto: Arc<Mutex<Protocol>>,
    stats: Arc<Stats>,
    me: String,
}

/// The largest datagram we will read.
///
/// ⚠️ A reconciliation carries the whole member set, so this bounds the fleet a single `Sync`
/// can describe: ~64 bytes a member puts 10,000 members past it. Reconciliation is the rare
/// path and a truncated one is refused rather than half-applied, so the failure is a retry
/// rather than a wrong member set — but it is a ceiling, and it is written down.
const MAX_DATAGRAM: usize = 65_507;

impl Member {
    /// The address peers reach this node on.
    #[must_use]
    pub fn self_addr(&self) -> String {
        self.me.clone()
    }

    /// How many peers this node believes are reachable.
    pub async fn member_count(&self) -> usize {
        self.proto.lock().await.cluster().alive_count()
    }

    /// Everyone this node believes is reachable, by advertised address.
    pub async fn members(&self) -> Vec<String> {
        self.proto
            .lock()
            .await
            .cluster()
            .alive()
            .into_iter()
            .map(|m| m.addr.clone())
            .collect()
    }

    /// Every reachable peer with the zone it declared.
    ///
    /// ⚠️ The zone is what makes a cell's roster a cell's roster. Without it a node writes
    /// every zone's members into its own cell and places across AZs anyway — a per-cell
    /// roster *address* with fleet-wide *contents*.
    pub async fn members_zoned(&self) -> Vec<(String, String)> {
        self.proto
            .lock()
            .await
            .cluster()
            .alive()
            .into_iter()
            .map(|m| (m.addr.clone(), m.zone.clone()))
            .collect()
    }

    /// Bytes sent, received, and dropped since start.
    #[must_use]
    pub fn traffic(&self) -> (u64, u64, u64) {
        self.stats.read()
    }

    /// Learn of a peer out of band — the roster, which is the partition-healing backstop.
    pub async fn dial(&self, addr: &str) -> bool {
        let Ok(id) = derive_id(addr) else {
            return false;
        };
        // ⚠️ Zone unknown: the roster gives an address, not a zone. Gossip supplies it.
        self.proto
            .lock()
            .await
            .cluster_mut()
            .join(id, addr.to_owned(), String::new());
        true
    }
}

/// A node's identity, derived from the address it advertises.
///
/// ⚠️ **Not a fresh uuid, and this is a deviation from `membership.md` worth stating.** The
/// roster stores addresses, so a node reading it learns an address and no identity; deriving
/// one lets the backstop work without a second lookup. The cost is that a restarted node
/// reuses its identity, which `membership.md` warns makes a cold node look warm. That matters
/// for *cache* placement, which is M4d — and M4d is where it has to be fixed, by carrying
/// identity in the roster rather than by guessing it here.
fn derive_id(addr: &str) -> Result<NodeId, ()> {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in addr.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    let mut out = [0u8; 16];
    out[..8].copy_from_slice(&h.to_le_bytes());
    out[8..].copy_from_slice(&h.rotate_left(17).to_le_bytes());
    Ok(out)
}

/// Bind, and start probing.
pub async fn start(
    listen: &str,
    advertise: &str,
    zone: &str,
    seeds: &[String],
    loss: f64,
    period: std::time::Duration,
) -> Result<Member, Box<dyn std::error::Error>> {
    let listen: SocketAddr = listen.parse()?;
    let socket = Arc::new(UdpSocket::bind(listen).await?);

    let me = derive_id(advertise).map_err(|()| "unusable advertise address")?;
    let mut cluster = Cluster::new(me, advertise.to_owned(), zone.to_owned());
    for s in seeds {
        if let Ok(id) = derive_id(s) {
            cluster.join(id, s.clone(), String::new());
        }
    }

    let proto = Arc::new(Mutex::new(Protocol::new(cluster)));
    let stats = Arc::new(Stats::default());

    // Deterministic per node, so an injected-loss run is replayable.
    let seed = u64::from_le_bytes(
        me.get(..8)
            .and_then(|s| s.try_into().ok())
            .unwrap_or([0; 8]),
    );

    spawn_receiver(&socket, &proto, &stats, loss, seed);
    spawn_ticker(&socket, &proto, &stats, loss, seed, period);

    Ok(Member {
        proto,
        stats,
        me: advertise.to_owned(),
    })
}

fn spawn_receiver(
    socket: &Arc<UdpSocket>,
    proto: &Arc<Mutex<Protocol>>,
    stats: &Arc<Stats>,
    loss: f64,
    seed: u64,
) {
    let (socket, proto, stats) = (Arc::clone(socket), Arc::clone(proto), Arc::clone(stats));
    tokio::spawn(async move {
        let mut buf = vec![0u8; MAX_DATAGRAM];
        let mut rng = seed | 1;
        loop {
            let Ok((n, from)) = socket.recv_from(&mut buf).await else {
                continue;
            };
            stats.recvd.fetch_add(n as u64, Ordering::Relaxed);
            let Some(msg) = buf.get(..n).and_then(Message::decode) else {
                continue;
            };
            let replies = proto.lock().await.receive(&from.to_string(), &msg);
            send_all(&socket, &stats, replies, loss, &mut rng).await;
        }
    });
}

fn spawn_ticker(
    socket: &Arc<UdpSocket>,
    proto: &Arc<Mutex<Protocol>>,
    stats: &Arc<Stats>,
    loss: f64,
    seed: u64,
    period: std::time::Duration,
) {
    let (socket, proto, stats) = (Arc::clone(socket), Arc::clone(proto), Arc::clone(stats));
    tokio::spawn(async move {
        let mut rng = seed | 1;
        let mut round = 0u64;
        loop {
            tokio::time::sleep(period).await;
            round = round.wrapping_add(1);
            let out = proto.lock().await.tick(seed.wrapping_add(round));
            send_all(&socket, &stats, out, loss, &mut rng).await;
        }
    });
}

/// Drop a fraction of outbound datagrams, and count the rest.
///
/// ⚠️ Loss is applied to **outbound** only. Dropping inbound would count the bytes before
/// discarding them and inflate the traffic figure by exactly the loss rate.
async fn send_all(
    socket: &UdpSocket,
    stats: &Stats,
    out: Vec<(String, Message)>,
    loss: f64,
    rng: &mut u64,
) {
    for (to, msg) in out {
        *rng ^= *rng << 13;
        *rng ^= *rng >> 7;
        *rng ^= *rng << 17;
        #[expect(
            clippy::cast_precision_loss,
            reason = "a 53-bit mantissa is finer than any loss rate worth injecting"
        )]
        let draw = (*rng >> 11) as f64 / (1u64 << 53) as f64;
        if draw < loss {
            stats.dropped.fetch_add(1, Ordering::Relaxed);
            continue;
        }
        let bytes = msg.encode();
        if socket.send_to(&bytes, &to).await.is_ok() {
            stats.sent.fetch_add(bytes.len() as u64, Ordering::Relaxed);
        }
    }
}
