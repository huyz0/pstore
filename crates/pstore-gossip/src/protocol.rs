//! The protocol, as a pure function of state and message.
//!
//! ⚠️ **No I/O, deliberately.** M4b's gossip behaviour was observable only by running a
//! hundred containers, and five bugs were found that way rather than by a test. Everything
//! here takes a message and returns messages, so a 1,000-node fleet is a loop rather than a
//! deployment.

use crate::cluster::{Cluster, Member, NodeId, State};
use crate::wire::Message;
use std::collections::BTreeMap;

/// Pessimism order for equal incarnations: a worse claim wins a tie.
const fn rank(s: State) -> u8 {
    match s {
        State::Alive => 0,
        State::Suspect => 1,
        State::Dead => 2,
    }
}

/// How many periods a probe may go unanswered before its target is suspected.
const PROBE_TIMEOUT: u64 = 3;

/// How many periods a suspect may remain unrefuted before it is declared dead.
///
/// ⚠️ Generous relative to `PROBE_TIMEOUT`, because this is the window in which a node that
/// was merely busy gets to answer. Shortening it is how a loaded fleet evicts its own healthy
/// members — OQ-12, in the form the timeouts control.
const SUSPECT_TIMEOUT: u64 = 6;

/// How many peers are asked to probe on our behalf when a direct probe fails.
const INDIRECT_PROBES: usize = 3;

/// How often a probe is spent on a member believed dead, rather than a live one.
///
/// ⚠️ Not zero. Recovery has to be *possible*, and nothing else in the protocol ever contacts
/// a node it has given up on. Rare enough that the steady-state cost is unchanged: one probe
/// per period is one probe per period, whoever it goes to.
const REVISIT_DEAD_EVERY: u64 = 5;

/// How many times one change rides along before it is retired.
///
/// ⚠️ Finite. Dissemination is best-effort by design; a change that misses every retransmit
/// is repaired by reconciliation, and that is what makes it safe to stop sending it. Keeping
/// changes forever is not caution, it is a permanent tax on every probe.
const RETRANSMITS: u8 = 4;

/// How many changes ride along on a probe.
///
/// ⚠️ Bounded, so a fleet in churn cannot turn a probe back into the O(N) message this crate
/// exists to remove. What does not fit propagates on the next round, one hop later.
const MAX_PIGGYBACK: usize = 6;

/// One node's protocol state.
#[derive(Debug)]
pub struct Protocol {
    cluster: Cluster,
    seq: u64,
    /// Probes awaiting an ack: sequence number to (target, the tick it was sent).
    pending: BTreeMap<u64, (NodeId, u64)>,
    /// When each suspect was first suspected, so it can be given up on.
    suspected_at: BTreeMap<NodeId, u64>,
    /// Recent changes worth telling peers about, newest last, each with the number of times
    /// it has already ridden along.
    updates: Vec<(Member, u8)>,
    tick: u64,
}

impl Protocol {
    /// A protocol driving the given view.
    #[must_use]
    pub fn new(cluster: Cluster) -> Self {
        Self {
            cluster,
            seq: 0,
            pending: BTreeMap::new(),
            suspected_at: BTreeMap::new(),
            updates: Vec::new(),
            tick: 0,
        }
    }

    /// What this node currently believes.
    #[must_use]
    pub fn cluster(&self) -> &Cluster {
        &self.cluster
    }

    /// Mutable access, for a caller that learns of a member out of band — a roster read.
    pub fn cluster_mut(&mut self) -> &mut Cluster {
        &mut self.cluster
    }

    /// One protocol period. Returns the datagrams to send.
    pub fn tick(&mut self, seed: u64) -> Vec<(String, Message)> {
        self.tick += 1;
        let mut out = Vec::new();

        self.expire_probes(&mut out);
        self.bury_suspects();

        // One direct probe per period. ⚠️ ONE, whatever the fleet size: this is the whole of
        // the O(1) claim, and a protocol that probes "a few percent of peers" is linear
        // wearing a constant's clothing.
        if let Some(target) = self.pick_peer(seed) {
            self.seq = self.seq.wrapping_add(1);
            self.pending.insert(self.seq, (target.id, self.tick));
            out.push((
                target.addr.clone(),
                Message::Ping {
                    from: *self.cluster.me(),
                    seq: self.seq,
                    checksum: self.cluster.checksum(),
                    updates: self.piggyback(),
                },
            ));
        }
        out
    }

    /// Handle one datagram from `from_addr`. Returns the datagrams to send in response.
    ///
    /// ⚠️ The sender's address is a **parameter, not a field in the message**. UDP hands the
    /// source address to the receiver for free, so putting it on the wire would pay bytes on
    /// every probe for something already known — and the steady-state size is the point of
    /// this crate. It is also how a node learns of a peer that contacts it before it has
    /// heard of it from anyone else: without that, the first node in a cold fleet receives
    /// probes from everybody and learns of nobody. Measured: it saw 1 of 30.
    pub fn receive(&mut self, from_addr: &str, msg: &Message) -> Vec<(String, Message)> {
        self.learn(msg.from(), from_addr);
        match msg {
            Message::Ping {
                from,
                seq,
                checksum,
                updates,
            } => {
                self.absorb(updates);
                // The ping itself is liveness: it arrived, so its sender is alive.
                self.mark_alive(from);
                let mut out = Vec::new();
                if let Some(addr) = self.addr_of(from) {
                    let mut updates = self.piggyback();
                    updates.extend(self.evidence_for(from));
                    out.push((
                        addr.clone(),
                        Message::Ack {
                            from: *self.cluster.me(),
                            seq: *seq,
                            checksum: self.cluster.checksum(),
                            updates,
                        },
                    ));
                    // ⚠️ Only on disagreement. Sending state alongside every ack is what makes
                    // a checksum decorative.
                    if *checksum != self.cluster.checksum() {
                        out.push((addr, self.sync()));
                    }
                }
                out
            }
            Message::Ack {
                from,
                seq,
                checksum,
                updates,
            } => {
                self.absorb(updates);
                self.pending.remove(seq);
                self.mark_alive(from);
                let mut out = Vec::new();
                let evidence = self.evidence_for(from);
                if let Some(addr) = self.addr_of(from) {
                    // It acked, so it is alive; it does not yet know we had given up on it.
                    if !evidence.is_empty() {
                        self.seq = self.seq.wrapping_add(1);
                        out.push((
                            addr.clone(),
                            Message::Ping {
                                from: *self.cluster.me(),
                                seq: self.seq,
                                checksum: self.cluster.checksum(),
                                updates: evidence,
                            },
                        ));
                    } else if *checksum != self.cluster.checksum() {
                        out.push((addr, self.sync()));
                    }
                }
                out
            }
            Message::PingReq { from, seq, target } => {
                // Probe on someone else's behalf. The target's ack goes to us, and our own
                // view carries the news onward — which is what makes an asymmetric
                // unreachability survivable.
                let mut out = Vec::new();
                if let Some(addr) = self.addr_of(target) {
                    self.seq = self.seq.wrapping_add(1);
                    self.pending.insert(self.seq, (*target, self.tick));
                    out.push((
                        addr,
                        Message::Ping {
                            from: *self.cluster.me(),
                            seq: self.seq,
                            checksum: self.cluster.checksum(),
                            updates: self.piggyback(),
                        },
                    ));
                }
                let _ = (from, seq);
                out
            }
            Message::Sync { from, members } => {
                self.absorb(members);
                self.mark_alive(from);
                // Answer only if we still differ — otherwise two nodes trade syncs forever.
                let mut out = Vec::new();
                if let Some(addr) = self.addr_of(from)
                    && members.len() != self.cluster.len()
                {
                    out.push((addr, self.sync()));
                }
                out
            }
        }
    }

    fn sync(&self) -> Message {
        Message::Sync {
            from: *self.cluster.me(),
            members: self.cluster.members().cloned().collect(),
        }
    }

    /// Merge what a peer told us, under incarnation precedence.
    ///
    /// ⚠️ **A claim is not authority.** An incoming record wins only at a *higher*
    /// incarnation; at an equal one the more pessimistic state wins (Dead beats Suspect beats
    /// Alive), and a lower one is ignored outright as the stale packet it is.
    ///
    /// Applying `Dead` unconditionally is what left a healed partition split: each half held
    /// the other dead, so every reconciliation re-killed everyone it had just revived, and
    /// the two halves fought to a standstill. Measured, the worst node saw 2 of 20.
    fn absorb(&mut self, updates: &[Member]) {
        for m in updates {
            if m.id == *self.cluster.me() {
                // ⚠️ Someone believes we are not alive. Only we can say otherwise, and only by
                // raising our own incarnation above the one their claim carries.
                if m.state != State::Alive {
                    self.refute_self_above(m.incarnation);
                }
                continue;
            }
            let Some(local) = self.cluster.member(&m.id).cloned() else {
                self.cluster.join(m.id, m.addr.clone());
                self.apply(m);
                continue;
            };
            let newer = m.incarnation > local.incarnation;
            let worse = m.incarnation == local.incarnation && rank(m.state) > rank(local.state);
            if newer || worse {
                self.apply(m);
            }
        }
    }

    /// Set a member to exactly what the winning record says.
    fn apply(&mut self, m: &Member) {
        match m.state {
            // ⚠️ `refute` is the only path that raises an incarnation, and it never invents
            // one: the value comes from the record, which came from the member itself.
            State::Alive => {
                self.cluster.refute(&m.id, m.incarnation);
                self.suspected_at.remove(&m.id);
            }
            State::Suspect => {
                self.cluster.refute(&m.id, m.incarnation);
                self.cluster.suspect(&m.id);
                self.suspected_at.entry(m.id).or_insert(self.tick);
            }
            State::Dead => {
                self.cluster.declare_dead(&m.id);
                self.suspected_at.remove(&m.id);
            }
        }
        self.note_update(&m.id);
    }

    /// Our record of a peer, when that peer needs to see it.
    ///
    /// ⚠️ A node we believe dead has just spoken to us. We must **not** revive it ourselves:
    /// inventing an incarnation for another node makes two nodes disagree about its version
    /// forever, and two nodes that disagree about anything reconcile forever — which
    /// defeats the entire point of a checksum. Instead we hand it our record, and it refutes
    /// with an incarnation only it is entitled to raise.
    fn evidence_for(&self, id: &NodeId) -> Vec<Member> {
        match self.cluster.state(id) {
            Some(State::Suspect | State::Dead) => {
                self.cluster.member(id).cloned().into_iter().collect()
            }
            _ => Vec::new(),
        }
    }

    /// Raise our own incarnation past `theirs`, so our refutation outranks their claim.
    fn refute_self_above(&mut self, theirs: u64) {
        let me = *self.cluster.me();
        let mine = self.cluster.incarnation(&me).unwrap_or(0);
        let next = mine.max(theirs).wrapping_add(1);
        self.cluster.refute(&me, next);
        self.note_update(&me);
    }

    /// Record a peer we had not heard of, at the address it just contacted us from.
    fn learn(&mut self, id: NodeId, addr: &str) {
        if self.cluster.member(&id).is_none() {
            self.cluster.join(id, addr.to_owned());
            self.note_update(&id);
        }
    }

    /// A message arrived from this peer, which is the strongest evidence of liveness there
    /// is — but see `evidence_for`: acting on it locally is not this function's job.
    fn mark_alive(&mut self, id: &NodeId) {
        self.suspected_at.remove(id);
        let _ = id;
    }

    /// Probes that have gone unanswered long enough to doubt their target.
    fn expire_probes(&mut self, out: &mut Vec<(String, Message)>) {
        let due: Vec<(u64, NodeId)> = self
            .pending
            .iter()
            .filter(|(_, (_, sent))| self.tick.saturating_sub(*sent) >= PROBE_TIMEOUT)
            .map(|(seq, (id, _))| (*seq, *id))
            .collect();
        for (seq, id) in due {
            self.pending.remove(&seq);
            if self.cluster.state(&id) != Some(State::Alive) {
                continue;
            }
            // ⚠️ Ask others before concluding. A direct probe failing means *we* could not
            // reach it, which is not the same claim as its being gone, and treating them as
            // the same is how one node's bad link evicts a healthy peer.
            for peer in self.helpers(&id) {
                out.push((
                    peer,
                    Message::PingReq {
                        from: *self.cluster.me(),
                        seq,
                        target: id,
                    },
                ));
            }
            self.cluster.suspect(&id);
            self.suspected_at.entry(id).or_insert(self.tick);
            self.note_update(&id);
        }
    }

    fn bury_suspects(&mut self) {
        let done: Vec<NodeId> = self
            .suspected_at
            .iter()
            .filter(|(id, since)| {
                self.tick.saturating_sub(**since) >= SUSPECT_TIMEOUT
                    && self.cluster.state(id) == Some(State::Suspect)
            })
            .map(|(id, _)| *id)
            .collect();
        for id in done {
            self.cluster.declare_dead(&id);
            self.suspected_at.remove(&id);
            self.note_update(&id);
        }
    }

    fn note_update(&mut self, id: &NodeId) {
        if let Some(m) = self.cluster.member(id) {
            let m = m.clone();
            self.updates.retain(|(u, _)| u.id != m.id);
            self.updates.push((m, 0));
        }
    }

    /// The changes to ride along on the next message, and the count that retires them.
    ///
    /// ⚠️ **Drains.** An earlier version read this list and never emptied it, so every probe
    /// carried the same six member records for the life of the process — turning a 74-byte
    /// round into a 1,200-byte one and quietly restoring the O(N)-ish cost this crate exists
    /// to remove. Measured on a real 100-node fleet: 6,000 bytes/s/node against a predicted
    /// 370, with the member sets in perfect agreement and nothing whatsoever changing.
    ///
    /// Each change is retransmitted a fixed number of times and then dropped. Anything that
    /// misses every one of those is repaired by reconciliation, which is what the checksum is
    /// for — dissemination is allowed to be lossy precisely because repair exists.
    fn piggyback(&mut self) -> Vec<Member> {
        let mut out = Vec::new();
        for (m, sent) in self.updates.iter_mut().rev().take(MAX_PIGGYBACK) {
            out.push(m.clone());
            *sent = sent.saturating_add(1);
        }
        self.updates.retain(|(_, sent)| *sent < RETRANSMITS);
        out
    }

    fn addr_of(&self, id: &NodeId) -> Option<String> {
        self.cluster.member(id).map(|m| m.addr.clone())
    }

    /// A peer to probe this period.
    ///
    /// ⚠️ **The dead are probed too**, rarely, and a node with no live peers probes nothing
    /// else. A death is a conclusion drawn from silence, and silence is symmetric: a node
    /// that believes the entire fleet is dead is overwhelmingly likely to be the one that was
    /// partitioned. Without this a healed partition never heals — measured, the isolated half
    /// buried everyone, then had no live peer left to probe and no way back, and the worst
    /// node sat at 1 of 20 forever.
    ///
    /// It is the same failure M4b found in `chitchat`, which seeds once: a node whose peers
    /// were all unreachable at one instant stays unreachable by construction.
    fn pick_peer(&self, seed: u64) -> Option<Member> {
        let live: Vec<&Member> = self
            .cluster
            .alive()
            .into_iter()
            .filter(|m| m.id != *self.cluster.me())
            .collect();
        let buried: Vec<&Member> = self
            .cluster
            .members()
            .filter(|m| m.state == State::Dead && m.id != *self.cluster.me())
            .collect();

        // Rarely when there is anyone alive to talk to; always when there is not.
        let revisit = live.is_empty() || self.tick.is_multiple_of(REVISIT_DEAD_EVERY);
        let candidates = if revisit && !buried.is_empty() {
            buried
        } else {
            live
        };
        if candidates.is_empty() {
            return None;
        }
        let mut h = seed ^ self.tick.wrapping_mul(0x9E37_79B9_7F4A_7C15);
        h ^= h >> 33;
        h = h.wrapping_mul(0xff51_afd7_ed55_8ccd);
        h ^= h >> 33;
        candidates
            .get((h % candidates.len() as u64) as usize)
            .map(|m| (*m).clone())
    }

    fn helpers(&self, avoid: &NodeId) -> Vec<String> {
        self.cluster
            .alive()
            .into_iter()
            .filter(|m| m.id != *self.cluster.me() && m.id != *avoid)
            .take(INDIRECT_PROBES)
            .map(|m| m.addr.clone())
            .collect()
    }
}
