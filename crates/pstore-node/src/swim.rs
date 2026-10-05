//! A UDP driver for `pstore_gossip::Protocol`.
//!
//! ⚠️ The protocol itself has no I/O and is tested without a socket; this is the thin part
//! that a test cannot reach, and it is deliberately thin for that reason. Anything with a
//! branch worth being wrong about belongs in `pstore-gossip`, not here.

use crate::seal::{Header, Keys, Replay};
use crate::transport::Bernoulli;
use pstore_gossip::{Cluster, Message, NodeId, Protocol, State};
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
    /// Datagrams refused by the seal (M53): forged, for another node, stale, or replayed.
    refused: AtomicU64,
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

    /// Datagrams refused by the seal since start (M53). Kept out of [`Self::traffic`], whose
    /// three fields the chitchat path shares.
    #[must_use]
    pub fn refused(&self) -> u64 {
        self.stats.refused.load(Ordering::Relaxed)
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
    let h = crate::fnv1a(addr.as_bytes());
    let mut out = [0u8; 16];
    out[..8].copy_from_slice(&h.to_le_bytes());
    out[8..].copy_from_slice(&h.rotate_left(17).to_le_bytes());
    Ok(out)
}

/// The node a datagram to `to` is sealed for (M53).
///
/// - A reply to the source of a datagram just opened is for that datagram's sealer, which the
///   seal verified -- whatever the source address says, behind NAT or a `0.0.0.0` bind.
/// - Otherwise the member the view holds at `to`, else the id `to` derives, as seeds are
///   joined. ⚠️ A member not declared dead first (code review): a seed joined under a
///   derived id can share its address with the id the node really has, and a datagram sealed
///   for the dead one is refused by the live one.
#[must_use]
pub fn dest_for(to: &str, reply: Option<(&str, NodeId)>, cluster: &Cluster) -> NodeId {
    if let Some((source, sealer)) = reply
        && source == to
    {
        return sealer;
    }
    let mut at = cluster.members().filter(|m| m.addr == to);
    let first = at.next();
    first
        .filter(|m| m.state != State::Dead)
        .or_else(|| at.find(|m| m.state != State::Dead))
        .or(first)
        .map(|m| m.id)
        .or_else(|| derive_id(to).ok())
        .unwrap_or_default()
}

/// What a keyed node seals with (M53): its keys, its id, its epoch and its counter.
struct Sealing {
    keys: Keys,
    me: NodeId,
    epoch: u64,
    counter: AtomicU64,
}

impl Sealing {
    /// ⚠️ Called immediately before the datagram's own `send_to`, never for a batch ahead of
    /// sending, so the counters a peer sees are out of order by no more than the sends in
    /// flight -- well inside the replay window (spec review).
    fn seal(&self, payload: &[u8], dest: NodeId) -> Vec<u8> {
        let h = Header {
            sealer: self.me,
            dest,
            epoch: self.epoch,
            counter: self.counter.fetch_add(1, Ordering::Relaxed),
            time: now_micros(),
        };
        crate::seal::seal(&self.keys, payload, &h)
    }
}

/// Microseconds since the Unix epoch, by this host's clock.
fn now_micros() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_micros()).unwrap_or(u64::MAX))
}

/// Bind, and start probing, unauthenticated.
pub async fn start(
    listen: &str,
    advertise: &str,
    zone: &str,
    seeds: &[String],
    loss: f64,
    period: std::time::Duration,
) -> Result<Member, Box<dyn std::error::Error>> {
    start_with(listen, advertise, zone, seeds, loss, period, None).await
}

/// Bind, and start probing; with `keys`, every datagram is sealed and every one received must
/// open (M53).
pub async fn start_with(
    listen: &str,
    advertise: &str,
    zone: &str,
    seeds: &[String],
    loss: f64,
    period: std::time::Duration,
    keys: Option<Keys>,
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

    let sealing = keys.map(|keys| {
        Arc::new(Sealing {
            keys,
            me,
            epoch: now_micros(),
            counter: AtomicU64::new(0),
        })
    });
    spawn_receiver(&socket, &proto, &stats, loss, seed, sealing.clone());
    spawn_ticker(&socket, &proto, &stats, loss, seed, period, sealing);

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
    sealing: Option<Arc<Sealing>>,
) {
    let (socket, proto, stats) = (Arc::clone(socket), Arc::clone(proto), Arc::clone(stats));
    tokio::spawn(async move {
        // The largest message the protocol sends, never a size of our own (M44), plus the
        // seal it reserves room for (M53): an answer that would exceed it goes as several
        // `Part`s, so sender and reader cannot disagree. Only a member whose address alone is
        // over ~65 KB can still fail to send.
        let mut buf = vec![0u8; pstore_gossip::MAX_DATAGRAM + pstore_gossip::SEAL];
        let mut rng = Bernoulli::new(seed);
        let mut replay = Replay::default();
        loop {
            let Ok((n, from)) = socket.recv_from(&mut buf).await else {
                continue;
            };
            stats.recvd.fetch_add(n as u64, Ordering::Relaxed);
            let Some(frame) = buf.get(..n) else {
                continue;
            };
            let source = from.to_string();
            let (payload, sealer) = match &sealing {
                None => (frame, None),
                Some(s) => {
                    let me = crate::seal::Me {
                        id: s.me,
                        since: s.epoch,
                    };
                    match crate::seal::admit(&s.keys, &me, &mut replay, frame, now_micros()) {
                        Some((payload, h)) => (payload, Some(h.sealer)),
                        None => {
                            stats.refused.fetch_add(1, Ordering::Relaxed);
                            continue;
                        }
                    }
                }
            };
            let Some(msg) = Message::decode(payload) else {
                continue;
            };
            let replies = {
                let mut p = proto.lock().await;
                let out = p.receive(&source, &msg);
                addressed(
                    out,
                    sealing.is_some(),
                    sealer.map(|s| (source.as_str(), s)),
                    p.cluster(),
                )
            };
            send_all(&socket, &stats, replies, loss, &mut rng, sealing.as_deref()).await;
        }
    });
}

/// Each outgoing datagram with the node it is for, when the transport seals (M53).
fn addressed(
    out: Vec<(String, Message)>,
    keyed: bool,
    reply: Option<(&str, NodeId)>,
    cluster: &Cluster,
) -> Vec<(String, NodeId, Message)> {
    out.into_iter()
        .map(|(to, m)| {
            let dest = if keyed {
                dest_for(&to, reply, cluster)
            } else {
                NodeId::default()
            };
            (to, dest, m)
        })
        .collect()
}

fn spawn_ticker(
    socket: &Arc<UdpSocket>,
    proto: &Arc<Mutex<Protocol>>,
    stats: &Arc<Stats>,
    loss: f64,
    seed: u64,
    period: std::time::Duration,
    sealing: Option<Arc<Sealing>>,
) {
    let (socket, proto, stats) = (Arc::clone(socket), Arc::clone(proto), Arc::clone(stats));
    tokio::spawn(async move {
        let mut rng = Bernoulli::new(seed);
        let mut round = 0u64;
        loop {
            tokio::time::sleep(period).await;
            round = round.wrapping_add(1);
            let out = {
                let mut p = proto.lock().await;
                let out = p.tick(seed.wrapping_add(round));
                addressed(out, sealing.is_some(), None, p.cluster())
            };
            send_all(&socket, &stats, out, loss, &mut rng, sealing.as_deref()).await;
        }
    });
}

/// Drop a fraction of outbound datagrams, and count the rest.
///
/// ⚠️ Loss is applied to **outbound** only. Dropping inbound would count the bytes before
/// discarding them and inflate the traffic figure by exactly the loss rate.
///
/// ⚠️ **The same `Bernoulli` the chitchat path's `Metered` uses** (M8e). This used to be a
/// hand-rolled copy of its xorshift, seeded with the `seed | 1` that `Bernoulli`'s own comment
/// records as lossy -- two generators for one job, and only one of them tested.
async fn send_all(
    socket: &UdpSocket,
    stats: &Stats,
    out: Vec<(String, NodeId, Message)>,
    loss: f64,
    rng: &mut Bernoulli,
    sealing: Option<&Sealing>,
) {
    for (to, dest, msg) in out {
        if rng.fires(loss) {
            stats.dropped.fetch_add(1, Ordering::Relaxed);
            continue;
        }
        let bytes = match sealing {
            None => msg.encode(),
            Some(s) => s.seal(&msg.encode(), dest),
        };
        if socket.send_to(&bytes, &to).await.is_ok() {
            stats.sent.fetch_add(bytes.len() as u64, Ordering::Relaxed);
        }
    }
}
