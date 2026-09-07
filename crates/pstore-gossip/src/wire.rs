//! The wire format.
//!
//! ⚠️ Hand-rolled rather than `serde` + a format crate, for one reason: **the steady-state
//! size is the milestone**. A converged probe has to be tens of bytes, and a self-describing
//! format spends most of a small message describing itself. It is also one fewer dependency
//! on the path that every node runs constantly.
//!
//! Every integer is little-endian and fixed-width. Lengths are `u32`. There are no optional
//! fields, so a decoder never has to guess.

use crate::cluster::{Member, NodeId, State};

/// One datagram.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    /// "Are you there, and do we agree?" — the entire steady state.
    Ping {
        /// Who is asking.
        from: NodeId,
        /// Matches the ack to the probe, so a late reply cannot answer a later question.
        seq: u64,
        /// The sender's whole view, in eight bytes.
        checksum: u64,
        /// Changes worth piggybacking. Empty when nothing changed, which is almost always.
        updates: Vec<Member>,
    },
    /// "I am here." Liveness is this message arriving, never a counter inside it.
    Ack {
        /// Who is answering.
        from: NodeId,
        /// Echoed from the ping.
        seq: u64,
        /// The responder's view, for comparison against the pinger's.
        checksum: u64,
        /// Changes worth piggybacking.
        updates: Vec<Member>,
    },
    /// "Probe this peer for me" — the indirect path, for when a direct probe fails.
    PingReq {
        /// Who is asking.
        from: NodeId,
        /// Matches the eventual ack.
        seq: u64,
        /// The peer to probe.
        target: NodeId,
    },
    /// Full reconciliation. Sent only when two checksums disagree.
    Sync {
        /// Who is reconciling.
        from: NodeId,
        /// The sender's members.
        members: Vec<Member>,
    },
}

const PING: u8 = 1;
const ACK: u8 = 2;
const PING_REQ: u8 = 3;
const SYNC: u8 = 4;

fn put_u64(out: &mut Vec<u8>, v: u64) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn put_member(out: &mut Vec<u8>, m: &Member) {
    out.extend_from_slice(&m.id);
    put_u64(out, m.incarnation);
    out.push(match m.state {
        State::Alive => 1,
        State::Suspect => 2,
        State::Dead => 3,
    });
    for field in [m.addr.as_bytes(), m.zone.as_bytes()] {
        // A length that does not fit is a member that cannot be encoded; truncating its
        // address would produce a member nobody can reach, which is worse than refusing to
        // carry it.
        let len = u32::try_from(field.len()).unwrap_or(0);
        out.extend_from_slice(&len.to_le_bytes());
        if len > 0 {
            out.extend_from_slice(field);
        }
    }
}

/// A cursor that refuses to read past the end, so a truncated frame is `None` rather than a
/// shorter message.
struct Reader<'a> {
    buf: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, at: 0 }
    }

    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.at.checked_add(n)?;
        let out = self.buf.get(self.at..end)?;
        self.at = end;
        Some(out)
    }

    fn u8(&mut self) -> Option<u8> {
        self.take(1)?.first().copied()
    }

    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }

    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }

    fn id(&mut self) -> Option<NodeId> {
        self.take(16)?.try_into().ok()
    }

    fn member(&mut self) -> Option<Member> {
        let id = self.id()?;
        let incarnation = self.u64()?;
        let state = match self.u8()? {
            1 => State::Alive,
            2 => State::Suspect,
            3 => State::Dead,
            // ⚠️ Refused, never defaulted. A state this build does not know is a claim about
            // a member's liveness, and guessing at one invents it.
            _ => return None,
        };
        let len = self.u32()? as usize;
        let addr = String::from_utf8(self.take(len)?.to_vec()).ok()?;
        // ⚠️ Refused when absent, never defaulted. A guessed zone puts a node in the wrong
        // cell — and a wrong cell is a ring it does not belong to, which is silent.
        let zlen = self.u32()? as usize;
        let zone = String::from_utf8(self.take(zlen)?.to_vec()).ok()?;
        Some(Member {
            id,
            addr,
            zone,
            incarnation,
            state,
        })
    }

    fn members(&mut self) -> Option<Vec<Member>> {
        let n = self.u32()? as usize;
        // ⚠️ Bounded by what is actually left. A hostile or corrupt length would otherwise
        // reserve gigabytes before the first read fails.
        if n > self.buf.len() {
            return None;
        }
        (0..n).map(|_| self.member()).collect()
    }

    /// Whether the frame ended exactly here.
    ///
    /// ⚠️ Trailing bytes are refused. They mean the sender and this decoder disagree about the
    /// format, and a decoder that ignores the disagreement carries it forward silently.
    fn finished(&self) -> bool {
        self.at == self.buf.len()
    }
}

impl Message {
    /// Who sent it.
    #[must_use]
    pub fn from(&self) -> NodeId {
        match self {
            Self::Ping { from, .. }
            | Self::Ack { from, .. }
            | Self::PingReq { from, .. }
            | Self::Sync { from, .. } => *from,
        }
    }

    /// Encode for the wire.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        match self {
            Self::Ping {
                from,
                seq,
                checksum,
                updates,
            }
            | Self::Ack {
                from,
                seq,
                checksum,
                updates,
            } => {
                out.push(if matches!(self, Self::Ping { .. }) {
                    PING
                } else {
                    ACK
                });
                out.extend_from_slice(from);
                put_u64(&mut out, *seq);
                put_u64(&mut out, *checksum);
                out.extend_from_slice(&(updates.len() as u32).to_le_bytes());
                for m in updates {
                    put_member(&mut out, m);
                }
            }
            Self::PingReq { from, seq, target } => {
                out.push(PING_REQ);
                out.extend_from_slice(from);
                put_u64(&mut out, *seq);
                out.extend_from_slice(target);
            }
            Self::Sync { from, members } => {
                out.push(SYNC);
                out.extend_from_slice(from);
                out.extend_from_slice(&(members.len() as u32).to_le_bytes());
                for m in members {
                    put_member(&mut out, m);
                }
            }
        }
        out
    }

    /// Decode a frame, or `None` if it is not exactly one valid message.
    #[must_use]
    pub fn decode(buf: &[u8]) -> Option<Self> {
        let mut r = Reader::new(buf);
        let msg = match r.u8()? {
            PING | ACK => {
                let tag = buf.first().copied()?;
                let from = r.id()?;
                let seq = r.u64()?;
                let checksum = r.u64()?;
                let updates = r.members()?;
                if tag == PING {
                    Self::Ping {
                        from,
                        seq,
                        checksum,
                        updates,
                    }
                } else {
                    Self::Ack {
                        from,
                        seq,
                        checksum,
                        updates,
                    }
                }
            }
            PING_REQ => Self::PingReq {
                from: r.id()?,
                seq: r.u64()?,
                target: r.id()?,
            },
            SYNC => Self::Sync {
                from: r.id()?,
                members: r.members()?,
            },
            _ => return None,
        };
        r.finished().then_some(msg)
    }
}
