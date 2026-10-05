//! The member set, and the checksum two nodes use to agree in eight bytes.

use std::collections::BTreeMap;

/// A node identity: 16 raw bytes.
///
/// ⚠️ Raw, not the 32-character hex string M4b used. Identity is the bulk of what a member
/// record costs, it is carried in every reconciliation, and a hex string spends two bytes to
/// say what one byte says. At 10,000 members that is 160 KB of pure encoding overhead.
pub type NodeId = [u8; 16];

/// What this node currently believes about a peer.
///
/// ⚠️ `Suspect` is a state rather than a boolean because the difference between "not
/// answering" and "gone" is the difference between a slow node and a lost one, and collapsing
/// them is how a fleet evicts its own healthy members under load.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// Answering probes, directly or indirectly.
    Alive,
    /// Missed a probe. Still routed to, still counted, not yet gone.
    Suspect,
    /// Gone. Remembered, so a stale message cannot resurrect it.
    Dead,
}

impl State {
    /// A stable tag for the checksum. ⚠️ Never `as` on the enum: reordering the variants would
    /// silently change every checksum in the fleet, and a fleet that disagrees about its
    /// checksum reconciles forever.
    const fn tag(self) -> u64 {
        match self {
            Self::Alive => 1,
            Self::Suspect => 2,
            Self::Dead => 3,
        }
    }
}

/// One member, as this node currently believes it to be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Member {
    /// Identity, stable for the life of the process.
    pub id: NodeId,
    /// Where to probe it.
    pub addr: String,
    /// Which availability zone it is in.
    ///
    /// ⚠️ Carried in membership because a node cannot filter peers it cannot identify. D-79
    /// gives each AZ its own placement ring, and a cell's roster must hold only that cell's
    /// members — which is impossible if the gossip view does not say who is where. It is
    /// **self-declared**: nothing here can check it, and a node claiming the wrong zone joins
    /// the wrong cell. D-83's health bulletin is where that would later be cross-checked.
    pub zone: String,
    /// Bumped by the member itself to refute a suspicion. Higher always wins.
    pub incarnation: u64,
    /// What this node believes about it.
    pub state: State,
}

impl Member {
    /// The member's contribution to the cluster checksum.
    ///
    /// ⚠️ Covers identity, incarnation and state — **not** the address, which never changes
    /// for a given identity, and **not** any counter that advances on its own. A checksum
    /// over something that ticks can never match, which is exactly why this crate exists.
    fn fingerprint(&self) -> u64 {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        let mut mix = |b: u8| {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x100_0000_01b3);
        };
        for b in self.id {
            mix(b);
        }
        // ⚠️ The zone is part of the identity a checksum must cover: a member that moved zone
        // is a different placement, and two nodes disagreeing about it must reconcile.
        for b in self.zone.as_bytes() {
            mix(*b);
        }
        for b in self.incarnation.to_le_bytes() {
            mix(b);
        }
        for b in self.state.tag().to_le_bytes() {
            mix(b);
        }
        h
    }
}

/// This node's view of the fleet.
#[derive(Debug, Clone)]
pub struct Cluster {
    me: NodeId,
    members: BTreeMap<NodeId, Member>,
    checksum: u64,
    /// The checksum split by bucket (M34), kept beside it in `insert`: their wrapping sum is
    /// the checksum, so two nodes that disagree can find WHERE in 16 sums.
    buckets: [u64; BUCKETS],
    /// Each bucket split again into [`LEAVES_PER_BUCKET`] leaves (M43), kept in the same
    /// `insert`: leaf `b * 16 + j` is bucket `b`'s `j`th, and a bucket is its leaves' sum.
    leaves: [u64; LEAVES],
}

/// How many leaves each bucket is split into (M43).
pub const LEAVES_PER_BUCKET: usize = 16;

/// Every leaf: one tag each in a `TaggedDigest` (M43).
pub const LEAVES: usize = BUCKETS * LEAVES_PER_BUCKET;

/// How many buckets the checksum is split into (M34). A `Digest` carries one sum per bucket,
/// and a `Part` the members of the buckets that differ: about N/16 per differing bucket.
pub const BUCKETS: usize = 16;

/// A member's bucket: FNV-1a over its id, mod [`BUCKETS`]. ⚠️ Every node must compute the same
/// one, in a fleet of mixed versions too, so it is written out rather than taken from a
/// hasher whose output may change.
#[must_use]
pub fn bucket_of(id: &NodeId) -> usize {
    (fnv(id) % BUCKETS as u64) as usize
}

pub(crate) fn fnv(id: &NodeId) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in id {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    h
}

/// A member's leaf (M43): its bucket times 16, plus a second index from the high half of the
/// same FNV-1a, so it is independent of the low bits that chose the bucket. ⚠️ Written out for
/// the same reason [`bucket_of`] is: every node in a mixed fleet must compute the same one.
#[must_use]
pub fn leaf_of(id: &NodeId) -> usize {
    leaf_at(id, LEAVES_PER_BUCKET)
}

/// A member's leaf when each bucket is split into `per` leaves (M52): M43's formula with
/// `per` in place of 16, so at 16 it is [`leaf_of`].
///
/// ⚠️ **Finer leaves nest by low bits, not by range.** Taking `(h >> 32) mod per` keeps the
/// low bits, so the coarse leaf of a fine one at index `c` within its bucket is
/// `c mod per_coarse`, and a coarse leaf's children are spaced `per_coarse` apart.
#[must_use]
pub fn leaf_at(id: &NodeId, per: usize) -> usize {
    let h = fnv(id);
    (h % BUCKETS as u64) as usize * per + ((h >> 32) % per as u64) as usize
}

/// A leaf sum's one-byte tag (M43).
fn tag_of(sum: u64) -> u8 {
    (sum.wrapping_mul(0x9e37_79b9_7f4a_7c15) >> 56) as u8
}

impl Cluster {
    /// A cluster containing only this node, alive.
    #[must_use]
    pub fn new(me: NodeId, addr: String, zone: String) -> Self {
        let mut c = Self {
            me,
            members: BTreeMap::new(),
            checksum: 0,
            buckets: [0; BUCKETS],
            leaves: [0; LEAVES],
        };
        c.insert(Member {
            id: me,
            addr,
            zone,
            incarnation: 0,
            state: State::Alive,
        });
        c
    }

    /// This node's own identity.
    #[must_use]
    pub fn me(&self) -> &NodeId {
        &self.me
    }

    /// One member, if known.
    #[must_use]
    pub fn member(&self, id: &NodeId) -> Option<&Member> {
        self.members.get(id)
    }

    /// Every member, including the dead, in identity order.
    pub fn members(&self) -> impl Iterator<Item = &Member> {
        self.members.values()
    }

    /// A member's incarnation, if known.
    #[must_use]
    pub fn incarnation(&self, id: &NodeId) -> Option<u64> {
        self.members.get(id).map(|m| m.incarnation)
    }

    /// Everything this node knows of, including the dead it remembers.
    #[must_use]
    pub fn len(&self) -> usize {
        self.members.len()
    }

    /// Whether the view is empty. Never true — a node always knows itself.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.members.is_empty()
    }

    /// How many members are worth routing to, without building a list of them.
    ///
    /// ⚠️ Exists because the caller that asks most often only wants the number. Cloning a
    /// thousand addresses once a second, per node, to discover that a count did not change is
    /// a cost that scales with the fleet and buys nothing.
    #[must_use]
    pub fn alive_count(&self) -> usize {
        self.members
            .values()
            .filter(|m| m.state != State::Dead)
            .count()
    }

    /// The members worth routing to: alive, and suspects that have not yet been given up on.
    #[must_use]
    pub fn alive(&self) -> Vec<&Member> {
        self.members
            .values()
            .filter(|m| m.state != State::Dead)
            .collect()
    }

    /// What this node believes about a peer, if it knows of it at all.
    #[must_use]
    pub fn state(&self, id: &NodeId) -> Option<State> {
        self.members.get(id).map(|m| m.state)
    }

    /// The eight bytes two nodes exchange to discover they agree.
    ///
    /// ⚠️ O(1). Maintained as members change rather than computed from the set, because a
    /// re-hash per round is the O(N) cost this crate exists to remove — it would move the
    /// work from the network to the CPU and call it a saving.
    #[must_use]
    pub fn checksum(&self) -> u64 {
        self.checksum
    }

    /// The checksum, split by bucket (M34): what a `Digest` carries.
    #[must_use]
    pub fn buckets(&self) -> [u64; BUCKETS] {
        self.buckets
    }

    /// The 256 leaf sums (M43), each bucket's 16 in a row.
    #[must_use]
    pub fn leaves(&self) -> &[u64; LEAVES] {
        &self.leaves
    }

    /// One byte per leaf (M43): the top byte of the leaf's sum times a 64-bit odd constant, so
    /// two views that hold a leaf differently differ in its tag 255 times in 256. What a
    /// `TaggedDigest` carries. ⚠️ An array, not a `Vec`: the answer path computes it per
    /// `TaggedDigest` received and allocates nothing for it.
    #[must_use]
    pub fn leaf_tags(&self) -> [u8; LEAVES] {
        self.leaves.map(tag_of)
    }

    /// The leaf sums when each bucket is split into `per` leaves (M52): the incremental 256 at
    /// `per` 16, and otherwise one pass over the view. ⚠️ Above 16 this allocates, once per
    /// `TaggedDigest` sent or answered: about twice a node a round.
    #[must_use]
    pub fn leaf_sums(&self, per: usize) -> Vec<u64> {
        if per == LEAVES_PER_BUCKET {
            return self.leaves.to_vec();
        }
        let mut sums = vec![0u64; BUCKETS * per];
        for m in self.members.values() {
            if let Some(s) = sums.get_mut(leaf_at(&m.id, per)) {
                *s = s.wrapping_add(m.fingerprint());
            }
        }
        sums
    }

    /// [`Self::leaf_tags`] at `per` leaves a bucket (M52).
    #[must_use]
    pub fn leaf_tags_at(&self, per: usize) -> Vec<u8> {
        self.leaf_sums(per).into_iter().map(tag_of).collect()
    }

    /// The same value, computed from the whole set.
    ///
    /// Exists so a test can hold the incremental path to the definition; nothing on a hot
    /// path calls it.
    #[must_use]
    pub fn checksum_from_scratch(&self) -> u64 {
        self.members
            .values()
            .fold(0u64, |acc, m| acc.wrapping_add(m.fingerprint()))
    }

    /// Learn of a member, or do nothing if it is already known.
    pub fn join(&mut self, id: NodeId, addr: String, zone: String) {
        if self.members.contains_key(&id) {
            return;
        }
        self.insert(Member {
            id,
            addr,
            zone,
            incarnation: 0,
            state: State::Alive,
        });
    }

    /// Insert or replace a member wholesale, from a record that won on incarnation.
    ///
    /// ⚠️ Replaces the **address and zone** too, not only the state. The caller decides the
    /// zone first (M24): `Protocol::apply` never lets an empty one clear a known one, nor an
    /// equal incarnation move one.
    pub fn upsert(&mut self, m: Member) {
        self.insert(m);
    }

    /// Fills a member's **empty** zone, and changes nothing else (M24).
    ///
    /// ⚠️ A peer first learned from a bare probe has an unknown zone -- a `Ping` carries none,
    /// and putting one on every probe would tax the steady state this crate exists to keep at
    /// 74 bytes. Its own record, at the same incarnation, never wins on precedence, so the zone
    /// comes in here: never the state, which would let a stale `Alive` revive a suspect.
    /// Answers whether it filled anything.
    pub fn fill_zone(&mut self, id: &NodeId, zone: &str) -> bool {
        let Some(m) = self.members.get(id) else {
            return false;
        };
        if !m.zone.is_empty() || zone.is_empty() {
            return false;
        }
        let mut next = m.clone();
        zone.clone_into(&mut next.zone);
        self.insert(next);
        true
    }

    /// Mark a peer as having missed a probe.
    ///
    /// ⚠️ A node never suspects itself, whoever asks. Accepting a suspicion of oneself means
    /// leaving the fleet on the word of a peer whose own probe loop may simply have been
    /// slow — OQ-12's failure mode, reached from the other side.
    pub fn suspect(&mut self, id: &NodeId) {
        if *id == self.me {
            return;
        }
        self.transition(id, State::Suspect, |s| s == State::Alive);
    }

    /// Give up on a peer. Remembered rather than removed.
    pub fn declare_dead(&mut self, id: &NodeId) {
        if *id == self.me {
            return;
        }
        self.transition(id, State::Dead, |s| s != State::Dead);
    }

    /// A member's own claim that it is alive, at a stated incarnation.
    ///
    /// ⚠️ Accepted only at a **higher** incarnation than the one held. Equal-or-lower is what
    /// a replayed or delayed packet looks like, and accepting it lets an old message
    /// resurrect a node the fleet has already routed away from.
    pub fn refute(&mut self, id: &NodeId, incarnation: u64) {
        let Some(m) = self.members.get(id) else {
            return;
        };
        if incarnation <= m.incarnation {
            return;
        }
        let mut next = m.clone();
        next.incarnation = incarnation;
        next.state = State::Alive;
        self.insert(next);
    }

    fn transition(&mut self, id: &NodeId, to: State, from_ok: impl Fn(State) -> bool) {
        let Some(m) = self.members.get(id) else {
            return;
        };
        if !from_ok(m.state) {
            return;
        }
        let mut next = m.clone();
        next.state = to;
        self.insert(next);
    }

    /// Insert or replace, keeping the checksum correct by construction.
    ///
    /// ⚠️ The only path that writes `members`. Every mutation goes through here so the
    /// incremental checksum cannot be forgotten at one call site — which is the single way an
    /// O(1) checksum silently becomes wrong, and a wrong checksum means two nodes agree to
    /// stay divergent.
    fn insert(&mut self, m: Member) {
        let old = self.members.get(&m.id).map(Member::fingerprint);
        if let Some(old) = old {
            self.checksum = self.checksum.wrapping_sub(old);
        }
        self.checksum = self.checksum.wrapping_add(m.fingerprint());
        // `bucket_of` is below `BUCKETS` by construction, so this always finds its slot.
        if let Some(slot) = self.buckets.get_mut(bucket_of(&m.id)) {
            *slot = slot
                .wrapping_sub(old.unwrap_or(0))
                .wrapping_add(m.fingerprint());
        }
        if let Some(slot) = self.leaves.get_mut(leaf_of(&m.id)) {
            *slot = slot
                .wrapping_sub(old.unwrap_or(0))
                .wrapping_add(m.fingerprint());
        }
        self.members.insert(m.id, m);
    }
}
