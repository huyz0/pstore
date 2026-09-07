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
}

impl Cluster {
    /// A cluster containing only this node, alive.
    #[must_use]
    pub fn new(me: NodeId, addr: String) -> Self {
        let mut c = Self {
            me,
            members: BTreeMap::new(),
            checksum: 0,
        };
        c.insert(Member {
            id: me,
            addr,
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
    pub fn join(&mut self, id: NodeId, addr: String) {
        if self.members.contains_key(&id) {
            return;
        }
        self.insert(Member {
            id,
            addr,
            incarnation: 0,
            state: State::Alive,
        });
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
        if let Some(old) = self.members.get(&m.id) {
            self.checksum = self.checksum.wrapping_sub(old.fingerprint());
        }
        self.checksum = self.checksum.wrapping_add(m.fingerprint());
        self.members.insert(m.id, m);
    }
}
