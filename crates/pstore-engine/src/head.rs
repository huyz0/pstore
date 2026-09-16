//! The one mutable object per tenant, and the protocol that changes it.
//!
//! The **tenant** is the CAS unit, not the index (D-42): a tenant's indexes are written by
//! one application and commit together, which at 1M tenants × ~50 indexes is worth ~50× on
//! commit cost. HEAD is small on purpose — every reader fetches it, and a small object
//! makes both the read and the contention window cheap.

use crate::EngineError;
use pstore_blob::{BlobStore, CasError, Key, Precondition};
use pstore_types::{CasTag, Epoch, LaneId, Seq};
use std::collections::BTreeMap;

/// One committed segment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SegmentRef {
    /// Where it lives. Derived, never discovered.
    pub key: String,
    /// Rows it holds.
    pub rows: u32,
}

/// What an index's rows must look like: the two facts the engine already depends on and
/// could not state.
///
/// ⚠️ **Inferred by the first fold, and immutable afterwards.** `api-design.md` says types are
/// inferred by default and an index is created implicitly by its first write, so nothing
/// precedes this — and a change that would need a reindex is refused with the migration path
/// named rather than accepted and half-applied.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IndexSchema {
    /// Components in every vector of the index's dense field.
    pub dims: u32,
    /// The attribute the text index is built over. **Empty means the fold that recorded this
    /// schema saw no text at all**, which is not the same as a field named "".
    pub text_field: String,
}

/// A tenant's committed state.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Head {
    /// Monotonic. Advancing it publishes a new snapshot.
    pub epoch: Epoch,
    /// ⚠️ **The ABA guard.** On a backend whose tag is content-derived — an S3 ETag on a
    /// single-part PUT — two identical HEAD values would carry identical tags, so a writer
    /// holding the first could land against a world that changed and changed back. A nonce
    /// that never repeats makes two HEAD values byte-different even when everything else
    /// about them matches.
    pub nonce: u64,
    /// Per index, the segments that make it up.
    pub indexes: BTreeMap<String, Vec<SegmentRef>>,
    /// How far each lane has been folded, so a reader knows what it still has to replay.
    pub watermarks: BTreeMap<u64, u64>,
    /// Per index, what its rows must look like. ⚠️ **A trailing section**: a HEAD written
    /// before M7d has none, and that is "no schemas recorded", never a schema of zero.
    pub schemas: BTreeMap<String, IndexSchema>,
    /// Per index, rows a fold **discarded** because they contradicted the schema.
    ///
    /// ⚠️ **Acknowledged writes, dropped**, and counted here precisely so that it is visible.
    /// The alternative — failing the fold — was measured in spec review: a fold is
    /// all-or-nothing across every index in the bundle set, so one contradicting row would
    /// stop every later fold for the whole tenant, forever.
    pub schema_rejects: BTreeMap<String, u64>,
    /// Keys that **stopped being referenced** at a given epoch, newest last.
    ///
    /// ⚠️ This is what lets GC run with **zero LIST**. A bucket enumeration would tell us
    /// which objects exist; it would not tell us which ones a reader might still be
    /// holding, and it is priced like a PUT and capped at 1000 keys. Recording the
    /// dereference at the moment it happens turns garbage collection into a manifest diff
    /// — and the epoch it is recorded under is exactly what the retention window is
    /// measured against.
    ///
    /// Bounded, not unbounded: GC prunes an entry in the same pass that reaps it.
    pub graveyard: BTreeMap<u64, Vec<String>>,
}

impl Head {
    /// The key a tenant's HEAD lives at. **Derived from the tenant id alone** — no lookup,
    /// no listing, no catalog on the hot path.
    #[must_use]
    pub fn key(tenant: pstore_types::TenantId) -> Key {
        Key::new(format!("{:04x}/tnt/{}/HEAD", tenant.0 as u16, tenant.0))
    }

    /// Encodes HEAD.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&self.epoch.0.to_le_bytes());
        out.extend_from_slice(&self.nonce.to_le_bytes());
        out.extend_from_slice(&(self.indexes.len() as u32).to_le_bytes());
        for (name, segs) in &self.indexes {
            put_str(&mut out, name);
            out.extend_from_slice(&(segs.len() as u32).to_le_bytes());
            for s in segs {
                put_str(&mut out, &s.key);
                out.extend_from_slice(&s.rows.to_le_bytes());
            }
        }
        out.extend_from_slice(&(self.watermarks.len() as u32).to_le_bytes());
        for (lane, seq) in &self.watermarks {
            out.extend_from_slice(&lane.to_le_bytes());
            out.extend_from_slice(&seq.to_le_bytes());
        }
        out.extend_from_slice(&(self.graveyard.len() as u32).to_le_bytes());
        for (epoch, keys) in &self.graveyard {
            out.extend_from_slice(&epoch.to_le_bytes());
            out.extend_from_slice(&(keys.len() as u32).to_le_bytes());
            for k in keys {
                put_str(&mut out, k);
            }
        }
        // ⚠️ **Trailing, and that is the compatibility mechanism.** Everything above is byte
        // for byte what the pre-M7d encoder wrote, so an old reader gets the whole of what it
        // understands and stops.
        out.extend_from_slice(&(self.schemas.len() as u32).to_le_bytes());
        for (name, schema) in &self.schemas {
            put_str(&mut out, name);
            out.extend_from_slice(&schema.dims.to_le_bytes());
            put_str(&mut out, &schema.text_field);
        }
        out.extend_from_slice(&(self.schema_rejects.len() as u32).to_le_bytes());
        for (name, n) in &self.schema_rejects {
            put_str(&mut out, name);
            out.extend_from_slice(&n.to_le_bytes());
        }
        out
    }

    /// Decodes HEAD, refusing anything malformed.
    pub fn decode(buf: &[u8]) -> Result<Self, EngineError> {
        let mut c = Cur { b: buf, i: 0 };
        let mut h = Self {
            epoch: Epoch(c.u64()?),
            nonce: c.u64()?,
            ..Self::default()
        };
        for _ in 0..c.u32()? {
            let name = c.string()?;
            let n = c.u32()?;
            let mut segs = Vec::with_capacity(n as usize);
            for _ in 0..n {
                segs.push(SegmentRef {
                    key: c.string()?,
                    rows: c.u32()?,
                });
            }
            h.indexes.insert(name, segs);
        }
        for _ in 0..c.u32()? {
            let lane = c.u64()?;
            h.watermarks.insert(lane, c.u64()?);
        }
        for _ in 0..c.u32()? {
            let epoch = c.u64()?;
            let n = c.u32()?;
            let mut keys = Vec::with_capacity(n as usize);
            for _ in 0..n {
                keys.push(c.string()?);
            }
            h.graveyard.insert(epoch, keys);
        }
        // ⚠️ **End of buffer here is "no schemas", not a corrupt HEAD.** Every HEAD in every
        // store predating M7d ends at the graveyard, and reading that as an error makes them
        // all unreadable — M6c's trap one layer up, where absence read as empty would have
        // turned off the text index of every segment written before that section existed.
        //
        // ⚠️ The price, stated rather than discovered: a HEAD truncated exactly at this
        // boundary now decodes as valid instead of `CorruptHead`. That is unavoidable for an
        // optional trailing section, and the section is what keeps every existing HEAD
        // readable.
        if c.at_end() {
            return Ok(h);
        }
        for _ in 0..c.u32()? {
            let name = c.string()?;
            h.schemas.insert(
                name,
                IndexSchema {
                    dims: c.u32()?,
                    text_field: c.string()?,
                },
            );
        }
        if c.at_end() {
            return Ok(h);
        }
        for _ in 0..c.u32()? {
            let name = c.string()?;
            h.schema_rejects.insert(name, c.u64()?);
        }
        Ok(h)
    }

    /// Whether this lane's sequence has already been folded into a committed epoch.
    #[must_use]
    pub fn is_folded(&self, lane: LaneId, seq: Seq) -> bool {
        self.watermarks.get(&lane.0).is_some_and(|w| *w > seq.0)
    }
}

fn put_str(out: &mut Vec<u8>, s: &str) {
    out.extend_from_slice(&(s.len() as u32).to_le_bytes());
    out.extend_from_slice(s.as_bytes());
}

struct Cur<'a> {
    b: &'a [u8],
    i: usize,
}

impl Cur<'_> {
    /// Whether every byte has been consumed. The optional-section boundary test.
    fn at_end(&self) -> bool {
        self.i >= self.b.len()
    }
    fn take(&mut self, n: usize) -> Result<&[u8], EngineError> {
        let end = self.i.checked_add(n).ok_or(EngineError::CorruptHead)?;
        let out = self.b.get(self.i..end).ok_or(EngineError::CorruptHead)?;
        self.i = end;
        Ok(out)
    }
    fn u32(&mut self) -> Result<u32, EngineError> {
        Ok(u32::from_le_bytes(
            self.take(4)?
                .try_into()
                .map_err(|_| EngineError::CorruptHead)?,
        ))
    }
    fn u64(&mut self) -> Result<u64, EngineError> {
        Ok(u64::from_le_bytes(
            self.take(8)?
                .try_into()
                .map_err(|_| EngineError::CorruptHead)?,
        ))
    }
    fn string(&mut self) -> Result<String, EngineError> {
        let n = self.u32()? as usize;
        String::from_utf8(self.take(n)?.to_vec()).map_err(|_| EngineError::CorruptHead)
    }
}

/// HEAD as it was read, with the tag the next write must be conditioned on.
#[derive(Debug, Clone)]
pub struct HeadAt {
    /// The state.
    pub head: Head,
    /// `None` when HEAD does not exist yet, which means the next write is a create.
    pub tag: Option<CasTag>,
}

/// Reads HEAD, or the empty state if the tenant has never committed.
pub(crate) async fn read<S: BlobStore>(
    store: &S,
    tenant: pstore_types::TenantId,
) -> Result<HeadAt, EngineError> {
    let key = Head::key(tenant);
    match store.get_with_tag(&key).await {
        Ok((bytes, tag)) => Ok(HeadAt {
            head: Head::decode(&bytes)?,
            tag: Some(tag),
        }),
        Err(pstore_blob::BlobError::NotFound(_)) => Ok(HeadAt {
            head: Head::default(),
            tag: None,
        }),
        Err(e) => Err(e.into()),
    }
}

/// Publishes a new HEAD, conditioned on exactly the state the caller observed.
///
/// ⚠️ This is the whole fencing mechanism. A caller that was paused, partitioned, or
/// duplicated cannot land, because its write names a version of the world that no longer
/// exists — and the *storage layer* enforces that, not a lock we would have to keep alive.
pub(crate) async fn commit<S: BlobStore>(
    store: &S,
    tenant: pstore_types::TenantId,
    at: &HeadAt,
    next: &Head,
) -> Result<Epoch, EngineError> {
    crate::require_fencing(store)?;
    let pre = match &at.tag {
        Some(t) => Precondition::Match(t.clone()),
        None => Precondition::NotExists,
    };
    match store
        .put_conditional(&Head::key(tenant), next.encode().into(), pre)
        .await
    {
        Ok(_) => Ok(next.epoch),
        Err(CasError::Lost) => Err(EngineError::Lost),
        Err(CasError::Contended) => Err(EngineError::Contended),
        Err(CasError::Io(e)) => Err(EngineError::Blob(e)),
    }
}
