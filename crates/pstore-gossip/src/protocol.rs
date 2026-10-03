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

/// The floor on how many periods a suspect may remain unrefuted before it is declared dead.
///
/// ⚠️ Generous relative to `PROBE_TIMEOUT`, because this is the window in which a node that
/// was merely busy gets to answer. Shortening it is how a loaded fleet evicts its own healthy
/// members — OQ-12, in the form the timeouts control.
const SUSPECT_TIMEOUT_MIN: u64 = 6;

/// How the suspicion window grows with the fleet.
///
/// ⚠️ **It has to grow.** A suspicion is only refutable if it reaches the suspected node, and
/// dissemination takes O(log N) periods to cross a fleet of N. A constant window is therefore
/// a window that is too short at scale, and the node gets buried before the news that it is
/// suspected ever arrives. Measured at 100 nodes under 10% loss, with a constant 6: healthy
/// nodes were declared dead and the worst view sat at 98 of 100 indefinitely.
fn suspect_timeout(members: usize) -> u64 {
    let log2 = usize::BITS - members.max(1).leading_zeros();
    SUSPECT_TIMEOUT_MIN.max(3 * u64::from(log2))
}

/// The hash that turns a period's seed and tick into a peer index.
///
/// ⚠️ **Pinned against an independent model** (M8f). Seven mutants in this arithmetic survived
/// the only test of it, which asked that 60 probes reach at least 5 of 11 peers -- a bound any
/// hash that is not degenerate meets. A function so the values can be asserted exactly.
fn mix(seed: u64, tick: u64) -> u64 {
    let mut h = seed ^ tick.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    h ^= h >> 33;
    h = h.wrapping_mul(0xff51_afd7_ed55_8ccd);
    h ^= h >> 33;
    h
}

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

/// The largest fleet that still reconciles with a whole `Sync` (M34): twice the buckets. Above
/// it, a mismatch sends a `Digest`, and only the differing buckets come back. ⚠️ Not lower:
/// at 20 members a partial reconciliation agreed in 297 of 400 rounds at 2% loss, against a
/// whole `Sync`'s 395, for no bytes saved.
const RECONCILE_WHOLE_UP_TO: usize = 2 * crate::cluster::BUCKETS;

/// The largest fleet whose mismatch still sends a `Digest` (M43): seven members a bucket.
/// Above it a mismatch sends a `TaggedDigest`. Measured at 10% loss: at 100 members its 256
/// tag bytes cost more than they save (757 B per node per round against 734), and at 128 they
/// save (838 against 926). ⚠️ One schedule, one loss rate, interpolated: a cost knob.
const TAG_ABOVE: usize = 7 * crate::cluster::BUCKETS;

/// A differing bucket holding at most this many of the answerer's members is answered whole,
/// whatever its tags say (M43): too few to be worth filtering.
const LEAF_ABOVE: usize = 4;

/// How many changes ride along on a probe.
///
/// ⚠️ Bounded, so a fleet in churn cannot turn a probe back into the O(N) message this crate
/// exists to remove. What does not fit propagates on the next round, one hop later.
const MAX_PIGGYBACK: usize = 6;

/// What a pending probe is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Probe {
    /// Our own direct probe: its timeout asks peers to probe for us.
    Direct,
    /// Our own probe, already asked of peers: its timeout is evidence.
    Asked,
    /// A probe made on a peer's behalf (M38). Its timeout is one path failing, which is the
    /// asker's to weigh against the others it asked: never evidence here. A helper that
    /// suspected on it originated 2,953 of 3,063 suspicions at 100 members and 10% loss.
    Relayed,
}

/// One node's protocol state.
#[derive(Debug)]
pub struct Protocol {
    cluster: Cluster,
    seq: u64,
    /// Probes awaiting an ack: sequence number to (target, tick sent, what it is for).
    pending: BTreeMap<u64, (NodeId, u64, Probe)>,
    /// Probes we are making on someone else's behalf: our sequence to (who asked, their
    /// sequence). ⚠️ The ack has to be **relayed back**, or the requester learns nothing from
    /// the indirect round and suspects anyway — which makes the whole indirect step
    /// decorative.
    relaying: BTreeMap<u64, (String, u64)>,
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
            relaying: BTreeMap::new(),
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
            self.pending
                .insert(self.seq, (target.id, self.tick, Probe::Direct));
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
                        out.push((addr, self.reconcile()));
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
                // If this ack answers a probe someone else asked for, pass it on — that is
                // the entire value of having asked.
                if let Some((asker, their_seq)) = self.relaying.remove(seq) {
                    out.push((
                        asker,
                        Message::Ack {
                            from: *from,
                            seq: their_seq,
                            checksum: *checksum,
                            updates: Vec::new(),
                        },
                    ));
                }
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
                        out.push((addr, self.reconcile()));
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
                    self.pending
                        .insert(self.seq, (*target, self.tick, Probe::Relayed));
                    if let Some(asker) = self.addr_of(from) {
                        self.relaying.insert(self.seq, (asker, *seq));
                    }
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
                out
            }
            Message::Digest { from, buckets } => {
                // Answer with what differs: the peer sends its own `Digest`, and learns ours.
                self.mark_alive(from);
                let mine = self.cluster.buckets();
                let differ: Vec<usize> = (0..mine.len())
                    .filter(|i| buckets.get(*i) != mine.get(*i))
                    .collect();
                let mut out = Vec::new();
                if !differ.is_empty()
                    && let Some(addr) = self.addr_of(from)
                {
                    let members = self
                        .cluster
                        .members()
                        .filter(|m| differ.contains(&crate::cluster::bucket_of(&m.id)))
                        .cloned()
                        .collect();
                    out.push((
                        addr,
                        Message::Part {
                            from: *self.cluster.me(),
                            members,
                        },
                    ));
                }
                out
            }
            Message::TaggedDigest {
                from,
                buckets,
                tags,
            } => {
                // M43: answered whatever this view's size -- the answer depends on the
                // message, never on the receiver (spec review).
                self.mark_alive(from);
                let send = self.leaves_to_send(buckets, tags);
                let mut out = Vec::new();
                if send.iter().any(|m| *m != 0)
                    && let Some(addr) = self.addr_of(from)
                {
                    let members = self
                        .cluster
                        .members()
                        .filter(|m| {
                            let leaf = crate::cluster::leaf_of(&m.id);
                            let per = crate::cluster::LEAVES_PER_BUCKET;
                            send.get(leaf / per)
                                .is_some_and(|mask| mask & (1 << (leaf % per)) != 0)
                        })
                        .cloned()
                        .collect();
                    out.push((
                        addr,
                        Message::Part {
                            from: *self.cluster.me(),
                            members,
                        },
                    ));
                }
                out
            }
            Message::Part { from, members } => {
                self.absorb(members);
                self.mark_alive(from);
                Vec::new()
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

    /// What a checksum mismatch sends (M34): a `Digest` past [`RECONCILE_WHOLE_UP_TO`]
    /// members, else a whole `Sync`, which is cheap at that size and converges in fewer
    /// exchanges. Past [`TAG_ABOVE`], a `TaggedDigest` (M43).
    fn reconcile(&self) -> Message {
        if self.cluster.len() > TAG_ABOVE {
            Message::TaggedDigest {
                from: *self.cluster.me(),
                buckets: self.cluster.buckets().to_vec(),
                tags: self.cluster.leaf_tags().to_vec(),
            }
        } else if self.cluster.len() > RECONCILE_WHOLE_UP_TO {
            Message::Digest {
                from: *self.cluster.me(),
                buckets: self.cluster.buckets().to_vec(),
            }
        } else {
            self.sync()
        }
    }

    /// Which leaves a `TaggedDigest` is answered with (M43): one 16-bit mask per bucket, so
    /// the answer is one pass over the members against 256 bits, never a list searched per
    /// member (spec review). For each bucket whose sum differs:
    /// - every leaf, when this view holds [`LEAF_ABOVE`] or fewer of its members;
    /// - every leaf, when no tag in it differs: a collision. ⚠️ So a difference the 64-bit
    ///   sums show is never left unanswered;
    /// - otherwise, the leaves whose tags differ.
    fn leaves_to_send(&self, buckets: &[u64], tags: &[u8]) -> [u16; crate::cluster::BUCKETS] {
        let per = crate::cluster::LEAVES_PER_BUCKET;
        let mine = self.cluster.buckets();
        let my_tags = self.cluster.leaf_tags();
        let mut count = [0usize; crate::cluster::BUCKETS];
        for m in self.cluster.members() {
            if let Some(c) = count.get_mut(crate::cluster::bucket_of(&m.id)) {
                *c += 1;
            }
        }
        let mut send = [0u16; crate::cluster::BUCKETS];
        for (b, mask) in send.iter_mut().enumerate() {
            if buckets.get(b) == mine.get(b) {
                continue;
            }
            let differing = (0..per)
                .filter(|j| tags.get(b * per + j) != my_tags.get(b * per + j))
                .fold(0u16, |acc, j| acc | (1 << j));
            *mask = if count.get(b).copied().unwrap_or(0) <= LEAF_ABOVE || differing == 0 {
                u16::MAX
            } else {
                differing
            };
        }
        send
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
                //
                // ⚠️ **Only at or above our own incarnation** (M38). A lower claim is already
                // outranked by our record, and raising again made one suspicion cost five
                // refutations, each changing every checksum. It is answered with that record
                // instead: ignored outright, a claimant whose evidence ping we ack sends it
                // again, forever (spec review, M38). Each one re-arms our record's
                // retransmits, so it holds one of the piggyback's slots while stale claims keep
                // arriving: bounded, and over once the claimants hold it.
                if m.state != State::Alive {
                    let mine = self.cluster.incarnation(&m.id).unwrap_or(0);
                    if m.incarnation >= mine {
                        self.refute_self_above(m.incarnation);
                    } else {
                        self.note_update(&m.id);
                    }
                }
                continue;
            }
            let Some(local) = self.cluster.member(&m.id).cloned() else {
                self.cluster.join(m.id, m.addr.clone(), m.zone.clone());
                self.apply(m);
                continue;
            };
            let newer = m.incarnation > local.incarnation;
            let worse = m.incarnation == local.incarnation && rank(m.state) > rank(local.state);
            if newer || worse {
                self.apply(m);
            } else if m.incarnation == local.incarnation && self.cluster.fill_zone(&m.id, &m.zone) {
                // ⚠️ M24: the peer's own record, at the incarnation already held, is how a peer
                // learned from a probe gets its zone -- and only its zone. It is news, so it
                // spreads, and every view converges on one checksum.
                self.note_update(&m.id);
            }
        }
    }

    /// Set a member to what the winning record says.
    ///
    /// ⚠️ Takes the record's address and state, and its zone **unless** that would lose one
    /// (M24). An empty zone means "unknown", never "none", so it never clears a known zone;
    /// and a member declares its zone once per incarnation, so an equal one never moves it. A
    /// record that wins and carries a zone over an empty one fills it, as any winner does.
    fn apply(&mut self, m: &Member) {
        let mut m = m.clone();
        if let Some(local) = self.cluster.member(&m.id)
            && !local.zone.is_empty()
            && (m.zone.is_empty() || m.incarnation == local.incarnation)
        {
            m.zone.clone_from(&local.zone);
        }
        let m = &m;
        self.cluster.upsert(m.clone());
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
            // ⚠️ Zone unknown: a probe does not carry one. Until an authoritative record
            // arrives this peer belongs to no cell, which keeps it out of every ring rather
            // than putting it in the wrong one.
            self.cluster.join(id, addr.to_owned(), String::new());
            self.note_update(&id);
        }
    }

    /// A message arrived from this peer, which is the strongest evidence of liveness there
    /// is — but see `evidence_for`: acting on it locally is not this function's job.
    fn mark_alive(&mut self, id: &NodeId) {
        self.suspected_at.remove(id);
        let _ = id;
    }

    /// Probes that have gone unanswered long enough to act on.
    ///
    /// ⚠️ **A direct timeout does not suspect.** It means *we* could not reach the peer, which
    /// is not the same claim as its being gone — and on a lossy network the two are confused
    /// constantly. Measured at 100 nodes under 10% loss with everything healthy: the worst
    /// view fell to 92 of 100 and the fleet flapped indefinitely, because every lost datagram
    /// became a suspicion. So a direct timeout asks other peers, and only a second timeout,
    /// with their answers also missing, is evidence of absence.
    fn expire_probes(&mut self, out: &mut Vec<(String, Message)>) {
        let due: Vec<(u64, NodeId, Probe)> = self
            .pending
            .iter()
            .filter(|(_, (_, sent, _))| self.tick.saturating_sub(*sent) >= PROBE_TIMEOUT)
            .map(|(seq, (id, _, kind))| (*seq, *id, *kind))
            .collect();
        for (seq, id, kind) in due {
            self.pending.remove(&seq);
            if kind == Probe::Relayed {
                // ⚠️ And the relay entry with it (M38): cleared only by an ack before, so every
                // relayed probe never answered stayed for the life of the node.
                self.relaying.remove(&seq);
                continue;
            }
            if self.cluster.state(&id) != Some(State::Alive) {
                continue;
            }
            if kind == Probe::Asked {
                // The indirect round produced nothing either. Now it is evidence.
                self.cluster.suspect(&id);
                self.suspected_at.entry(id).or_insert(self.tick);
                self.note_update(&id);
                continue;
            }
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
            // Re-arm: the same probe, now waiting on the indirect answers.
            self.pending.insert(seq, (id, self.tick, Probe::Asked));
        }
    }

    fn bury_suspects(&mut self) {
        let window = suspect_timeout(self.cluster.len());
        let done: Vec<NodeId> = self
            .suspected_at
            .iter()
            .filter(|(id, since)| {
                self.tick.saturating_sub(**since) >= window
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
        let h = mix(seed, self.tick);
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

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]
mod tests {
    //! Exact contracts for the private machinery, where the simulation tests assert bounds.
    //!
    //! ⚠️ M8f: the nightly mutation sweep found 15 survivors here. The simulation tests say
    //! "carried at most 24 times" and "reached at least 5 of 11 peers", and a mutant that
    //! stays inside a bound passes it. These pin the contract itself.
    use super::*;

    fn nid(n: u8) -> NodeId {
        let mut b = [0u8; 16];
        b[0] = n;
        b
    }

    /// Node 0 is this node; `live` are joined, `dead` are joined and declared dead.
    fn protocol(live: &[u8], dead: &[u8]) -> Protocol {
        let mut c = Cluster::new(nid(0), "n0".to_owned(), "az-a".to_owned());
        for &n in live.iter().chain(dead) {
            c.join(nid(n), format!("n{n}"), "az-a".to_owned());
        }
        for &n in dead {
            c.declare_dead(&nid(n));
        }
        Protocol::new(c)
    }

    /// `n`'s record as a peer would send it (M24).
    fn record(n: u8, zone: &str, incarnation: u64, state: State) -> Member {
        Member {
            id: nid(n),
            addr: format!("n{n}"),
            zone: zone.to_owned(),
            incarnation,
            state,
        }
    }

    /// Node 0, knowing node 1 at its address only, with an empty zone, as a probe leaves it.
    fn probed() -> Protocol {
        let mut c = Cluster::new(nid(0), "n0".to_owned(), "az-a".to_owned());
        c.join(nid(1), "n1".to_owned(), String::new());
        Protocol::new(c)
    }

    fn zone_of(p: &Protocol, n: u8) -> String {
        p.cluster
            .member(&nid(n))
            .map(|m| m.zone.clone())
            .unwrap_or_default()
    }

    /// `me`'s view of members 0 to 120, all alive at incarnation 0.
    fn tagged_view(me: u8) -> Protocol {
        let mut c = Cluster::new(nid(me), format!("n{me}"), "az-a".to_owned());
        for n in 0..=120 {
            c.join(nid(n), format!("n{n}"), "az-a".to_owned());
        }
        Protocol::new(c)
    }

    /// `p`'s `TaggedDigest`, with every tag `copy` says replaced by `from`'s: a collision.
    fn colliding(p: &Protocol, from: &Protocol, copy: impl Fn(usize) -> bool) -> Message {
        let Message::TaggedDigest {
            from: sender,
            buckets,
            mut tags,
        } = p.reconcile()
        else {
            panic!("a view of 121 did not send a TaggedDigest");
        };
        let theirs = from.cluster.leaf_tags();
        for (l, t) in tags.iter_mut().enumerate() {
            if copy(l) {
                *t = theirs[l];
            }
        }
        Message::TaggedDigest {
            from: sender,
            buckets,
            tags,
        }
    }

    /// The members of the one `Part` `out` holds, by id.
    fn part_of(out: &[(String, Message)]) -> Vec<NodeId> {
        match out {
            [(to, Message::Part { members, .. })] if to == "n1" => {
                members.iter().map(|m| m.id).collect()
            }
            _ => panic!("expected one Part to n1, got {out:?}"),
        }
    }

    #[test]
    fn a_tag_collision_sends_the_whole_bucket() {
        // M43: a bucket whose sums differ but whose tags all agree is a collision, and is
        // answered whole -- a difference the 64-bit sums show is never left unanswered.
        // A bucket of more than `LEAF_ABOVE` members with two members in different leaves,
        // neither of them node 0 or 1.
        let (b, m1, m2) = (0..crate::cluster::BUCKETS)
            .find_map(|b| {
                let ids: Vec<NodeId> = (0..=120)
                    .map(nid)
                    .filter(|i| crate::cluster::bucket_of(i) == b)
                    .collect();
                let m1 = *ids.iter().find(|i| i[0] > 1)?;
                let m2 = *ids.iter().find(|i| {
                    i[0] > 1 && crate::cluster::leaf_of(i) != crate::cluster::leaf_of(&m1)
                })?;
                (ids.len() > LEAF_ABOVE).then_some((b, m1, m2))
            })
            .expect("no bucket of more than 4 with two leaves");
        let (l1, l2) = (crate::cluster::leaf_of(&m1), crate::cluster::leaf_of(&m2));
        let bucket = |p: &Protocol| -> Vec<NodeId> {
            p.cluster
                .members()
                .filter(|m| crate::cluster::bucket_of(&m.id) == b)
                .map(|m| m.id)
                .collect()
        };

        // A full collision: every tag equal, the bucket's sums not.
        let mut a = tagged_view(0);
        let p = tagged_view(1);
        a.cluster.refute(&m1, 1);
        let digest = colliding(&p, &a, |_| true);
        assert_eq!(part_of(&a.receive("n1", &digest)), bucket(&a));

        // A partial one: the answerer holds the newer copy of both leaves; l1's tags differ
        // and l2's collide. The first `Part` carries l1 alone.
        let mut a = tagged_view(0);
        let mut p = tagged_view(1);
        a.cluster.refute(&m1, 1);
        a.cluster.refute(&m2, 1);
        let digest = colliding(&p, &a, |l| l == l2);
        let mut out = a.receive("n1", &digest);
        let first = part_of(&out);
        assert!(
            first.contains(&m1),
            "the first Part lacks the leaf that differs"
        );
        assert!(
            first.iter().all(|i| crate::cluster::leaf_of(i) == l1),
            "the first Part carried more than leaf {l1}"
        );
        let Some((_, part)) = out.pop() else {
            unreachable!()
        };
        p.receive("n0", &part);
        assert_eq!(p.cluster.leaves()[l1], a.cluster.leaves()[l1]);
        assert_ne!(p.cluster.buckets()[b], a.cluster.buckets()[b]);
        // l1 now agrees and l2 still collides: no tag in the bucket differs, so it goes whole.
        let digest = colliding(&p, &a, |l| l == l2);
        let mut out = a.receive("n1", &digest);
        assert_eq!(part_of(&out), bucket(&a));
        let Some((_, part)) = out.pop() else {
            unreachable!()
        };
        p.receive("n0", &part);
        assert_eq!(p.cluster.checksum(), a.cluster.checksum());
    }

    #[test]
    fn a_bucket_of_four_is_sent_whole_and_of_five_by_leaf() {
        // M43 code review: the answerer's own count decides. A differing bucket of at most
        // `LEAF_ABOVE` of its members goes whole; past that, only the leaves whose tags differ.
        let b = (0..crate::cluster::BUCKETS)
            .find(|b| *b != crate::cluster::bucket_of(&nid(0)))
            .expect("a bucket without node 0");
        let ids: Vec<NodeId> = (1..=250)
            .map(nid)
            .filter(|i| crate::cluster::bucket_of(i) == b)
            .collect();
        assert!(ids.len() > LEAF_ABOVE, "too few ids in bucket {b}");
        let leaf = crate::cluster::leaf_of(&ids[0]);
        for size in [LEAF_ABOVE, LEAF_ABOVE + 1] {
            let mut c = Cluster::new(nid(0), "n0".to_owned(), "az-a".to_owned());
            for i in &ids[..size] {
                c.join(*i, format!("n{}", i[0]), "az-a".to_owned());
            }
            let a = Protocol::new(c);
            let mut buckets = a.cluster.buckets().to_vec();
            buckets[b] ^= 1;
            let mut tags = a.cluster.leaf_tags().to_vec();
            tags[leaf] ^= 1;
            let mask = a.leaves_to_send(&buckets, &tags);
            let want = if size <= LEAF_ABOVE {
                u16::MAX
            } else {
                1 << (leaf % crate::cluster::LEAVES_PER_BUCKET)
            };
            assert_eq!(mask[b], want, "a bucket of {size}");
            assert!(
                mask.iter().enumerate().all(|(j, m)| j == b || *m == 0),
                "a bucket whose sums agree was sent: {mask:?}"
            );
        }
    }

    fn honest(p: &Protocol) {
        assert_eq!(p.cluster.checksum(), p.cluster.checksum_from_scratch());
    }

    #[test]
    fn a_relay_that_times_out_suspects_nobody() {
        // M38: a helper's one relayed path failing is not evidence. At 100 members and 10%
        // loss it originated 2,953 of 3,063 suspicions, and every one changed every checksum.
        let mut p = protocol(&[1, 2], &[]);
        let out = p.receive(
            "n2",
            &Message::PingReq {
                from: nid(2),
                seq: 9,
                target: nid(1),
            },
        );
        assert_eq!(out.len(), 1, "the relayed ping: {out:?}");
        assert_eq!(p.relaying.len(), 1);
        for _ in 0..=2 * PROBE_TIMEOUT {
            p.tick += 1;
            let mut out = Vec::new();
            p.expire_probes(&mut out);
            assert!(out.is_empty(), "a relay ran an indirect round: {out:?}");
        }
        assert_eq!(p.cluster.state(&nid(1)), Some(State::Alive));
        assert!(p.relaying.is_empty(), "the relay entry leaked");
        assert!(p.pending.is_empty());
    }

    #[test]
    fn a_stale_claim_is_not_refuted() {
        // M38: a claim below our incarnation is already outranked. Raising it again made one
        // suspicion cost five refutations, each changing every checksum.
        let mut p = protocol(&[1], &[]);
        p.cluster.refute(&nid(0), 3);
        p.updates.clear();
        p.absorb(&[record(0, "az-a", 2, State::Suspect)]);
        p.absorb(&[record(0, "az-a", 2, State::Dead)]);
        assert_eq!(p.cluster.incarnation(&nid(0)), Some(3));
        // ⚠️ But answered, with the record that outranks it (spec review): ignored outright,
        // a claimant whose evidence ping is acked sends it again, forever.
        let ours = p.piggyback();
        assert!(
            ours.iter()
                .any(|m| m.id == nid(0) && m.incarnation == 3 && m.state == State::Alive),
            "{ours:?}"
        );
        // A claim at our own incarnation is still refuted.
        p.absorb(&[record(0, "az-a", 3, State::Suspect)]);
        assert_eq!(p.cluster.incarnation(&nid(0)), Some(4));
    }

    #[test]
    fn an_empty_zone_never_clears_a_known_one() {
        let mut p = protocol(&[1], &[]);
        // A worse state at an equal incarnation wins, and keeps the zone it had.
        p.absorb(&[record(1, "", 0, State::Suspect)]);
        assert_eq!(p.cluster.state(&nid(1)), Some(State::Suspect));
        assert_eq!(zone_of(&p, 1), "az-a");
        honest(&p);
        // So does a higher incarnation.
        p.absorb(&[record(1, "", 1, State::Alive)]);
        assert_eq!(p.cluster.incarnation(&nid(1)), Some(1));
        assert_eq!(zone_of(&p, 1), "az-a");
        honest(&p);
    }

    #[test]
    fn an_equal_incarnation_never_moves_a_zone() {
        let mut p = protocol(&[1], &[]);
        p.absorb(&[record(1, "az-z", 0, State::Alive)]);
        assert_eq!(zone_of(&p, 1), "az-a");
        p.absorb(&[record(1, "az-z", 0, State::Suspect)]);
        assert_eq!(zone_of(&p, 1), "az-a", "a winning record moved the zone");
        honest(&p);
        p.absorb(&[record(1, "az-z", 1, State::Alive)]);
        assert_eq!(zone_of(&p, 1), "az-z");
        honest(&p);
    }

    #[test]
    fn a_fill_never_revives() {
        // Suspect: the zone is filled, and the suspicion still runs to its end.
        let mut p = probed();
        p.cluster.suspect(&nid(1));
        p.suspected_at.insert(nid(1), p.tick);
        p.absorb(&[record(1, "az-b", 0, State::Alive)]);
        assert_eq!(zone_of(&p, 1), "az-b");
        assert_eq!(p.cluster.state(&nid(1)), Some(State::Suspect));
        assert_eq!(p.cluster.incarnation(&nid(1)), Some(0));
        assert_eq!(p.suspected_at.get(&nid(1)), Some(&0));
        honest(&p);
        p.tick = suspect_timeout(p.cluster.len()) + 1;
        p.bury_suspects();
        assert_eq!(p.cluster.state(&nid(1)), Some(State::Dead));
        // Dead: filled, and still dead.
        let mut p = probed();
        p.cluster.declare_dead(&nid(1));
        p.absorb(&[record(1, "az-b", 0, State::Alive)]);
        assert_eq!(zone_of(&p, 1), "az-b");
        assert_eq!(p.cluster.state(&nid(1)), Some(State::Dead));
        honest(&p);
    }

    #[test]
    fn a_winning_record_fills_an_empty_zone() {
        // Precedence wins, at an equal incarnation, over a member with no zone: the record is
        // applied, and its zone with it (code review round 1, m-2).
        let mut p = probed();
        p.absorb(&[record(1, "az-b", 0, State::Suspect)]);
        assert_eq!(p.cluster.state(&nid(1)), Some(State::Suspect));
        assert_eq!(zone_of(&p, 1), "az-b");
        honest(&p);
    }

    #[test]
    fn a_fill_is_news_and_keeps_the_checksum() {
        let mut p = probed();
        while !p.piggyback().is_empty() {}
        p.absorb(&[record(1, "az-b", 0, State::Alive)]);
        honest(&p);
        let carried = p.piggyback();
        assert!(
            carried.iter().any(|m| m.id == nid(1) && m.zone == "az-b"),
            "the fill was not news: {carried:?}"
        );
    }

    #[test]
    fn the_suspect_timeout_grows_with_the_log_of_the_fleet() {
        // The floor hides the scaling at small fleets, which is where every simulation runs.
        assert_eq!(suspect_timeout(1), SUSPECT_TIMEOUT_MIN);
        assert_eq!(suspect_timeout(1 << 20), 63, "3 x a 21-bit fleet size");
    }

    #[test]
    fn a_change_rides_along_exactly_retransmits_times() {
        let mut p = protocol(&[1], &[]);
        p.note_update(&nid(1));
        let carried: Vec<usize> = (0..10).map(|_| p.piggyback().len()).collect();
        let expected: Vec<usize> = (0..10)
            .map(|i| usize::from(i < usize::from(RETRANSMITS)))
            .collect();
        assert_eq!(
            carried, expected,
            "a change must ride exactly {RETRANSMITS} times"
        );
    }

    #[test]
    fn noting_a_member_again_replaces_only_its_own_entry() {
        let mut p = protocol(&[1, 2], &[]);
        p.note_update(&nid(1));
        p.note_update(&nid(2));
        p.note_update(&nid(1));
        let pending: Vec<NodeId> = p.updates.iter().map(|(m, _)| m.id).collect();
        assert_eq!(
            pending,
            vec![nid(2), nid(1)],
            "B must survive A being noted again"
        );
    }

    /// Every peer picked over 64 seeds at `tick`, set directly: driving `tick()` would
    /// suspect and then bury the fixture's peers, since nothing here answers a probe.
    fn picks(p: &mut Protocol, tick: u64) -> Vec<NodeId> {
        p.tick = tick;
        (0..64u64)
            .filter_map(|s| p.pick_peer(s).map(|m| m.id))
            .collect()
    }

    #[test]
    fn the_dead_are_revisited_on_revisit_ticks_and_whenever_no_one_is_alive() {
        let (live, dead) = ([1u8, 2, 3], [4u8, 5]);
        let is = |set: &[u8], id: &NodeId| set.iter().any(|&n| nid(n) == *id);

        let mut p = protocol(&live, &dead);
        for t in [1, 2, 3, 4, 6] {
            let got = picks(&mut p, t);
            assert_eq!(got.len(), 64, "tick {t} picked nobody");
            assert!(
                got.iter().all(|id| is(&live, id)),
                "tick {t} picked outside the live set"
            );
        }
        for t in [5, 10] {
            let got = picks(&mut p, t);
            assert_eq!(got.len(), 64, "revisit tick {t} picked nobody");
            assert!(
                got.iter().all(|id| is(&dead, id)),
                "revisit tick {t} picked a live peer"
            );
        }

        let mut isolated = protocol(&[], &dead);
        for t in [1, 2, 3, 4, 5, 6, 10] {
            let got = picks(&mut isolated, t);
            assert_eq!(got.len(), 64, "an isolated node picked nobody at tick {t}");
            assert!(got.iter().all(|id| is(&dead, id)));
        }

        let mut alone = protocol(&[], &[]);
        assert!(
            picks(&mut alone, 1).is_empty(),
            "a node with no peers picked one"
        );
    }

    /// ⚠️ **A suspect that speaks is not buried on the old timer.** Hearing from a peer clears
    /// its suspicion clock without changing its state -- refuting is the peer's job, prompted
    /// by the evidence the ack carries -- so a peer that answered must not be declared dead
    /// when a window that started before it answered runs out. Found by M8f's sweep: with
    /// `mark_alive` emptied, nothing noticed.
    #[test]
    fn a_suspect_that_speaks_is_not_buried_on_the_old_timer() {
        let mut p = protocol(&[1], &[]);
        p.cluster.suspect(&nid(1));
        p.suspected_at.insert(nid(1), 0);
        let ping = Message::Ping {
            from: nid(1),
            seq: 0,
            checksum: p.cluster.checksum(),
            updates: Vec::new(),
        };
        p.receive("n1", &ping);
        p.tick = 1_000;
        p.bury_suspects();
        assert_ne!(
            p.cluster.state(&nid(1)),
            Some(State::Dead),
            "a suspect that answered was buried on the timer it answered"
        );
    }

    #[test]
    fn indirect_probes_go_to_live_peers_other_than_the_target() {
        let p = protocol(&[1, 2, 3, 4, 5], &[6]);
        let helpers = p.helpers(&nid(2));
        assert_eq!(helpers.len(), INDIRECT_PROBES, "{helpers:?}");
        for h in &helpers {
            assert!(
                ["n1", "n3", "n4", "n5"].contains(&h.as_str()),
                "{h} is not a live peer other than this node and the target"
            );
        }
        let mut distinct = helpers.clone();
        distinct.sort();
        distinct.dedup();
        assert_eq!(
            distinct.len(),
            helpers.len(),
            "a helper was asked twice: {helpers:?}"
        );
    }

    /// ⚠️ Values from an independent model of the same arithmetic, not from this code:
    ///
    /// ```text
    /// M = (1 << 64) - 1
    /// def mix(s, t):
    ///     h = (s ^ ((t * 0x9E3779B97F4A7C15) & M)) & M
    ///     h ^= h >> 33; h = (h * 0xff51afd7ed558ccd) & M; h ^= h >> 33
    ///     return h
    /// ```
    ///
    /// No zero seed or tick: at `(0, 0)` every mutant of the mixing survives. A deliberate
    /// change to the selection hash must update these -- that is the point of pinning them.
    #[test]
    fn peer_selection_mixing_matches_an_independent_model() {
        assert_eq!(mix(1, 1), 0x93f0_1a4e_d8b4_cd0f);
        assert_eq!(mix(42, 5), 0x7742_60bc_98c9_f5c2);
        assert_eq!(mix(0xDEAD_BEEF, 3), 0xbbd4_8e3b_6581_0b13);
    }
}
