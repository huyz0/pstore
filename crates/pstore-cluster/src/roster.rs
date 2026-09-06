//! The roster: who is in the fleet, seeded from the blob store (D-4).
//!
//! ⚠️ **A cache of gossip, not the source of truth.** A joining node reads it in **one GET**
//! — no LIST, no DNS, no service discovery — and then joins the mesh. The bucket we already
//! depend on is the discovery mechanism, which is what keeps "one stateful dependency" true.
//! A stale roster costs a slower join, never a wrong membership.

use pstore_blob::{BlobStore, CasError, Key, Precondition};
use pstore_types::CasTag;

/// What can go wrong reading or refolding the roster.
#[derive(Debug, thiserror::Error)]
pub enum RosterError {
    /// The blob store failed.
    #[error("blob store: {0}")]
    Blob(String),
    /// The object was not a roster.
    #[error("roster corrupt: {0}")]
    Corrupt(&'static str),
    /// Another node refolded first. **Rebase and retry** — never overwrite.
    #[error("another node refolded first")]
    Lost,
}

/// The fleet, as a sorted set of node ids.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Roster {
    nodes: Vec<String>,
    ring: Vec<(u64, String)>,
}

impl Roster {
    /// The key a cluster's roster lives at. Derived, never discovered.
    #[must_use]
    pub fn key(cluster: &str) -> Key {
        Key::new(format!("{:04x}/clu/ROSTER", fnv(cluster.as_bytes()) as u16))
    }

    /// A roster from node ids.
    ///
    /// Sorted and deduplicated, so two nodes that learned membership in different orders
    /// build the same ring — otherwise they would disagree about who serves what while both
    /// believing they had the same view.
    #[must_use]
    pub fn from_nodes<I: IntoIterator<Item = String>>(nodes: I) -> Self {
        let mut nodes: Vec<String> = nodes.into_iter().collect();
        nodes.sort();
        nodes.dedup();
        let mut ring: Vec<(u64, String)> = nodes
            .iter()
            .map(|n| (crate::placement::ring_position(n), n.clone()))
            .collect();
        ring.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
        Self { nodes, ring }
    }

    /// The node ids, sorted.
    #[must_use]
    pub fn nodes(&self) -> &[String] {
        &self.nodes
    }

    /// The hash ring, sorted by position.
    #[must_use]
    pub fn ring(&self) -> &[(u64, String)] {
        &self.ring
    }

    /// Encodes the roster: one id per line.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        self.nodes.join("\n").into_bytes()
    }

    /// Decodes a roster, refusing anything that is not one.
    pub fn decode(raw: &[u8]) -> Result<Self, RosterError> {
        let text = std::str::from_utf8(raw).map_err(|_| RosterError::Corrupt("not utf-8"))?;
        Ok(Self::from_nodes(
            text.lines().filter(|l| !l.is_empty()).map(str::to_owned),
        ))
    }

    /// Reads the roster. **One GET.**
    ///
    /// A missing roster is an empty fleet, not an error: that is a cold cluster, and the
    /// first node to start must be able to join one.
    pub async fn read<S: BlobStore>(
        store: &S,
        cluster: &str,
    ) -> Result<(Self, Option<CasTag>), RosterError> {
        match store.get_with_tag(&Self::key(cluster)).await {
            Ok((bytes, tag)) => Ok((Self::decode(&bytes)?, Some(tag))),
            Err(pstore_blob::BlobError::NotFound(_)) => Ok((Self::default(), None)),
            Err(e) => Err(RosterError::Blob(e.to_string())),
        }
    }

    /// Refolds the roster from a gossip view, conditioned on the tag that was read.
    ///
    /// ⚠️ **CAS on an observed tag, not create-if-absent.** MinIO ignores the
    /// `If-None-Match: *` wildcard — measured by this project's own conformance suite, not
    /// assumed — so a cold cluster's first roster write is exactly the race that primitive
    /// does not survive there. Conditioning on a tag works on every backend we support.
    ///
    /// Returns `Lost` when another node refolded first. The caller **rebases** — reads the
    /// newer roster and merges — rather than retrying the same bytes, because overwriting
    /// would drop whatever members the winner had just recorded.
    pub async fn refold<S: BlobStore>(
        store: &S,
        cluster: &str,
        view: &Self,
        at: Option<CasTag>,
    ) -> Result<(), RosterError> {
        let pre = match at {
            Some(t) => Precondition::Match(t),
            None => Precondition::NotExists,
        };
        match store
            .put_conditional(&Self::key(cluster), view.encode().into(), pre)
            .await
        {
            Ok(_) => Ok(()),
            Err(CasError::Lost | CasError::Contended) => Err(RosterError::Lost),
            Err(CasError::Io(e)) => Err(RosterError::Blob(e)),
        }
    }

    /// This roster plus `other`'s members.
    ///
    /// Union, never replacement: a refold that lost a CAS must not drop the members the
    /// winner recorded, and gossip is the source of truth for both.
    #[must_use]
    pub fn merged(&self, other: &Self) -> Self {
        Self::from_nodes(self.nodes.iter().chain(&other.nodes).cloned())
    }
}

fn fnv(b: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for x in b {
        h ^= u64::from(*x);
        h = h.wrapping_mul(0x1000_0000_01b3);
    }
    h
}
