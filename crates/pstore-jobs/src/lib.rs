//! A sharded register of runnable work, kept in the blob store itself (M22).
//!
//! ```text
//! {spread:04x}/jobs/{name}/CONFIG              create-once · {shards}
//! {spread:04x}/jobs/{name}/{shard:04x}/QUEUE   MUTABLE · CAS · {id -> {gen, claim?}}
//! ```
//!
//! ⚠️ **A register, not a broker** ([`ownership-and-leases.md`](../../../docs/research/04-cluster/ownership-and-leases.md)
//! § Work scheduling, and its M22 banner). It has no address, no master and no liveness
//! protocol. It answers one question nothing else can without a LIST: *which ids have work*.
//! The truth about that work lives elsewhere (for replication, the tenant's HEAD), and every
//! add or remove goes through [`Register::reconcile`], which reads it.
//!
//! **Claims are advisory** (Design rule 12): a claim only suppresses duplicate work. Code
//! that would be wrong if two holders ran one id is a bug in that code, not in this crate.

use bytes::Bytes;
use pstore_blob::{BlobError, BlobStore, CasError, Key, Precondition};
use pstore_types::CasTag;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

/// Commit attempts before a CAS loop gives up.
pub const MAX_ATTEMPTS: u32 = 24;

/// Why an operation on the register failed.
#[derive(Debug, thiserror::Error)]
pub enum JobsError {
    /// The store refused a read.
    #[error("blob store: {0}")]
    Blob(#[from] BlobError),
    /// The store refused a write for a reason other than losing a race.
    #[error("conditional write: {0}")]
    Cas(String),
    /// Every attempt lost its race.
    #[error("contended: {MAX_ATTEMPTS} attempts lost")]
    Contended,
    /// An object did not decode.
    #[error("corrupt {0}")]
    Corrupt(String),
    /// The caller's view of the truth failed.
    #[error("{0}")]
    Want(String),
}

/// An advisory claim on one id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Claim {
    /// Who holds it: a process's lane.
    pub owner: u64,
    /// When it lapses, in the holder's milliseconds. Skew only delays a takeover.
    pub expires_ms: u64,
}

/// One id's entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// A digest of the truth the entry was reconciled against. A holder that sees it change
    /// re-reads that truth. `gen` on the wire.
    #[serde(rename = "gen")]
    pub generation: u64,
    /// Who is working on it, if anyone.
    pub claim: Option<Claim>,
}

/// One shard as read.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Shard {
    /// Bumped by every write, so no two writes leave the same bytes. ⚠️ An S3 ETag is the
    /// content's MD5: a write of identical bytes keeps the tag, and a stale writer holding it
    /// would land (ABA). HEAD carries a nonce for the same reason.
    #[serde(default)]
    pub version: u64,
    /// Every id the shard holds.
    pub entries: BTreeMap<String, Entry>,
}

/// The register `name` in `store`.
#[derive(Debug)]
pub struct Register<S> {
    store: Arc<S>,
    name: String,
    shards: u16,
}

/// FNV-1a 64: stable across builds and machines, which `std`'s hasher is not.
#[must_use]
pub fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, b| {
        (h ^ u64::from(*b)).wrapping_mul(0x0100_0000_01b3)
    })
}

/// `CONFIG`'s body.
#[derive(Debug, Serialize, Deserialize)]
struct Config {
    shards: u16,
}

/// A short, growing pause between attempts. The register's writers are few per shard, so
/// the pause only has to break lockstep, not to schedule.
async fn backoff(attempt: u32) {
    tokio::time::sleep(Duration::from_millis(1 << attempt.min(6))).await;
}

/// The two-hex-byte prefix that spreads `what` over the store's partitions.
fn spread(what: &str) -> u16 {
    (fnv1a(what.as_bytes()) & 0xffff) as u16
}

impl<S: BlobStore> Register<S> {
    /// Opens `name`, creating its `CONFIG` with `default_shards` if it has none. An existing
    /// `CONFIG` wins: the shard count is fixed at first use.
    ///
    /// # Errors
    /// If the store refuses, or `CONFIG` does not decode.
    pub async fn open(store: Arc<S>, name: &str, default_shards: u16) -> Result<Self, JobsError> {
        if default_shards == 0 {
            return Err(JobsError::Corrupt(format!(
                "register {name}: a shard count of 0"
            )));
        }
        let key = Key::new(format!("{:04x}/jobs/{name}/CONFIG", spread(name)));
        let decode = |b: &[u8]| -> Result<u16, JobsError> {
            let c: Config = serde_json::from_slice(b)
                .map_err(|e| JobsError::Corrupt(format!("{}: {e}", key.as_str())))?;
            if c.shards == 0 {
                return Err(JobsError::Corrupt(format!("{}: 0 shards", key.as_str())));
            }
            Ok(c.shards)
        };
        for attempt in 0..MAX_ATTEMPTS {
            match store.get(&key).await {
                Ok(b) => {
                    let shards = decode(&b)?;
                    return Ok(Self {
                        store,
                        name: name.to_owned(),
                        shards,
                    });
                }
                Err(BlobError::NotFound(_)) => {}
                Err(e) => return Err(e.into()),
            }
            let body = serde_json::to_vec(&Config {
                shards: default_shards,
            })
            .map_err(|e| JobsError::Corrupt(e.to_string()))?;
            match store
                .put_conditional(&key, Bytes::from(body), Precondition::NotExists)
                .await
            {
                Ok(_) => {
                    return Ok(Self {
                        store,
                        name: name.to_owned(),
                        shards: default_shards,
                    });
                }
                // Another process created it first: its count wins, read on the next pass.
                Err(CasError::Lost | CasError::Contended) => backoff(attempt).await,
                Err(CasError::Io(e)) => return Err(JobsError::Cas(e)),
            }
        }
        Err(JobsError::Contended)
    }

    /// This register, read and written through `store` instead: the same name and shard
    /// count, so a caller can bill each operation to whoever asked for it. **No request.**
    #[must_use]
    pub fn on<T: BlobStore>(&self, store: Arc<T>) -> Register<T> {
        Register {
            store,
            name: self.name.clone(),
            shards: self.shards,
        }
    }

    /// The shard count.
    #[must_use]
    pub fn shards(&self) -> u16 {
        self.shards
    }

    /// The shard `id` lives in.
    #[must_use]
    pub fn shard_of(&self, id: &str) -> u16 {
        // Below `shards`, which is a u16: the cast cannot truncate.
        (fnv1a(id.as_bytes()) % u64::from(self.shards)) as u16
    }

    /// The key of `shard`.
    #[must_use]
    pub fn shard_key(&self, shard: u16) -> Key {
        let name = &self.name;
        Key::new(format!(
            "{:04x}/jobs/{name}/{shard:04x}/QUEUE",
            spread(&format!("{name}/{shard}"))
        ))
    }

    /// `shard` and the tag to condition its next write on; `None` when it does not exist,
    /// which reads as empty.
    async fn read_tagged(&self, shard: u16) -> Result<(Shard, Option<CasTag>), JobsError> {
        let key = self.shard_key(shard);
        match self.store.get_with_tag(&key).await {
            Ok((b, tag)) => {
                let s = serde_json::from_slice(&b)
                    .map_err(|e| JobsError::Corrupt(format!("{}: {e}", key.as_str())))?;
                Ok((s, Some(tag)))
            }
            Err(BlobError::NotFound(_)) => Ok((Shard::default(), None)),
            Err(e) => Err(e.into()),
        }
    }

    /// Writes `next` over the `shard` read at `tag`. `Ok(false)` when another writer won,
    /// so the caller re-reads. A `Contended` write is retried as it was, without a re-read.
    async fn write(
        &self,
        shard: u16,
        next: &Shard,
        tag: Option<CasTag>,
        attempt: &mut u32,
    ) -> Result<bool, JobsError> {
        let mut next = next.clone();
        next.version = next.version.wrapping_add(1);
        let body =
            Bytes::from(serde_json::to_vec(&next).map_err(|e| JobsError::Corrupt(e.to_string()))?);
        let pre = tag.map_or(Precondition::NotExists, Precondition::Match);
        let key = self.shard_key(shard);
        while *attempt < MAX_ATTEMPTS {
            *attempt += 1;
            match self
                .store
                .put_conditional(&key, body.clone(), pre.clone())
                .await
            {
                Ok(_) => return Ok(true),
                Err(CasError::Lost) => {
                    backoff(*attempt).await;
                    return Ok(false);
                }
                Err(CasError::Contended) => backoff(*attempt).await,
                Err(CasError::Io(e)) => return Err(JobsError::Cas(e)),
            }
        }
        Err(JobsError::Contended)
    }

    /// Reads `shard`, applies `change`, and writes it if it changed, until a write lands.
    /// `change` answers the operation's result.
    async fn update<T>(
        &self,
        shard: u16,
        mut change: impl FnMut(&mut Shard) -> T,
    ) -> Result<T, JobsError> {
        let mut attempt = 0;
        while attempt < MAX_ATTEMPTS {
            let (mut next, tag) = self.read_tagged(shard).await?;
            let before = next.clone();
            let out = change(&mut next);
            if next == before || self.write(shard, &next, tag, &mut attempt).await? {
                return Ok(out);
            }
        }
        Err(JobsError::Contended)
    }

    /// `shard` as it is now.
    ///
    /// # Errors
    /// If the store refuses, or the shard does not decode.
    pub async fn read(&self, shard: u16) -> Result<Shard, JobsError> {
        Ok(self.read_tagged(shard).await?.0)
    }

    /// Claims, for `owner`, every entry of `shard` that is unclaimed or expired, up to `room`
    /// of them, and refreshes `owner`'s own. Returns every entry `owner` now holds there.
    /// **Never adds an entry.**
    ///
    /// # Errors
    /// If the store refuses, or every attempt loses.
    pub async fn claim(
        &self,
        shard: u16,
        owner: u64,
        now_ms: u64,
        ttl_ms: u64,
        room: usize,
    ) -> Result<BTreeMap<String, Entry>, JobsError> {
        let held = Claim {
            owner,
            expires_ms: now_ms.saturating_add(ttl_ms),
        };
        self.update(shard, |s| {
            let mut taken = 0;
            for e in s.entries.values_mut() {
                match e.claim {
                    // Its own first: refreshing what it holds is not taking more.
                    Some(c) if c.owner == owner => e.claim = Some(held),
                    Some(c) if c.expires_ms > now_ms => {}
                    _ if taken < room => {
                        e.claim = Some(held);
                        taken += 1;
                    }
                    _ => {}
                }
            }
            mine(s, owner)
        })
        .await
    }

    /// Extends `owner`'s claims in `shard`, and returns what it still holds: an entry removed,
    /// or taken by another owner, is gone from the answer. **Never adds an entry.**
    ///
    /// # Errors
    /// If the store refuses, or every attempt loses.
    pub async fn renew(
        &self,
        shard: u16,
        owner: u64,
        now_ms: u64,
        ttl_ms: u64,
    ) -> Result<BTreeMap<String, Entry>, JobsError> {
        let held = Claim {
            owner,
            expires_ms: now_ms.saturating_add(ttl_ms),
        };
        self.update(shard, |s| {
            for e in s.entries.values_mut() {
                if e.claim.is_some_and(|c| c.owner == owner) {
                    e.claim = Some(held);
                }
            }
            mine(s, owner)
        })
        .await
    }

    /// Makes `id`'s entry match the truth: read the shard, **then** ask `want`, then write
    /// the shard conditioned on the tag read first. `want` answers `Some(gen)` when `id` has
    /// work and `None` when it has none. A new entry is claimed by `claim_for`, if given.
    /// Returns the entry as written, or `None` if there is none.
    ///
    /// ⚠️ **The order is the argument.** A change to the truth that lands after the shard read
    /// is followed by its own reconcile, whose write changes the shard's tag, so this one's
    /// write loses and repeats with a fresh read of both. The last reconcile to commit read
    /// the latest truth. No grace period, no clock.
    ///
    /// ⚠️ **`touch` is what makes "whose write changes the tag" true.** The reconcile that
    /// follows a change to the truth must write even when the shard already agrees: the
    /// exhaustive test found a pause whose reconcile saw nothing to do and returned, after
    /// which a resume's reconcile -- holding the truth from before the pause -- wrote its
    /// stale entry against an unchanged tag. A reconcile that changed nothing (a worker's
    /// cleanup, a status repair) passes `false` and writes only a difference.
    ///
    /// # Errors
    /// If the store refuses, `want` fails, or every attempt loses.
    pub async fn reconcile<F, Fut>(
        &self,
        id: &str,
        want: F,
        claim_for: Option<Claim>,
        touch: bool,
    ) -> Result<Option<Entry>, JobsError>
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = Result<Option<u64>, JobsError>>,
    {
        let mut want = want;
        let shard = self.shard_of(id);
        let mut attempt = 0;
        while attempt < MAX_ATTEMPTS {
            // ⚠️ The shard first, then the truth: see above.
            let (mut next, tag) = self.read_tagged(shard).await?;
            let wanted = want().await?;
            let now = next.entries.get(id).copied();
            let entry = wanted.map(|generation| Entry {
                generation,
                claim: now.map_or(claim_for, |e| e.claim),
            });
            if entry == now && !touch {
                return Ok(entry);
            }
            match entry {
                Some(e) => next.entries.insert(id.to_owned(), e),
                None => next.entries.remove(id),
            };
            if self.write(shard, &next, tag, &mut attempt).await? {
                return Ok(entry);
            }
        }
        Err(JobsError::Contended)
    }
}

/// The entries of `s` that `owner` holds.
fn mine(s: &Shard, owner: u64) -> BTreeMap<String, Entry> {
    s.entries
        .iter()
        .filter(|(_, e)| e.claim.is_some_and(|c| c.owner == owner))
        .map(|(k, e)| (k.clone(), *e))
        .collect()
}
