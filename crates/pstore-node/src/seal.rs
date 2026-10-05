//! Sealed gossip datagrams (M53): HMAC-SHA256 under a cluster key, a destination, and a
//! replay window, so a datagram is believed only if a key holder sent it, to this node, once.
//!
//! The frame is `payload || sealer || dest || epoch || counter || time || tag`:
//! [`SEAL`](pstore_gossip::SEAL) bytes after the payload, which `pstore-gossip` reserves out
//! of every datagram.
//!
//! ⚠️ **Replay is keyed by `(sealer, epoch)`, never by the payload's `from`.** A relayed
//! indirect-probe `Ack` names the probed target while the relay seals it, so a window keyed
//! by `from` would mix two senders' counters and refuse one of them (spec review).

use pstore_gossip::NodeId;
use ring::hmac;
use std::collections::HashMap;

/// Prefixed to every MAC input, so a tag made for another use of the key never verifies here.
const LABEL: &[u8] = b"pstore-gossip-1";
/// The header fields after the payload, before the tag: two ids and three `u64`s.
const HEADER: usize = 16 + 16 + 8 + 8 + 8;
/// HMAC-SHA256's output, kept whole.
const TAG: usize = 32;
/// How far a seal's time may be from the receiver's clock, either way.
pub const FRESH_MICROS: u64 = 60_000_000;
/// How far below the highest counter a reordered datagram is still accepted: 1 to 63.
const WINDOW: u64 = 64;
/// Live epochs kept per sealer: a restart overlaps its old epoch, and nothing honest needs more.
const EPOCHS: usize = 2;
/// The least time between sweeps of the replay table.
const SWEEP_MICROS: u64 = 1_000_000;

/// Who sealed a datagram, for whom, and when.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    /// The sending node's id.
    pub sealer: NodeId,
    /// The id of the node it is for.
    pub dest: NodeId,
    /// The sender's transport start, in microseconds since the Unix epoch.
    pub epoch: u64,
    /// Datagrams sealed before this one in the epoch.
    pub counter: u64,
    /// When it was sealed, in microseconds since the Unix epoch.
    pub time: u64,
}

/// The cluster keys: the first seals, and any opens.
pub struct Keys {
    seal: hmac::Key,
    open: Vec<hmac::Key>,
}

impl Keys {
    /// `None` without a key.
    #[must_use]
    pub fn new(keys: &[Vec<u8>]) -> Option<Self> {
        let first = keys.first()?;
        Some(Self {
            seal: hmac::Key::new(hmac::HMAC_SHA256, first),
            open: keys
                .iter()
                .map(|k| hmac::Key::new(hmac::HMAC_SHA256, k))
                .collect(),
        })
    }
}

/// `payload`, sealed.
#[must_use]
pub fn seal(keys: &Keys, payload: &[u8], h: &Header) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + HEADER + TAG);
    out.extend_from_slice(payload);
    out.extend_from_slice(&h.sealer);
    out.extend_from_slice(&h.dest);
    out.extend_from_slice(&h.epoch.to_le_bytes());
    out.extend_from_slice(&h.counter.to_le_bytes());
    out.extend_from_slice(&h.time.to_le_bytes());
    let mut ctx = hmac::Context::with_key(&keys.seal);
    ctx.update(LABEL);
    ctx.update(&out);
    out.extend_from_slice(ctx.sign().as_ref());
    out
}

/// The payload and header of a frame whose tag verifies under one of `keys`; `None` for
/// anything else. The tag is checked, in constant time, before any field is read.
#[must_use]
pub fn open<'a>(keys: &Keys, frame: &'a [u8]) -> Option<(&'a [u8], Header)> {
    let body_len = frame.len().checked_sub(TAG)?;
    let (body, tag) = frame.split_at_checked(body_len)?;
    let payload_len = body.len().checked_sub(HEADER)?;
    let mut msg = Vec::with_capacity(LABEL.len() + body.len());
    msg.extend_from_slice(LABEL);
    msg.extend_from_slice(body);
    if !keys.open.iter().any(|k| hmac::verify(k, &msg, tag).is_ok()) {
        return None;
    }
    let (payload, header) = body.split_at_checked(payload_len)?;
    let id = |at: usize| -> Option<NodeId> { header.get(at..at + 16)?.try_into().ok() };
    let word = |at: usize| -> Option<u64> {
        Some(u64::from_le_bytes(header.get(at..at + 8)?.try_into().ok()?))
    };
    Some((
        payload,
        Header {
            sealer: id(0)?,
            dest: id(16)?,
            epoch: word(32)?,
            counter: word(40)?,
            time: word(48)?,
        },
    ))
}

/// Whether a seal made at `time` is fresh at `now`: within [`FRESH_MICROS`], either way.
#[must_use]
pub fn fresh(time: u64, now: u64) -> bool {
    time.abs_diff(now) <= FRESH_MICROS
}

/// Who a node is, for [`admit`]: its id, and when its transport started.
#[derive(Clone, Copy, Debug)]
pub struct Me {
    /// The id a frame must be sealed for.
    pub id: NodeId,
    /// This transport's start, in microseconds since the Unix epoch.
    pub since: u64,
}

/// The payload and header of `frame` if it may reach the protocol (M53): its seal verifies,
/// it is for `me`, it was sealed no earlier than `me` started and is fresh at `now`, and
/// `replay` has not seen it. `replay` records it.
///
/// ⚠️ **Sealed before this node started is refused** (code review). A node keeps its id
/// across a restart and its replay table does not survive one, so a datagram captured
/// within the minute before and replayed after would otherwise be believed again. The cost
/// is a sender whose clock runs s behind this one's: refused for s after this node starts.
pub fn admit<'a>(
    keys: &Keys,
    me: &Me,
    replay: &mut Replay,
    frame: &'a [u8],
    now: u64,
) -> Option<(&'a [u8], Header)> {
    let (payload, h) = open(keys, frame)?;
    (h.dest == me.id && h.time >= me.since && fresh(h.time, now) && replay.accept(&h, now))
        .then_some((payload, h))
}

/// One epoch's counters seen: the highest, and a bit for each of the 63 below it.
#[derive(Debug)]
struct Window {
    epoch: u64,
    highest: u64,
    seen: u64,
    newest: u64,
}

/// One sealer's live epochs, and the highest epoch it has had evicted.
#[derive(Debug, Default)]
struct Sealer {
    floor: Option<u64>,
    epochs: Vec<Window>,
}

/// The counters a node has accepted, per `(sealer, epoch)`.
#[derive(Debug, Default)]
pub struct Replay {
    sealers: HashMap<NodeId, Sealer>,
    swept: u64,
}

impl Replay {
    /// Whether `h` is new, recording it if so. `now` drives the sweep.
    pub fn accept(&mut self, h: &Header, now: u64) -> bool {
        self.sweep(now);
        let s = self.sealers.entry(h.sealer).or_default();
        if s.floor.is_some_and(|f| h.epoch <= f) {
            return false;
        }
        // With two epochs held, one older than both is a replay, and evicts nothing.
        if s.epochs.len() >= EPOCHS && s.epochs.iter().all(|w| h.epoch < w.epoch) {
            return false;
        }
        let Some(w) = s.epochs.iter_mut().find(|w| w.epoch == h.epoch) else {
            // A new epoch. When the sealer already has its two, the older goes, and its epoch
            // becomes the floor.
            if s.epochs.len() >= EPOCHS
                && let Some((i, lowest)) = s
                    .epochs
                    .iter()
                    .enumerate()
                    .map(|(i, w)| (i, w.epoch))
                    .min_by_key(|(_, e)| *e)
            {
                s.epochs.swap_remove(i);
                s.floor = Some(s.floor.map_or(lowest, |f| f.max(lowest)));
            }
            s.epochs.push(Window {
                epoch: h.epoch,
                highest: h.counter,
                seen: 1,
                newest: h.time,
            });
            return true;
        };
        if h.counter > w.highest {
            let shift = h.counter - w.highest;
            w.seen = if shift >= WINDOW { 0 } else { w.seen << shift };
            w.seen |= 1;
            w.highest = h.counter;
        } else {
            let below = w.highest - h.counter;
            if below >= WINDOW || w.seen & (1 << below) != 0 {
                return false;
            }
            w.seen |= 1 << below;
        }
        w.newest = w.newest.max(h.time);
        true
    }

    /// Entries whose newest time is past [`FRESH_MICROS`], and sealers left with none, are
    /// dropped: freshness already refuses anything they could match.
    fn sweep(&mut self, now: u64) {
        if now.saturating_sub(self.swept) < SWEEP_MICROS {
            return;
        }
        self.swept = now;
        for s in self.sealers.values_mut() {
            s.epochs.retain(|w| fresh(w.newest, now));
        }
        self.sealers.retain(|_, s| !s.epochs.is_empty());
    }

    /// Epochs held, over every sealer.
    #[must_use]
    pub fn len(&self) -> usize {
        self.sealers.values().map(|s| s.epochs.len()).sum()
    }

    /// Whether nothing is held.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.sealers.is_empty()
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]
mod tests {
    use super::*;

    const NOW: u64 = 1_800_000_000_000_000;
    const ME: NodeId = [2; 16];
    /// This node, started long enough ago that no test frame predates it.
    const AT: Me = Me { id: ME, since: 0 };

    fn keys(k: &[&[u8]]) -> Keys {
        Keys::new(&k.iter().map(|k| k.to_vec()).collect::<Vec<_>>()).unwrap()
    }

    fn key(b: u8) -> Vec<u8> {
        vec![b; 32]
    }

    fn header(epoch: u64, counter: u64) -> Header {
        Header {
            sealer: [1; 16],
            dest: ME,
            epoch,
            counter,
            time: NOW,
        }
    }

    #[test]
    fn a_seal_opens_only_unaltered_under_its_key() {
        // Golden vector, computed outside the crate with Python's `hmac`:
        //   key = bytes(range(32)); body = b"pstore" + [1]*16 + [2]*16 + <QQQ(3, 4, 5)
        //   tag = hmac.new(key, b"pstore-gossip-1" + body, sha256)
        // The label is written here as a literal, so a change to it fails this test.
        let golden = keys(&[&(0..32).collect::<Vec<u8>>()]);
        let h = Header {
            sealer: [1; 16],
            dest: [2; 16],
            epoch: 3,
            counter: 4,
            time: 5,
        };
        let frame = seal(&golden, b"pstore", &h);
        let hex: String = frame.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(
            hex,
            "7073746f72650101010101010101010101010101010102020202020202020202020202020202\
             0300000000000000040000000000000005000000000000004\
             5d664426caa8f36021c0f9c1e6ca520fcb05123a51c4b3216e1c345cc409bd7"
        );
        assert_eq!(LABEL, b"pstore-gossip-1");
        assert_eq!(frame.len(), 6 + pstore_gossip::SEAL);

        let ours = keys(&[&key(7)]);
        let payload: Vec<u8> = (0..100).collect();
        let sealed = seal(&ours, &payload, &header(1, 0));
        assert_eq!(open(&ours, &sealed), Some((&payload[..], header(1, 0))));
        // A second accepted key opens it too, either way round.
        assert!(open(&keys(&[&key(9), &key(7)]), &sealed).is_some());
        assert!(open(&keys(&[&key(9)]), &sealed).is_none(), "another key");
        for i in 0..sealed.len() {
            let mut bent = sealed.clone();
            bent[i] ^= 1;
            assert!(open(&ours, &bent).is_none(), "byte {i} flipped");
        }
        for n in 0..pstore_gossip::SEAL {
            assert!(open(&ours, &sealed[..n]).is_none(), "{n} bytes");
        }
        // An empty payload is still a whole frame.
        assert!(open(&ours, &seal(&ours, &[], &header(1, 1))).is_some());
        // Sealed for another node: the seal is good, and it is still refused.
        let mut elsewhere = header(1, 2);
        elsewhere.dest = [3; 16];
        let frame = seal(&ours, &payload, &elsewhere);
        assert!(open(&ours, &frame).is_some());
        assert!(admit(&ours, &AT, &mut Replay::default(), &frame, NOW).is_none());
        assert!(admit(&ours, &AT, &mut Replay::default(), &sealed, NOW).is_some());
    }

    #[test]
    fn a_stale_seal_is_refused() {
        let k = keys(&[&key(7)]);
        for (time, ok) in [
            (NOW - FRESH_MICROS, true),
            (NOW + FRESH_MICROS, true),
            (NOW - FRESH_MICROS - 1, false),
            (NOW + FRESH_MICROS + 1, false),
        ] {
            let mut h = header(1, 0);
            h.time = time;
            let frame = seal(&k, b"x", &h);
            let got = admit(&k, &AT, &mut Replay::default(), &frame, NOW).is_some();
            assert_eq!(got, ok, "time {time} at {NOW}");
        }
    }

    #[test]
    fn a_frame_sealed_before_this_node_started_is_refused() {
        // Code review: the id survives a restart and the replay table does not.
        let k = keys(&[&key(7)]);
        let me = Me { id: ME, since: NOW };
        for (time, ok) in [(NOW - 1, false), (NOW, true), (NOW + 1, true)] {
            let mut h = header(1, 0);
            h.time = time;
            let frame = seal(&k, b"x", &h);
            let got = admit(&k, &me, &mut Replay::default(), &frame, NOW + 1).is_some();
            assert_eq!(got, ok, "sealed at {time}, started at {NOW}");
        }
    }

    #[test]
    fn a_replay_is_refused_and_reordering_within_the_window_is_not() {
        let mut r = Replay::default();
        let at = |r: &mut Replay, epoch, counter| r.accept(&header(epoch, counter), NOW);
        assert!(at(&mut r, 10, 100));
        assert!(!at(&mut r, 10, 100), "a repeat");
        assert!(at(&mut r, 10, 37), "63 below");
        assert!(!at(&mut r, 10, 37), "63 below, twice");
        assert!(!at(&mut r, 10, 36), "64 below");
        assert!(at(&mut r, 10, 99));
        assert!(at(&mut r, 10, 101));
        // The window moved by one: what it held is still held, the new highest included.
        assert!(!at(&mut r, 10, 101), "the new highest, twice");
        assert!(!at(&mut r, 10, 100), "the old highest, after a move");
        assert!(!at(&mut r, 10, 99), "below it, after a move");
        assert!(at(&mut r, 10, 98), "unseen, 3 below");
        assert!(at(&mut r, 10, 200), "a jump past the window");
        assert!(!at(&mut r, 10, 101), "now 99 below");
        assert!(at(&mut r, 10, 137), "63 below the new highest");
        // A restart: a new epoch is a new window.
        assert!(at(&mut r, 20, 0));
        assert!(at(&mut r, 10, 201), "the old epoch is still live");
        // A third epoch evicts the older of the two, and its epoch becomes the floor.
        assert!(at(&mut r, 30, 0));
        assert!(!at(&mut r, 10, 202), "an evicted epoch");
        assert!(!at(&mut r, 5, 0), "an epoch below the floor");
        assert!(at(&mut r, 20, 1));
        assert_eq!(r.len(), 2);
        // Two sealers never share a window, though their payloads may name the same `from`.
        let mut relay = header(10, 100);
        relay.sealer = [9; 16];
        assert!(
            r.accept(&relay, NOW),
            "the same epoch and counter from another sealer"
        );
        assert!(!r.accept(&relay, NOW));
    }

    #[test]
    fn an_epoch_older_than_both_live_ones_evicts_nothing() {
        let mut r = Replay::default();
        let at = |r: &mut Replay, epoch, counter| r.accept(&header(epoch, counter), NOW);
        assert!(at(&mut r, 20, 0));
        assert!(at(&mut r, 30, 0));
        assert!(!at(&mut r, 10, 0));
        assert!(at(&mut r, 20, 1), "still live");
        assert!(at(&mut r, 30, 1), "still live");
    }

    #[test]
    fn an_evicted_epoch_stays_refused_when_its_sealer_holds_one() {
        // The floor is what refuses an evicted epoch once a sweep has left its sealer a
        // single live epoch, and so room for a new one.
        let mut r = Replay::default();
        let at = |r: &mut Replay, epoch, time| {
            r.accept(
                &Header {
                    time,
                    ..header(epoch, 0)
                },
                time,
            )
        };
        assert!(at(&mut r, 10, NOW));
        assert!(at(&mut r, 20, NOW));
        assert!(at(&mut r, 30, NOW + FRESH_MICROS), "evicts 10");
        let later = NOW + FRESH_MICROS + SWEEP_MICROS;
        let mut again = header(30, 1);
        again.time = later;
        assert!(r.accept(&again, later), "the sweep leaves only 30");
        assert_eq!(r.len(), 1);
        assert!(!at(&mut r, 10, later), "the evicted epoch, at the floor");
        assert!(!at(&mut r, 5, later), "below the floor");
    }

    #[test]
    fn a_quiet_sender_is_forgotten_after_the_window() {
        let mut r = Replay::default();
        assert!(r.is_empty());
        let from = |sealer: u8, time: u64| Header {
            sealer: [sealer; 16],
            time,
            ..header(10, 0)
        };
        assert!(r.accept(&from(1, NOW), NOW));
        // At the window's edge: kept, and its replay still refused.
        assert!(!r.accept(&from(1, NOW), NOW + FRESH_MICROS));
        assert_eq!(r.len(), 1);
        // Half a second on, sealer 1 is stale, but a sweep runs at most once a second.
        let half = NOW + FRESH_MICROS + SWEEP_MICROS / 2;
        assert!(r.accept(&from(2, half), half));
        assert_eq!(r.len(), 2, "no sweep within the second");
        // A second on: swept. Sealer 2, heard from since, stays.
        let next = NOW + FRESH_MICROS + SWEEP_MICROS;
        assert!(r.accept(&from(3, next), next));
        assert_eq!(r.len(), 2, "sealer 1's epoch is gone");
        assert!(!r.is_empty());
        assert!(
            r.accept(&from(1, next), next),
            "a forgotten sealer starts afresh"
        );
        // Everything quiet for the window: nothing held.
        r.sweep(next + FRESH_MICROS + SWEEP_MICROS);
        assert!(r.is_empty());
        assert_eq!(r.len(), 0);
    }
}
