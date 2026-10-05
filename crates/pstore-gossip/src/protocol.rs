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

/// Members a view holds per 256 leaves before a `TaggedDigest` doubles them (M52): about 1.56
/// a leaf. Measured at 10% loss on the `Sim`: 512 leaves at 800 members cost ~3,161 B per node
/// per round against 3,445, and 1,024 at 1,600 ~6,122 against 10,159; 1,024 at 400 cost more
/// (2,818 against 1,672), since tags dominate a small fleet.
const TAG_SCALE: usize = 400;

/// Leaves per bucket for a view of `len` members (M52): 16 × 2^k for the smallest k with
/// `len ≤ TAG_SCALE × 2^k`, at most 2^7.
fn leaves_per_bucket(len: usize) -> usize {
    let mut k = 0;
    while k < crate::wire::MAX_LEAF_SHIFT && len > TAG_SCALE << k {
        k += 1;
    }
    crate::cluster::LEAVES_PER_BUCKET << k
}

/// How many `TaggedDigest`s a peer may leave unanswered before it is sent M34's `Digest`
/// instead (M48). A build from before M43 decodes tag 7 to nothing; a current one always
/// answers. Three silences in a row are ~0.7% at 10% loss (the digest or its answer lost,
/// ~19% each), and a false mark only costs M34's price for that peer.
const UNANSWERED: u8 = 3;

/// Ticks a peer stays stepped down to 256 tags before finer ones are tried again (M52).
/// ⚠️ The expiry is what keeps two current nodes from holding each other at 256 for good when
/// both were stepped down by loss (code review): one tries finer, the other answers it and,
/// having received finer tags, drops its own entry. A build from before M52 costs three
/// unanswered digests per expiry.
const COARSE_TICKS: u64 = 64;

/// The bytes of digest answers a node sends per tick past the tick's first answer (M49):
/// four datagrams. Honest traffic does not reach it in the `Sim` at 200, 800 or 1,600
/// members under heavy loss (`budget_drops` asserted 0). ⚠️ At 1,600 only since M52 spread
/// the indirect probes: before, the three relays every node asked took 98,601 drops. A flood
/// of digests draws at most this plus one answer a tick, where before M49 it drew a full
/// answer per datagram.
const ANSWER_BUDGET: usize = 4 * crate::wire::MAX_DATAGRAM;

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
    /// `TaggedDigest`s sent to each peer since its last `Part` (M48).
    unanswered: BTreeMap<NodeId, u8>,
    /// Peers sent M34's `Digest` because they left [`UNANSWERED`] tags unanswered: until they
    /// send a `TaggedDigest` themselves, which only a build that reads tags does.
    untagged: std::collections::BTreeSet<NodeId>,
    /// Peers whose digest was answered (or dropped) this tick (M49). Cleared by `tick`.
    answered: std::collections::BTreeSet<NodeId>,
    /// Bytes of digest answers sent this tick (M49). Cleared by `tick`.
    spent: usize,
    /// Digests dropped because the tick's [`ANSWER_BUDGET`] was spent (M49).
    budget_drops: u64,
    /// Digests dropped because their peer was already answered this tick (M49).
    repeat_drops: u64,
    /// Peers reconciled with this tick (M50), on either path. Cleared by `tick`.
    reconciled: std::collections::BTreeSet<NodeId>,
    /// Times a peer was newly marked untagged (M50): how M48's fallback is counted.
    untagged_marks: u64,
    /// Members stepped down to 256 tags (M52), with the tick they were: they left
    /// [`UNANSWERED`] finer digests unanswered, as a build from before M52 does. Until they
    /// send a finer digest, or [`COARSE_TICKS`] pass and finer tags are tried again.
    coarse: BTreeMap<NodeId, u64>,
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
            unanswered: BTreeMap::new(),
            untagged: std::collections::BTreeSet::new(),
            answered: std::collections::BTreeSet::new(),
            spent: 0,
            budget_drops: 0,
            repeat_drops: 0,
            reconciled: std::collections::BTreeSet::new(),
            untagged_marks: 0,
            coarse: BTreeMap::new(),
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
        // M49: a new tick's answer budget, and every peer may be answered once more.
        self.answered.clear();
        // M50: and every peer may be reconciled with once more.
        self.reconciled.clear();
        self.spent = 0;
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
                    // a checksum decorative. And once a tick per peer (M50): a second digest
                    // carries the same state, is dropped by M49, and left M48 a false count.
                    if *checksum != self.cluster.checksum() && self.reconciled.insert(*from) {
                        out.push((addr, self.reconcile(from)));
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
                    } else if *checksum != self.cluster.checksum() && self.reconciled.insert(*from)
                    {
                        out.push((addr, self.reconcile(from)));
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
                    out.extend(self.parts(&addr, split(members)));
                }
                self.budgeted(from, out)
            }
            Message::TaggedDigest {
                from,
                buckets,
                tags,
            } => {
                // M43: answered whatever this view's size -- the answer depends on the
                // message, never on the receiver (spec review).
                self.mark_alive(from);
                // M48: it reads tags, so it is no longer sent a `Digest` for want of them.
                self.untagged.remove(from);
                // M52: but 256 tags show only that it reads 256. Its unanswered count is
                // cleared when it reads what we send it: finer tags from it, or 256 when 256
                // is what it is sent. ⚠️ Receiving 256 never steps a peer down by itself:
                // that held two growing current views at 256 for good (code review).
                if tags.len() > crate::cluster::LEAVES {
                    self.coarse.remove(from);
                    self.unanswered.remove(from);
                } else if self.leaves_for(from) == crate::cluster::LEAVES_PER_BUCKET {
                    self.unanswered.remove(from);
                }
                let per = tags.len() / crate::cluster::BUCKETS;
                let send = self.leaves_to_send(buckets, tags);
                let mut out = Vec::new();
                // ⚠️ Answered even when nothing differs (M48): one empty `Part`, so the
                // sender's silence means an old build or loss -- never a peer that piggyback
                // already levelled.
                if let Some(addr) = self.addr_of(from) {
                    let members = self
                        .cluster
                        .members()
                        .filter(|m| send.get(crate::cluster::leaf_at(&m.id, per)) == Some(&true))
                        .cloned()
                        .collect();
                    out.extend(self.parts(&addr, split(members)));
                }
                self.budgeted(from, out)
            }
            Message::Part { from, members } => {
                self.unanswered.remove(from);
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
                    // M44: the whole view, as one `Sync` when it fits a datagram and as
                    // `Part`s when it does not. A `Part` draws no reply, so this cannot loop,
                    // and the peer learns everything a `Sync` would have told it.
                    let mut chunks = split(self.cluster.members().cloned().collect());
                    if chunks.len() == 1 {
                        out.push((
                            addr,
                            Message::Sync {
                                from: *self.cluster.me(),
                                members: chunks.remove(0),
                            },
                        ));
                    } else {
                        out.extend(self.parts(&addr, chunks));
                    }
                }
                self.budgeted(from, out)
            }
        }
    }

    /// `out`, a digest's answer to `from`, if this tick may still send it (M49): once per peer
    /// per tick, and past the tick's first answer only within [`ANSWER_BUDGET`].
    /// ⚠️ The first answer goes whatever its size: a full-view answer past the budget is a
    /// cold joiner's, a heal's, or an untagged peer's, and would otherwise never be sent.
    fn budgeted(&mut self, from: &NodeId, out: Vec<(String, Message)>) -> Vec<(String, Message)> {
        if out.is_empty() {
            return out;
        }
        if !self.answered.insert(*from) {
            self.repeat_drops += 1;
            return Vec::new();
        }
        let len: usize = out.iter().map(|(_, m)| answer_len(m)).sum();
        if self.spent > 0 && self.spent + len > ANSWER_BUDGET {
            self.budget_drops += 1;
            return Vec::new();
        }
        self.spent += len;
        out
    }

    /// Digests dropped because a tick's answer budget was spent (M49). Honest traffic leaves
    /// this at 0 in the `Sim` at 200, 800 and 1,600 members, since M52 spread the indirect
    /// probes.
    #[must_use]
    pub fn budget_drops(&self) -> u64 {
        self.budget_drops
    }

    /// Digests dropped because their peer had been answered that tick (M49). The two carry
    /// the same state, so nothing is lost -- but a dropped repeat can leave one on M48's
    /// unanswered count: false `untagged` marks rose from 37 to 74 over the 200-member
    /// heavy-loss run (M49's code review). M50 sends a peer one reconciliation a tick, which
    /// took that run to 15 repeats and 1 mark.
    #[must_use]
    pub fn repeat_drops(&self) -> u64 {
        self.repeat_drops
    }

    /// Times a peer was newly marked untagged, M48's fallback (M50). Every current build
    /// reads tags, so in a fleet of them each mark is false.
    #[must_use]
    pub fn untagged_marks(&self) -> u64 {
        self.untagged_marks
    }

    /// `chunks` to `addr`, one `Part` each (M44).
    fn parts(&self, addr: &str, chunks: Vec<Vec<Member>>) -> Vec<(String, Message)> {
        chunks
            .into_iter()
            .map(|members| {
                (
                    addr.to_owned(),
                    Message::Part {
                        from: *self.cluster.me(),
                        members,
                    },
                )
            })
            .collect()
    }

    /// What a checksum mismatch sends (M34): a `Digest` past [`RECONCILE_WHOLE_UP_TO`]
    /// members, else a whole `Sync`, which is cheap at that size and converges in fewer
    /// exchanges. Past [`TAG_ABOVE`], a `TaggedDigest` (M43).
    ///
    /// M48: a peer that has left [`UNANSWERED`] tags unanswered is sent a `Digest`, which
    /// every build since M34 answers, so a mixed-version fleet still reconciles.
    fn reconcile(&mut self, peer: &NodeId) -> Message {
        if self.cluster.len() > TAG_ABOVE && !self.untagged.contains(peer) {
            let mut per = self.leaves_for(peer);
            // M52: a member that left finer tags unanswered is stepped down to 256 before it
            // is sent a `Digest`, since a build from M43 to M51 reads 256.
            if per > crate::cluster::LEAVES_PER_BUCKET
                && self.unanswered.get(peer) >= Some(&UNANSWERED)
                && self.cluster.state(peer).is_some()
            {
                self.coarse.insert(*peer, self.tick);
                self.unanswered.remove(peer);
                per = crate::cluster::LEAVES_PER_BUCKET;
            }
            let sent = self.unanswered.entry(*peer).or_insert(0);
            if *sent < UNANSWERED {
                *sent += 1;
                return Message::TaggedDigest {
                    from: *self.cluster.me(),
                    buckets: self.cluster.buckets().to_vec(),
                    tags: self.cluster.leaf_tags_at(per),
                };
            }
            if self.untagged.insert(*peer) {
                self.untagged_marks += 1;
            }
        }
        if self.cluster.len() > RECONCILE_WHOLE_UP_TO {
            Message::Digest {
                from: *self.cluster.me(),
                buckets: self.cluster.buckets().to_vec(),
            }
        } else {
            self.sync()
        }
    }

    /// Leaves per bucket in a `TaggedDigest` to `peer` (M52): the view's, unless `peer` was
    /// stepped down within the last [`COARSE_TICKS`].
    fn leaves_for(&self, peer: &NodeId) -> usize {
        match self.coarse.get(peer) {
            Some(at) if self.tick - at < COARSE_TICKS => crate::cluster::LEAVES_PER_BUCKET,
            _ => leaves_per_bucket(self.cluster.len()),
        }
    }

    /// Which leaves a `TaggedDigest` is answered with (M43): one flag per leaf at the peer's
    /// leaf count (M52), so the answer is one pass over the members against it, never a list
    /// searched per member (spec review). For each bucket whose sum differs:
    /// - every leaf, when this view holds [`LEAF_ABOVE`] or fewer of its members;
    /// - every leaf, when no tag in it differs: a collision. ⚠️ So a difference the 64-bit
    ///   sums show is never left unanswered;
    /// - otherwise, the leaves whose tags differ.
    fn leaves_to_send(&self, buckets: &[u64], tags: &[u8]) -> Vec<bool> {
        // M52: the leaf count is the peer's, read from its tags; the decoder admits only
        // 256 × 2^k of them.
        let per = tags.len() / crate::cluster::BUCKETS;
        let mine = self.cluster.buckets();
        let my_tags = self.cluster.leaf_tags_at(per);
        let mut count = [0usize; crate::cluster::BUCKETS];
        for m in self.cluster.members() {
            if let Some(c) = count.get_mut(crate::cluster::bucket_of(&m.id)) {
                *c += 1;
            }
        }
        let mut send = vec![false; tags.len()];
        for (b, leaves) in send.chunks_mut(per).enumerate() {
            if buckets.get(b) == mine.get(b) {
                continue;
            }
            let differ = |j: usize| tags.get(b * per + j) != my_tags.get(b * per + j);
            let whole = count.get(b).copied().unwrap_or(0) <= LEAF_ABOVE || !(0..per).any(differ);
            for (j, leaf) in leaves.iter_mut().enumerate() {
                *leaf = whole || differ(j);
            }
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

    /// The peers asked to probe `avoid` for us: [`INDIRECT_PROBES`] consecutive members of the
    /// live view, less this node and the target, from a start drawn from both ids and the tick
    /// (M52).
    /// ⚠️ Before M52 every node took the first three of its view, so the whole fleet relayed
    /// through three nodes: at 1,600 members they spent M49's answer budget every tick, 98,601
    /// drops in 200 rounds, and their lost answers were M48's false marks.
    fn helpers(&self, avoid: &NodeId) -> Vec<String> {
        let me = self.cluster.me();
        let live: Vec<&Member> = self
            .cluster
            .alive()
            .into_iter()
            .filter(|m| m.id != *me && m.id != *avoid)
            .collect();
        if live.is_empty() {
            return Vec::new();
        }
        let h = mix(
            crate::cluster::fnv(me) ^ crate::cluster::fnv(avoid),
            self.tick,
        );
        let start = (h % live.len() as u64) as usize;
        live.iter()
            .cycle()
            .skip(start)
            .take(INDIRECT_PROBES.min(live.len()))
            .map(|m| m.addr.clone())
            .collect()
    }
}

/// A digest answer's encoded length (M49): a `Part`'s or `Sync`'s header and members, as
/// [`split`] sums them.
fn answer_len(m: &Message) -> usize {
    use crate::wire::{MEMBERS_HEADER, member_len};
    match m {
        Message::Part { members, .. } | Message::Sync { members, .. } => {
            MEMBERS_HEADER + members.iter().map(member_len).sum::<usize>()
        }
        other => other.encode().len(),
    }
}

/// `members`, in order, cut into chunks whose `Part` or `Sync` encodes to at most
/// [`MAX_DATAGRAM`](crate::wire::MAX_DATAGRAM) bytes (M44).
///
/// - The length is summed exactly, never estimated: a datagram one byte over is dropped at
///   the socket, silently.
/// - A member that cannot fit even alone gets a chunk of its own. The socket drops it, as it
///   always did; leaving it out here would hide that it exists.
/// - Always at least one chunk, so an empty answer is still the one empty `Part` it was.
fn split(members: Vec<Member>) -> Vec<Vec<Member>> {
    use crate::wire::{MAX_DATAGRAM, MEMBERS_HEADER, member_len};
    let mut out = Vec::new();
    let mut chunk = Vec::new();
    let mut len = MEMBERS_HEADER;
    for m in members {
        let n = member_len(&m);
        if !chunk.is_empty() && len + n > MAX_DATAGRAM {
            out.push(std::mem::take(&mut chunk));
            len = MEMBERS_HEADER;
        }
        len += n;
        chunk.push(m);
    }
    out.push(chunk);
    out
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
        // A fresh copy's first reconciliation, so `p`'s own unanswered count is untouched (M48).
        let Message::TaggedDigest {
            from: sender,
            buckets,
            mut tags,
        } = Protocol::new(p.cluster.clone()).reconcile(&nid(1))
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
        // M49: a peer's digests are answered once a tick, and each exchange here is a round.
        a.tick(0);
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
        a.tick(0);
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
            // M52: the mask is a flag per leaf, where M43's was 16 bits a bucket; the same
            // three facts are asserted.
            let mask = a.leaves_to_send(&buckets, &tags);
            let per = crate::cluster::LEAVES_PER_BUCKET;
            let sent: Vec<usize> = (0..mask.len()).filter(|l| mask[*l]).collect();
            let want: Vec<usize> = if size <= LEAF_ABOVE {
                (b * per..(b + 1) * per).collect()
            } else {
                vec![leaf]
            };
            assert_eq!(sent, want, "a bucket of {size}");
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

    /// M52: every timed-out probe asked the first three members of its view, so a 1,600-member
    /// fleet relayed through three nodes and they spent M49's budget every tick.
    #[test]
    fn relays_are_spread_across_the_view() {
        let live: Vec<u8> = (1..=64).collect();
        let mut p = protocol(&live, &[]);
        let mut asked = std::collections::BTreeSet::new();
        for tick in 1..=64 {
            p.tick = tick;
            let helpers = p.helpers(&nid(1));
            let mut distinct = helpers.clone();
            distinct.sort();
            distinct.dedup();
            assert_eq!(distinct.len(), INDIRECT_PROBES, "tick {tick}: {helpers:?}");
            assert!(
                !helpers.iter().any(|h| h == "n0" || h == "n1"),
                "tick {tick}: this node or the target asked to relay: {helpers:?}"
            );
            asked.extend(helpers);
        }
        assert!(
            asked.len() >= 32,
            "64 ticks asked only {} members",
            asked.len()
        );

        let mut first = std::collections::BTreeSet::new();
        for me in 65..=80u8 {
            let mut c = Cluster::new(nid(me), format!("n{me}"), "az-a".to_owned());
            for &n in &live {
                c.join(nid(n), format!("n{n}"), "az-a".to_owned());
            }
            let mut q = Protocol::new(c);
            q.tick = 7;
            first.extend(q.helpers(&nid(1)).into_iter().take(1));
        }
        assert!(
            first.len() >= 8,
            "16 nodes chose {} first helpers",
            first.len()
        );

        // Pinned against an independent model of the rule (Python, in the M52 ledger).
        for (target, tick, want) in [
            (1u8, 1u64, ["n38", "n39", "n40"]),
            (5, 2, ["n3", "n4", "n6"]),
            (64, 3, ["n11", "n12", "n13"]),
            (33, 1_000, ["n20", "n21", "n22"]),
        ] {
            p.tick = tick;
            assert_eq!(
                p.helpers(&nid(target)),
                want,
                "target {target} at tick {tick}"
            );
        }
        p.tick = 7;
        let by_target: std::collections::BTreeSet<String> = (1..=16u8)
            .filter_map(|t| p.helpers(&nid(t)).into_iter().next())
            .collect();
        assert!(
            by_target.len() >= 8,
            "16 targets drew {} first helpers",
            by_target.len()
        );

        let small = protocol(&[1, 2], &[]);
        assert_eq!(small.helpers(&nid(1)), vec!["n2".to_owned()]);
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

    /// M44: a node id from a number up to 65,535, big-endian in the first two bytes, so the
    /// view yields them in number order and 256 is `nid(1)`.
    fn wide(i: u16) -> NodeId {
        let mut b = [0u8; 16];
        b[..2].copy_from_slice(&i.to_be_bytes());
        b
    }

    /// M44: node 0 and members `1..=n`, each with a 40-byte address and an 8-byte zone.
    fn crowd(n: u16) -> Protocol {
        let mut c = Cluster::new(wide(0), format!("{:040}", 0), "zone-abc".to_owned());
        for i in 1..=n {
            c.join(wide(i), format!("{i:040}"), "zone-abc".to_owned());
        }
        Protocol::new(c)
    }

    /// M44: the members of every message in `out`, which must all be `Part`s that fit.
    fn parts_of(out: &[(String, Message)]) -> Vec<Vec<Member>> {
        out.iter()
            .map(|(_, m)| {
                assert!(
                    m.encode().len() <= crate::wire::MAX_DATAGRAM,
                    "a message of {} bytes",
                    m.encode().len()
                );
                match m {
                    Message::Part { members, .. } => members.clone(),
                    other => panic!("expected a Part, got {other:?}"),
                }
            })
            .collect()
    }

    /// M44: a member's encoded length, by encoding it alone and taking off the header.
    fn size_of(m: &Member) -> usize {
        let part = Message::Part {
            from: nid(0),
            members: vec![m.clone()],
        };
        part.encode().len() - 21
    }

    fn view_of(p: &Protocol) -> Vec<Member> {
        p.cluster.members().cloned().collect()
    }

    /// M44: the answer, split tightly: every `Part` but the last would not fit one more.
    fn assert_split(p: &Protocol, out: &[(String, Message)]) {
        let parts = parts_of(out);
        assert!(parts.len() > 1, "{} members in one Part", view_of(p).len());
        assert_eq!(
            parts.concat(),
            view_of(p),
            "not every member once, in order"
        );
        for pair in parts.windows(2) {
            let full: usize = 21 + pair[0].iter().map(size_of).sum::<usize>();
            assert!(full + size_of(&pair[1][0]) > crate::wire::MAX_DATAGRAM);
        }
    }

    fn zero_digest(from: NodeId) -> Message {
        Message::Digest {
            from,
            buckets: vec![0; crate::cluster::BUCKETS],
        }
    }

    #[test]
    fn a_part_of_exactly_the_datagram_is_one_part() {
        // M44: two members whose `Part` is exactly `MAX_DATAGRAM` bytes travel together; one
        // byte more and they travel apart. Node 1 is 36 bytes; node 0 the rest. ⚠️ Sized from
        // the constant since M53 reserved the seal out of it: a literal 65,507 went stale.
        for (extra, parts) in [(0, 1), (1, 2)] {
            let addr = "a".repeat(crate::wire::MAX_DATAGRAM - 21 - 36 - 34 + extra);
            let mut c = Cluster::new(nid(0), addr, "z".to_owned());
            c.join(nid(1), "n1".to_owned(), "z".to_owned());
            let mut p = Protocol::new(c);
            let out = p.receive("n1", &zero_digest(nid(1)));
            let got = parts_of(&out);
            assert_eq!(got.len(), parts, "{extra} byte(s) over");
            assert_eq!(got.concat(), view_of(&p));
        }
    }

    #[test]
    fn a_large_answer_is_split_under_the_datagram() {
        let mut p = crowd(3_000);
        let out = p.receive("x", &zero_digest(wide(256)));
        assert_split(&p, &out);
    }

    #[test]
    fn a_large_tagged_answer_is_split_under_the_datagram() {
        // Every tag differs from this view's, so every leaf is sent.
        let mut p = crowd(3_000);
        let tags = p.cluster.leaf_tags().iter().map(|t| !t).collect();
        let msg = Message::TaggedDigest {
            from: wide(256),
            buckets: vec![0; crate::cluster::BUCKETS],
            tags,
        };
        let out = p.receive("x", &msg);
        assert_split(&p, &out);
    }

    #[test]
    fn a_sync_answer_that_cannot_fit_is_sent_as_parts() {
        let mut p = crowd(3_000);
        let sender = p.cluster.member(&wide(256)).cloned().expect("a member");
        let msg = Message::Sync {
            from: wide(256),
            members: vec![sender],
        };
        let out = p.receive("x", &msg);
        assert_split(&p, &out);

        // A small node absorbs them without a reply, and agrees with the big view.
        let mut c = Cluster::new(wide(1), format!("{:040}", 1), "zone-abc".to_owned());
        c.join(wide(256), format!("{:040}", 256), "zone-abc".to_owned());
        let mut s = Protocol::new(c);
        for (_, m) in &out {
            assert!(s.receive("x", m).is_empty());
        }
        assert_eq!(s.cluster.checksum(), p.cluster.checksum());

        // A view that fits answers with one `Sync`, as before.
        let mut small = protocol(&[1, 2], &[]);
        let msg = Message::Sync {
            from: nid(1),
            members: vec![record(1, "az-a", 0, State::Alive)],
        };
        let out = small.receive("n1", &msg);
        assert!(
            matches!(&out[..], [(_, Message::Sync { members, .. })] if members.len() == 3),
            "{out:?}"
        );
    }

    #[test]
    fn a_differing_bucket_this_view_holds_nothing_of_is_one_empty_part() {
        // M44: a split always yields a chunk, so an answer with no members is still the one
        // empty `Part` it was -- which tells the asker this node is alive.
        let mut p = protocol(&[1], &[]);
        let mut buckets = p.cluster.buckets().to_vec();
        let empty = buckets
            .iter()
            .position(|b| *b == 0)
            .expect("an empty bucket");
        buckets[empty] = 1;
        let out = p.receive(
            "n1",
            &Message::Digest {
                from: nid(1),
                buckets,
            },
        );
        assert_eq!(parts_of(&out), vec![Vec::<Member>::new()]);
    }

    #[test]
    fn an_oversize_member_is_sent_alone() {
        let mut c = Cluster::new(nid(0), "n0".to_owned(), "z".to_owned());
        for n in 1..=5u8 {
            let addr = if n == 3 {
                "a".repeat(70_000)
            } else {
                format!("n{n}")
            };
            c.join(nid(n), addr, "z".to_owned());
        }
        let mut p = Protocol::new(c);
        let out = p.receive("n1", &zero_digest(nid(1)));
        let ids = |ms: &[Member]| ms.iter().map(|m| m.id[0]).collect::<Vec<_>>();
        let mut got = Vec::new();
        for (_, m) in &out {
            let Message::Part { members, .. } = m else {
                panic!("expected a Part, got {m:?}");
            };
            if !members.iter().any(|m| m.id == nid(3)) {
                assert!(m.encode().len() <= crate::wire::MAX_DATAGRAM);
            }
            got.push(ids(members));
        }
        assert_eq!(got, vec![vec![0, 1, 2], vec![3], vec![4, 5]]);
    }

    /// M48: the reconciliation message in `out`, which a mismatched ping from `n1` drew.
    fn reconciled(out: &[(String, Message)]) -> &Message {
        out.iter()
            .map(|(_, m)| m)
            .find(|m| !matches!(m, Message::Ack { .. }))
            .expect("a mismatch drew no reconciliation")
    }

    /// M48: a ping from `n1` whose checksum differs from `p`'s.
    fn mismatched(p: &Protocol, seq: u64) -> Message {
        Message::Ping {
            from: nid(1),
            seq,
            checksum: p.cluster.checksum().wrapping_add(1),
            updates: Vec::new(),
        }
    }

    fn agreeing(p: &Protocol) -> Message {
        Message::TaggedDigest {
            from: nid(1),
            buckets: p.cluster.buckets().to_vec(),
            tags: p.cluster.leaf_tags().to_vec(),
        }
    }

    #[test]
    fn an_agreeing_tagged_digest_is_answered_empty() {
        // M48: silence must mean an old build or loss, never a peer that already agrees.
        let mut p = tagged_view(0);
        let digest = agreeing(&p);
        let out = p.receive("n1", &digest);
        assert!(
            matches!(&out[..], [(to, Message::Part { members, .. })] if to == "n1" && members.is_empty()),
            "{out:?}"
        );
    }

    #[test]
    fn a_peer_that_never_answers_tags_is_sent_a_digest() {
        let mut p = tagged_view(0);
        // M50: each mismatch is a round, so a tick before each: a peer is reconciled once a
        // tick.
        let kinds = |p: &mut Protocol, seq| match reconciled(&{
            p.tick(0);
            p.receive("n1", &mismatched(p, seq))
        }) {
            Message::TaggedDigest { .. } => "tagged",
            Message::Digest { .. } => "digest",
            other => panic!("{other:?}"),
        };
        let first: Vec<_> = (1..=3).map(|seq| kinds(&mut p, seq)).collect();
        assert_eq!(p.untagged_marks(), 0, "marked before its fourth silence");
        let first: Vec<_> = first.into_iter().chain([kinds(&mut p, 4)]).collect();
        assert_eq!(first, ["tagged", "tagged", "tagged", "digest"]);
        assert_eq!(p.untagged_marks(), 1);
        // Marked: an answer to the `Digest` does not unmark it...
        p.receive(
            "n1",
            &Message::Part {
                from: nid(1),
                members: Vec::new(),
            },
        );
        assert_eq!(kinds(&mut p, 5), "digest");
        // ...a `TaggedDigest` from it does, because only a build that reads tags sends one.
        let digest = agreeing(&p);
        p.receive("n1", &digest);
        assert_eq!(kinds(&mut p, 6), "tagged");
    }

    #[test]
    fn an_answering_peer_keeps_its_tags() {
        let mut p = tagged_view(0);
        for seq in 1..=10 {
            // M50: each mismatch is a round, and a peer is reconciled once a tick.
            p.tick(0);
            assert!(
                matches!(
                    reconciled(&p.receive("n1", &mismatched(&p, seq))),
                    Message::TaggedDigest { .. }
                ),
                "mismatch {seq}"
            );
            p.receive(
                "n1",
                &Message::Part {
                    from: nid(1),
                    members: Vec::new(),
                },
            );
        }
    }

    /// M48: `me`'s view of members 0 to 120, all but nodes 0 and 1 Dead.
    fn mostly_dead(me: u8) -> Protocol {
        let mut p = tagged_view(me);
        for n in 2..=120 {
            p.cluster.declare_dead(&nid(n));
        }
        p
    }

    #[test]
    fn a_mixed_version_pair_converges() {
        // Node 0 runs this build; node 1 is emulated as one from before M43: it drops every
        // `TaggedDigest` delivered to it, and sends M34's `Digest` where this build tags.
        let mut new = mostly_dead(0);
        let mut old = mostly_dead(1);
        // A record only the old node holds, past any piggyback.
        let newer = Member {
            id: nid(50),
            addr: "n50".to_owned(),
            zone: "az-a".to_owned(),
            incarnation: 9,
            state: State::Dead,
        };
        old.cluster.upsert(newer.clone());
        assert_ne!(new.cluster.checksum(), old.cluster.checksum());

        let as_old = |m: Message, old: &Protocol| match m {
            Message::TaggedDigest { from, .. } => Message::Digest {
                from,
                buckets: old.cluster.buckets().to_vec(),
            },
            other => other,
        };
        for t in 0..6u64 {
            let mut queue: Vec<(String, Message)> = new.tick(t);
            queue.extend(old.tick(t).into_iter().map(|(to, m)| (to, as_old(m, &old))));
            let mut steps = 0;
            while let Some((to, m)) = queue.pop() {
                steps += 1;
                assert!(steps < 1_000, "the exchange did not settle");
                if to == "n1" {
                    if matches!(m, Message::TaggedDigest { .. }) {
                        continue;
                    }
                    let replies = old.receive("n0", &m);
                    queue.extend(replies.into_iter().map(|(to, m)| (to, as_old(m, &old))));
                } else if to == "n0" {
                    queue.extend(new.receive("n1", &m));
                }
            }
            if new.cluster.member(&nid(50)) == Some(&newer) {
                break;
            }
        }
        assert_eq!(new.cluster.member(&nid(50)), Some(&newer));
        assert_eq!(new.cluster.checksum(), old.cluster.checksum());
    }

    #[test]
    fn a_peer_draws_one_answer_a_tick() {
        // M49: a second digest from the same peer in one tick is not answered.
        let mut p = tagged_view(0);
        assert!(!p.receive("n1", &zero_digest(nid(1))).is_empty());
        assert!(p.receive("n1", &zero_digest(nid(1))).is_empty());
        assert_eq!((p.repeat_drops(), p.budget_drops()), (1, 0));
        assert!(
            !p.receive("n2", &zero_digest(nid(2))).is_empty(),
            "another peer, the same tick"
        );
        p.tick(0);
        assert!(!p.receive("n1", &zero_digest(nid(1))).is_empty());
    }

    #[test]
    fn a_tick_sends_at_most_its_answer_budget() {
        // M49: about 81 KB an answer, so three fit in four datagrams and a fourth does not.
        let mut p = crowd(1_000);
        let answered = (1..=10u16)
            .filter(|i| !p.receive("x", &zero_digest(wide(*i))).is_empty())
            .count();
        assert_eq!(answered, 3);
        assert_eq!((p.budget_drops(), p.repeat_drops()), (7, 0));
        p.tick(0);
        assert!(!p.receive("x", &zero_digest(wide(1))).is_empty());
    }

    #[test]
    fn a_sync_answer_counts_against_the_budget() {
        let mut p = crowd(1_000);
        for i in 1..=3u16 {
            assert!(!p.receive("x", &zero_digest(wide(i))).is_empty());
        }
        let sender = p.cluster.member(&wide(11)).cloned().expect("a member");
        let msg = Message::Sync {
            from: wide(11),
            members: vec![sender],
        };
        assert!(
            p.receive("x", &msg).is_empty(),
            "a whole view past the budget"
        );
    }

    #[test]
    fn the_ticks_first_answer_is_sent_whatever_its_size() {
        // M49: about 284 KB, past the budget, and still sent -- or a view this large could
        // never answer a joiner.
        let mut p = crowd(3_500);
        let out = p.receive("x", &zero_digest(wide(1)));
        assert_eq!(parts_of(&out).concat(), view_of(&p));
        assert!(p.receive("x", &zero_digest(wide(2))).is_empty());
        p.tick(0);
        assert!(!p.receive("x", &zero_digest(wide(2))).is_empty());
    }

    #[test]
    fn answers_that_exactly_fill_the_budget_are_sent() {
        // M49: two answers of exactly half the budget each: both go, and the next does not.
        // Members 0-2 are 36 bytes; 3 and 4 have addresses sized so the whole view splits
        // into two `Part`s of exactly `MAX_DATAGRAM` each: 21 + 108 + (34 + a) and 21 + (34 + b).
        // ⚠️ Sized from the constant since M53 (spec review): with the old literals, member 4's
        // own `Part` outgrew the smaller datagram and could not be sent at all.
        let mut c = Cluster::new(nid(0), "n0".to_owned(), "z".to_owned());
        c.join(nid(1), "n1".to_owned(), "z".to_owned());
        c.join(nid(2), "n2".to_owned(), "z".to_owned());
        c.join(
            nid(3),
            "a".repeat(crate::wire::MAX_DATAGRAM - 21 - 108 - 34),
            "z".to_owned(),
        );
        c.join(
            nid(4),
            "b".repeat(crate::wire::MAX_DATAGRAM - 21 - 34),
            "z".to_owned(),
        );
        let mut p = Protocol::new(c);
        let len =
            |out: &[(String, Message)]| out.iter().map(|(_, m)| m.encode().len()).sum::<usize>();
        let first = p.receive("n1", &zero_digest(nid(1)));
        assert_eq!(len(&first), ANSWER_BUDGET / 2);
        let second = p.receive("n2", &zero_digest(nid(2)));
        assert_eq!(
            len(&second),
            ANSWER_BUDGET / 2,
            "the budget, exactly, is within it"
        );
        assert!(p.receive("n3", &zero_digest(nid(3))).is_empty());
        assert_eq!(p.budget_drops(), 1);
    }

    #[test]
    fn a_digest_that_draws_nothing_does_not_spend_its_peers_answer() {
        // M49: only an answer counts. A digest that agrees draws nothing, and the same peer's
        // next digest that tick is still answered.
        let mut p = tagged_view(0);
        let agreeing = Message::Digest {
            from: nid(1),
            buckets: p.cluster.buckets().to_vec(),
        };
        assert!(p.receive("n1", &agreeing).is_empty());
        assert!(!p.receive("n1", &zero_digest(nid(1))).is_empty());
        assert_eq!(p.repeat_drops(), 0);
    }

    #[test]
    fn a_peer_is_reconciled_once_a_tick() {
        // M50: one reconciliation a peer a tick, whichever path the mismatch arrives on.
        let reconciles = |out: &[(String, Message)]| {
            out.iter().any(|(_, m)| {
                matches!(
                    m,
                    Message::TaggedDigest { .. } | Message::Digest { .. } | Message::Sync { .. }
                )
            })
        };
        let mut p = tagged_view(0);
        let wrong = p.cluster.checksum().wrapping_add(1);
        assert!(reconciles(&p.receive("n1", &mismatched(&p, 1))));
        // The case that left M49's marks: the same peer's `Ack`, mismatched, that tick.
        let ack = Message::Ack {
            from: nid(1),
            seq: 99,
            checksum: wrong,
            updates: Vec::new(),
        };
        assert!(
            !reconciles(&p.receive("n1", &ack)),
            "the Ack path reconciled again"
        );
        let again = p.receive("n1", &mismatched(&p, 2));
        assert!(!reconciles(&again));
        assert!(
            again.iter().any(|(_, m)| matches!(m, Message::Ack { .. })),
            "a ping is still acked"
        );
        let other = Message::Ping {
            from: nid(2),
            seq: 3,
            checksum: wrong,
            updates: Vec::new(),
        };
        assert!(
            reconciles(&p.receive("n2", &other)),
            "another peer, the same tick"
        );
        p.tick(0);
        assert!(reconciles(&p.receive("n1", &mismatched(&p, 4))));
    }

    /// M52: the tag count of `p`'s `TaggedDigest` to node 1.
    fn tag_count(p: &mut Protocol) -> usize {
        match p.reconcile(&nid(1)) {
            Message::TaggedDigest { tags, .. } => tags.len(),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn the_leaf_count_follows_the_view() {
        // `crowd(n)` holds n + 1 members.
        for (members, tags) in [(400u16, 256), (401, 512), (800, 512), (801, 1_024)] {
            assert_eq!(
                tag_count(&mut crowd(members - 1)),
                tags,
                "{members} members"
            );
        }
        // The cap: 7 doublings from 25,601 members up, never 8 (code review).
        for (members, per) in [
            (0usize, 16usize),
            (25_600, 1_024),
            (25_601, 2_048),
            (51_200, 2_048),
            (51_201, 2_048),
            (usize::MAX, 2_048),
        ] {
            assert_eq!(leaves_per_bucket(members), per, "{members} members");
        }
    }

    /// M52: two views of 801 members, the second holding member `m` at a newer incarnation.
    fn differing_in(m: NodeId) -> (Protocol, Protocol) {
        let a = crowd(800);
        let mut b = crowd(800);
        b.cluster.refute(&m, 1);
        (a, b)
    }

    #[test]
    fn a_finer_answer_sends_only_the_fine_leaf() {
        // A member whose coarse leaf holds more of the view than its fine leaf at 1,024.
        let a = crowd(800);
        let ids: Vec<NodeId> = a.cluster.members().map(|m| m.id).collect();
        let fine = |m: &NodeId| crate::cluster::leaf_at(m, 64);
        let coarse = |m: &NodeId| crate::cluster::leaf_of(m);
        let m = *ids
            .iter()
            .find(|m| {
                let c = ids.iter().filter(|i| coarse(i) == coarse(m)).count();
                let f = ids.iter().filter(|i| fine(i) == fine(m)).count();
                m[..2] != [0, 0] && m[..2] != [1, 0] && c > f && c > LEAF_ABOVE
            })
            .expect("a member whose coarse leaf is larger");
        let (mut a, b) = differing_in(m);
        let mut want: Vec<NodeId> = ids
            .iter()
            .filter(|i| fine(i) == fine(&m))
            .copied()
            .collect();
        want.sort_unstable();
        // `b` asks `a` at its own leaf count, 1,024.
        let digest = Protocol::new(b.cluster.clone()).reconcile(&wide(256));
        let Message::TaggedDigest { tags, .. } = &digest else {
            panic!("{digest:?}")
        };
        assert_eq!(tags.len(), 1_024);
        let mut got: Vec<NodeId> = parts_of(&a.receive("x", &digest))
            .concat()
            .iter()
            .map(|x| x.id)
            .collect();
        got.sort_unstable();
        assert_eq!(got, want, "the fine leaf, and nothing else");
        // The same difference asked at 256 leaves draws the larger coarse leaf.
        a.tick(0);
        let coarse_digest = Message::TaggedDigest {
            from: wide(1),
            buckets: b.cluster.buckets().to_vec(),
            tags: b.cluster.leaf_tags().to_vec(),
        };
        let coarse_n = parts_of(&a.receive("x", &coarse_digest)).concat().len();
        assert!(coarse_n > want.len(), "{coarse_n} against {}", want.len());
    }

    /// M52: the tag count of `p`'s reconciliation with `peer`.
    fn tags_to(p: &mut Protocol, peer: NodeId) -> usize {
        match p.reconcile(&peer) {
            Message::TaggedDigest { tags, .. } => tags.len(),
            other => panic!("{other:?}"),
        }
    }

    /// M52: a `TaggedDigest` from `from` carrying `p`'s own sums at `per` leaves a bucket.
    fn digest_from(p: &Protocol, from: NodeId, per: usize) -> Message {
        Message::TaggedDigest {
            from,
            buckets: p.cluster.buckets().to_vec(),
            tags: p.cluster.leaf_tags_at(per),
        }
    }

    #[test]
    fn a_peer_that_leaves_finer_tags_unanswered_is_stepped_down() {
        // M52: a build from M43 to M51 refuses more than 256 tags, but sends 256 itself.
        let mut p = crowd(800);
        let old = wide(1);
        let addr = format!("{:040}", 1);
        for _ in 0..UNANSWERED {
            assert_eq!(tags_to(&mut p, old), 1_024);
            // Its own 256-tag digests show only that it reads 256: the count stands.
            let d = digest_from(&p, old, 16);
            p.receive(&addr, &d);
            p.tick(0);
        }
        assert_eq!(
            tags_to(&mut p, old),
            256,
            "stepped down after three silences"
        );
        assert_eq!(p.untagged_marks(), 0, "stepped down, not sent a Digest");
        // Sent 256, a 256-tag digest from it now clears the count: no Digest follows.
        assert_eq!(tags_to(&mut p, old), 256);
        let d = digest_from(&p, old, 16);
        p.receive(&addr, &d);
        for _ in 0..UNANSWERED {
            assert_eq!(tags_to(&mut p, old), 256);
        }
        // A finer digest from it ends the step-down at once.
        let fine = digest_from(&p, old, 64);
        p.receive(&addr, &fine);
        assert_eq!(tags_to(&mut p, old), 1_024);
    }

    #[test]
    fn a_step_down_expires() {
        let mut p = crowd(800);
        // Away from 0, so the expiry is measured from the step-down, not from the start.
        p.tick = 1_000;
        let old = wide(1);
        for _ in 0..UNANSWERED {
            assert_eq!(tags_to(&mut p, old), 1_024);
        }
        assert_eq!(tags_to(&mut p, old), 256);
        // Set directly: driving `tick` 64 times would probe, suspect and reconcile too.
        let at = p.tick;
        p.tick = at + COARSE_TICKS - 1;
        assert_eq!(tags_to(&mut p, old), 256, "within COARSE_TICKS");
        p.tick = at + COARSE_TICKS;
        assert_eq!(tags_to(&mut p, old), 1_024, "finer tags tried again");
    }

    #[test]
    fn receiving_coarse_tags_never_steps_a_peer_down() {
        // M52 code review: a first version stepped down any peer that sent 256 tags to a view
        // past 400, so two current views growing through 400 held each other at 256 for good.
        let mut p = crowd(800);
        let peer = wide(1);
        let addr = format!("{:040}", 1);
        for _ in 0..10 {
            assert_eq!(tags_to(&mut p, peer), 1_024);
            let d = digest_from(&p, peer, 16);
            p.receive(&addr, &d);
            p.receive(
                &addr,
                &Message::Part {
                    from: peer,
                    members: Vec::new(),
                },
            );
            p.tick(0);
        }
        assert!(p.coarse.is_empty(), "{:?}", p.coarse);
    }

    #[test]
    fn only_a_member_past_400_is_stepped_down() {
        // M52 code review: the record must stay bounded by the view.
        let mut p = crowd(800);
        let stranger = nid(200);
        assert!(p.cluster.state(&stranger).is_none());
        for _ in 0..UNANSWERED {
            assert_eq!(tags_to(&mut p, stranger), 1_024);
        }
        assert!(matches!(p.reconcile(&stranger), Message::Digest { .. }));
        assert!(p.coarse.is_empty(), "{:?}", p.coarse);
        // At 400 members there is nothing finer to step down from: M48's fallback, as before,
        // and a 256-tag digest from the peer clears its count, as before.
        let mut small = crowd(399);
        let old = wide(1);
        let addr = format!("{:040}", 1);
        for _ in 0..UNANSWERED - 1 {
            assert_eq!(tags_to(&mut small, old), 256);
        }
        let d = digest_from(&small, old, 16);
        small.receive(&addr, &d);
        for _ in 0..UNANSWERED {
            assert_eq!(tags_to(&mut small, old), 256);
        }
        assert!(matches!(small.reconcile(&old), Message::Digest { .. }));
        assert!(small.coarse.is_empty(), "{:?}", small.coarse);
    }
}
