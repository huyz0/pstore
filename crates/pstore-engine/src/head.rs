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
    /// Which dictionaries it has (M32), or `None` when HEAD does not say: a HEAD from before
    /// M32, or a ref resurrected for `as_of`. Unknown asks, as every query did before.
    pub dicts: Option<Dicts>,
    /// The segment object's length in bytes (M45), or `None` when HEAD does not say: a HEAD
    /// from before M45 or rewritten by a node from before it, or a ref resurrected for
    /// `as_of`. A known length opens by an absolute range; unknown, by the suffix read that
    /// Azure has no form of (C-14).
    pub len: Option<u64>,
}

/// Which dictionary sidecars a segment has (M32). Fixed when it is sealed, never changed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Dicts {
    /// A sparse dictionary (`.sdict`).
    pub sparse: bool,
    /// A term dictionary (`.tdict`).
    pub text: bool,
    /// Multi-vector encoding clusters (`.fdec`, `.fdev`; M71.2).
    pub fde: bool,
}

impl Dicts {
    /// HEAD's byte for a ref: bit 0 sparse, bit 1 text, bit 2 fde, `0xFF` unknown.
    fn byte(d: Option<Self>) -> u8 {
        // A sum, not `|`: the bits are disjoint, so `|` and `^` agree and a sweep cannot tell
        // them apart (M32's sweep found it); every mutation of this one changes a byte or
        // panics.
        d.map_or(0xFF, |d| {
            u8::from(d.sparse) + 2 * u8::from(d.text) + 4 * u8::from(d.fde)
        })
    }

    fn of(b: u8) -> Result<Option<Self>, EngineError> {
        match b {
            0xFF => Ok(None),
            0..=7 => Ok(Some(Self {
                sparse: b & 1 != 0,
                text: b & 2 != 0,
                fde: b & 4 != 0,
            })),
            _ => Err(EngineError::CorruptHead),
        }
    }
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
    /// The attribute the text index is built over. **Empty means no fold has sealed text
    /// yet**, which is not the same as a field named "": the first that does fills it (M30),
    /// and from then on it never changes. Every seal of the index uses it.
    pub text_field: String,
    /// How its vectors are compared (M9d). `dims` is the **stored** width, which this adds to.
    pub metric: Metric,
    /// How its text becomes terms, and BM25's `k1` and `b` (M14). The default is every index's
    /// before M14.
    pub fts: pstore_format::text::FullText,
    /// The attributes whose trigrams each segment sketches (M15.2), sorted; empty for none.
    pub trigram: Vec<String>,
    /// The **stored** width of each named dense field the index's rows have carried (M98), as
    /// `dims` is `vector`'s, which is never a key. Filled by the fold, never changed.
    pub fields: BTreeMap<String, u32>,
    /// The index's **further** text fields (M152), each with its own analyzer and BM25
    /// parameters; empty for none. Recorded by the first fold, never changed. No name is both
    /// one of these and `text_field`.
    pub texts: BTreeMap<String, pstore_format::text::FullText>,
    /// Each attribute's recorded type (M173): recorded by the fold that first reads one, then
    /// never changed. A write reads its attributes by them.
    pub types: BTreeMap<String, pstore_format::typed::Kind>,
}

/// The most further text fields an index may declare (M152).
pub const MAX_TEXTS: usize = 8;

impl IndexSchema {
    /// The width a client writes and queries: `dims` less what the metric's transform adds.
    #[must_use]
    pub fn client_dims(&self) -> u32 {
        self.dims.saturating_sub(self.metric.extra() as u32)
    }
}

/// How an index's vectors are compared (M9d).
///
/// ⚠️ **Every metric is ranked as a dot product.** The index's rungs maximise `<q, v>`, so the
/// others are transforms applied on the write and on the query: cosine normalizes both sides,
/// and euclidean stores `[v, −‖v‖²/2]` against a query `[q, 1]`, whose dot product
/// `q·v − ‖v‖²/2` ranks as `−‖q − v‖²`. Nothing below the engine learns what a metric is.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Metric {
    /// `−q·v`. pstore's metric before M9d, so every index without one recorded is this.
    #[default]
    DotProduct,
    /// `1 − cos(q, v)`.
    CosineDistance,
    /// `‖q − v‖²`.
    EuclideanSquared,
}

impl Metric {
    /// Its name on the wire, turbopuffer's spelling.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::DotProduct => "dot_product",
            Self::CosineDistance => "cosine_distance",
            Self::EuclideanSquared => "euclidean_squared",
        }
    }

    /// The metric a wire name names.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        [
            Self::DotProduct,
            Self::CosineDistance,
            Self::EuclideanSquared,
        ]
        .into_iter()
        .find(|m| m.name() == name)
    }

    /// Its code in HEAD, and on a row as the reserved `$metric` attribute.
    #[must_use]
    pub fn code(self) -> i64 {
        match self {
            Self::DotProduct => 0,
            Self::CosineDistance => 1,
            Self::EuclideanSquared => 2,
        }
    }

    /// The metric a code names.
    #[must_use]
    pub fn from_code(code: i64) -> Option<Self> {
        [
            Self::DotProduct,
            Self::CosineDistance,
            Self::EuclideanSquared,
        ]
        .into_iter()
        .find(|m| m.code() == code)
    }

    /// Components the stored vector has beyond the client's.
    #[must_use]
    pub fn extra(self) -> usize {
        usize::from(self == Self::EuclideanSquared)
    }
}

/// Why a past epoch cannot be answered.
///
/// ⚠️ **A refusal, not a partial index.** Both arms exist because answering anyway would be
/// worse than not answering: one would report an index missing the segments GC deleted, the
/// other would report the present wearing a date.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum TimeTravel {
    /// The epoch is below the horizon: GC has deleted objects that manifest named.
    #[error("epoch {asked} is below the reap horizon {horizon}: its objects are gone")]
    Reaped {
        /// What the caller asked for.
        asked: u64,
        /// The oldest epoch still reconstructible.
        horizon: u64,
    },
    /// The epoch has not happened.
    #[error("epoch {asked} is ahead of the current epoch {current}")]
    Future {
        /// What the caller asked for.
        asked: u64,
        /// Where the tenant actually is.
        current: u64,
    },
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
    /// The oldest epoch that can still be reconstructed, because `gc` has deleted everything
    /// buried at or below it. ⚠️ A third optional trailing section: absent means **zero**, so
    /// every HEAD written before M7e still decodes and still time-travels to its beginning.
    pub reaped_before: u64,
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
    /// Per segment key, its current **delete vector**: `(the vector's key, rows it deletes)`
    /// (M9c). ⚠️ A fourth optional trailing section: absent means no segment has deleted rows.
    /// A replaced vector is buried, so GC reaps it and `as_of` can still find it.
    pub deletes: BTreeMap<String, (String, u32)>,
    /// Indexes dropped within GC's window (M9f.2): `(name, the epoch that dropped it, its
    /// schema)`. `as_of` an epoch before the drop scores by that schema's metric -- the present
    /// has none, or a recreated index's. Pruned when GC's horizon reaches the epoch.
    pub dropped: Vec<(String, u64, IndexSchema)>,
    /// Each branch (M16), by the epoch it was made at: below it, the index did not exist.
    /// Pruned when GC's horizon reaches it, as `dropped` is.
    pub branched: BTreeMap<String, u64>,
    /// Each replica index (M22), by name: what it follows and how far. ⚠️ A trailing section
    /// after `branched`: absent means none.
    pub replications: BTreeMap<String, Replication>,
    /// Rows a fold rejected (M25), set aside rather than dropped: per index, each quarantine
    /// object's key and its row count. Live to every burial and to GC until an operator
    /// discards it or its index is dropped. ⚠️ A trailing section after `replications`.
    pub quarantine: BTreeMap<String, Vec<(String, u32)>>,
    /// Each external index (M86), by name. ⚠️ The last trailing section, after the segment
    /// lengths: absent means none.
    pub externals: BTreeMap<String, ExternalRef>,
    /// The last [`crate::OUTCOMES_KEPT`] commits that tallied an applied request's outcome
    /// (M129), oldest first: `(epoch, outcome object key)`. A pair that falls off is buried.
    /// ⚠️ A trailing section after the widths: absent means none.
    pub outcomes: Vec<(u64, String)>,
    /// M132: every commit above this epoch that tallied an applied outcome is in
    /// [`Self::outcomes`]; `None` means nothing is known before this HEAD. Initialised by an
    /// engine set for idempotency keys, raised by every fold that drops a listed pair. ⚠️ The
    /// last trailing section, after the outcomes: absent means `None`.
    pub keys_since: Option<u64>,
    /// Per index, `(created_ms, written_ms)` in Unix milliseconds (M148): when it came into
    /// being, 0 when unknown (made before M148), and the last commit that wrote it. ⚠️ A
    /// trailing section after `keys_since`, and `encode` writes only the entries of indexes
    /// that exist, so it never outlives its index.
    pub times: BTreeMap<String, (u64, u64)>,
}

/// Where a replica's source index lives (M22).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplicaSource {
    /// The named remote store, or empty for this process's own.
    pub store: String,
    /// The source's tenant.
    pub tenant: pstore_types::TenantId,
    /// The source index.
    pub index: String,
}

impl ReplicaSource {
    /// `index` of `tenant` in this process's own store.
    #[must_use]
    pub fn local(tenant: pstore_types::TenantId, index: &str) -> Self {
        Self {
            store: String::new(),
            tenant,
            index: index.to_owned(),
        }
    }
}

/// One replica index's replication (M22).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Replication {
    /// What it follows.
    pub source: ReplicaSource,
    /// Whether it is followed now; `false` is paused.
    pub running: bool,
    /// The epoch that last set it running: a resume is a new run.
    pub run: u64,
    /// The source's epoch and fingerprint at the last sync that committed.
    pub applied: Option<(u64, u64)>,
    /// Rows written to the replica that a fold dropped (they slipped past a stale door).
    pub rejected: u64,
}

/// What an external index's source object is (M86).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExternalKind {
    /// A Parquet file's `list<float>`/`FixedSizeList<float>` column.
    Parquet,
    /// A Lance dataset's vector column; `source` is the dataset's directory.
    Lance,
}

/// One external index (M86): a customer's column, and the codes pstore built over it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalRef {
    /// How to read `source`.
    pub kind: ExternalKind,
    /// The customer's object (Parquet) or dataset directory (Lance).
    pub source: String,
    /// The column in it.
    pub column: String,
    /// The codes blob pstore wrote: pstore's own key, derived, not discovered.
    pub codes: String,
    /// Rows the column held when pinned.
    pub rows: u64,
    /// The source's length when pinned. (Its CAS tag is in the codes blob, which is where a
    /// search compares it.)
    pub source_len: u64,
}

/// FNV-1a 64: stable across builds and machines, which `std`'s hasher is not (M22).
#[must_use]
pub(crate) fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, b| {
        (h ^ u64::from(*b)).wrapping_mul(0x0100_0000_01b3)
    })
}

/// Where a segment's delete vector written at `epoch` by `lane` lives (M9c): the segment's
/// own key, then the epoch and the writer's lane.
///
/// ⚠️ **With the lane** (spec review of M9c): two folders against the same HEAD both write a
/// vector for the same segment at the same epoch, with different contents, so their keys
/// differ by lane. ⚠️ **Corrected by M23:** the lane is not enough -- two processes on one lane
/// reach the same key -- and "create-if-absent is not honoured everywhere", the reason this
/// once gave for not using it, stopped being true when `require_fencing` began refusing any
/// backend that does not honour it. A vector is now created, never replaced, and a refused name
/// takes the next (`Engine::create_object`).
///
/// ⚠️ It keeps the segment's `/seg/` path, and that is safe only because [`key_index`] reads a
/// key as a segment **only when it ends in `.seg`** — without that, `as_of` would resurrect a
/// buried delete vector as a segment of its index.
#[must_use]
pub(crate) fn dv_key(segment: &str, epoch: u64, lane: u64) -> String {
    format!("{segment}.{epoch:020}-{lane:016x}.dv")
}

/// The segment and epoch a delete-vector key names, or `None` for any other key.
/// The key `index`'s delete vector for `segment` is recorded under (M16): the segment's own
/// key when the index owns it -- the index its path names -- and a scoped key when it borrows
/// it from a branch's source. Every read, insert and removal of `Head::deletes` goes through
/// this, so two indexes sharing a segment never share its deletes.
#[must_use]
pub(crate) fn dv_ref(index: &str, segment: &str) -> String {
    if key_index(segment) == Some(index) {
        segment.to_owned()
    } else {
        scoped(index, segment)
    }
}

/// `segment` as `index` borrows it: the key followed by `.br-` and the index name in hex, so
/// it holds no dot of its own and `dv_of` still parses a vector key built on it.
#[must_use]
pub(crate) fn scoped(index: &str, segment: &str) -> String {
    // One buffer, not a `String` a byte (M119): this runs per borrowed segment per query.
    use std::fmt::Write as _;
    let mut out = String::with_capacity(segment.len() + 4 + 2 * index.len());
    out.push_str(segment);
    out.push_str(".br-");
    for b in index.bytes() {
        let _ = write!(out, "{b:02x}");
    }
    out
}

/// The segment and index a scoped key names, or `None` for any other key (M16). As a
/// graveyard entry it is a **marker**: `index` stopped naming a borrowed `segment`.
///
/// A delete vector's key built on a scoped key is `None` too: its tail after `.br-` carries
/// the vector's `.{epoch}-{lane}.dv`, which is not hex.
#[must_use]
pub(crate) fn unscoped(key: &str) -> Option<(&str, String)> {
    let (segment, hex) = key.rsplit_once(".br-")?;
    if !segment.ends_with(".seg") || hex.len() % 2 != 0 {
        return None;
    }
    let bytes: Option<Vec<u8>> = (0..hex.len())
        .step_by(2)
        .map(|i| {
            hex.get(i..i + 2)
                .and_then(|h| u8::from_str_radix(h, 16).ok())
        })
        .collect();
    Some((segment, String::from_utf8(bytes?).ok()?))
}

/// What `index` buries when it stops naming `segment` (M16): the key when it owns it, and a
/// marker when it borrowed it -- which names no object, and which GC reads as a burial of
/// the segment.
#[must_use]
pub(crate) fn burial(index: &str, segment: &str) -> String {
    dv_ref(index, segment)
}

pub(crate) fn dv_of(key: &str) -> Option<(&str, u64)> {
    let (segment, stamp) = key.strip_suffix(".dv")?.rsplit_once('.')?;
    let (epoch, _lane) = stamp.split_once('-')?;
    Some((segment, epoch.parse().ok()?))
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
        out.extend_from_slice(&self.reaped_before.to_le_bytes());
        out.extend_from_slice(&(self.deletes.len() as u32).to_le_bytes());
        for (segment, (key, rows)) in &self.deletes {
            put_str(&mut out, segment);
            put_str(&mut out, key);
            out.extend_from_slice(&rows.to_le_bytes());
        }
        // M9d: each schema's metric, when it is not the default every older index has.
        let metrics: Vec<(&String, Metric)> = self
            .schemas
            .iter()
            .filter(|(_, s)| s.metric != Metric::DotProduct)
            .map(|(name, s)| (name, s.metric))
            .collect();
        out.extend_from_slice(&(metrics.len() as u32).to_le_bytes());
        for (name, metric) in metrics {
            put_str(&mut out, name);
            out.push(metric.code() as u8);
        }
        // M9f.2: the dropped indexes' schemas, whole (a live schema's metric is in the section
        // above; a dropped one has no entry there to hang it on).
        out.extend_from_slice(&(self.dropped.len() as u32).to_le_bytes());
        for (name, epoch, schema) in &self.dropped {
            put_str(&mut out, name);
            out.extend_from_slice(&epoch.to_le_bytes());
            out.extend_from_slice(&schema.dims.to_le_bytes());
            put_str(&mut out, &schema.text_field);
            out.push(schema.metric.code() as u8);
        }
        // M14: each schema's full-text schema, live then dropped (by position), when it is
        // not the default every older index has -- so a HEAD of default schemas is byte for
        // byte what the encoder before M14 wrote.
        let all: Vec<(u8, u32, &IndexSchema, Option<&String>)> = self
            .schemas
            .iter()
            .map(|(name, s)| (0u8, 0u32, s, Some(name)))
            .chain(
                self.dropped
                    .iter()
                    .enumerate()
                    .map(|(i, (_, _, s))| (1u8, i as u32, s, None)),
            )
            .collect();
        let put_ref = |out: &mut Vec<u8>, kind: u8, at: u32, name: Option<&String>| {
            out.push(kind);
            match name {
                Some(n) => put_str(out, n),
                None => out.extend_from_slice(&at.to_le_bytes()),
            }
        };
        let fts: Vec<_> = all
            .iter()
            .filter(|(_, _, s, _)| s.fts != pstore_format::text::FullText::default())
            .collect();
        let trigram: Vec<_> = all
            .iter()
            .filter(|(_, _, s, _)| !s.trigram.is_empty())
            .collect();
        // ⚠️ M15.2: an earlier optional section is written, with a count of 0, whenever a later
        // one is -- otherwise the later count would be read as the earlier one's.
        // M32: the dictionaries are the last section, written whenever any ref's are known.
        // M45: the lengths follow the dictionaries, and force them, by the same rule.
        // M86: the externals follow, and force the lengths (and so the dictionaries).
        // M98: the named fields' widths are last, and force the externals.
        let widths: Vec<_> = all
            .iter()
            .filter(|(_, _, s, _)| !s.fields.is_empty())
            .collect();
        // M152: the further text fields follow `keys_since` and force it, though it has no
        // presence marker: a forced `None` is written as `u64::MAX`, which no epoch reaches.
        let texts: Vec<_> = all
            .iter()
            .filter(|(_, _, s, _)| !s.texts.is_empty())
            .collect();
        debug_assert!(
            self.keys_since != Some(u64::MAX),
            "keys_since is an epoch, and u64::MAX means none"
        );
        // M129: the applied outcomes follow, and force the widths.
        // M132: `keys_since` follows, and forces the outcomes.
        // M173: the recorded types follow the text fields, and force their section (empty if
        // none), which forces `keys_since`.
        // M148: the times are last, only those of indexes that exist, and force the types'
        // count (0 when none), and so everything before it.
        let typed: Vec<_> = all
            .iter()
            .filter(|(_, _, s, _)| !s.types.is_empty())
            .collect();
        let times: Vec<_> = self
            .times
            .iter()
            .filter(|(n, _)| {
                self.indexes.contains_key(*n)
                    || self.schema_rejects.contains_key(*n)
                    || self.quarantine.contains_key(*n)
            })
            .collect();
        let types = !typed.is_empty() || !times.is_empty();
        let texted = !texts.is_empty() || types;
        let since = self.keys_since.is_some() || texted;
        let outs = !self.outcomes.is_empty() || since;
        let wide = !widths.is_empty() || outs;
        let ext = wide || !self.externals.is_empty();
        let lens = ext || self.indexes.values().flatten().any(|r| r.len.is_some());
        let dicts = lens || self.indexes.values().flatten().any(|r| r.dicts.is_some());
        let more = !self.replications.is_empty() || !self.quarantine.is_empty() || dicts;
        if !fts.is_empty() || !trigram.is_empty() || !self.branched.is_empty() || more {
            out.extend_from_slice(&(fts.len() as u32).to_le_bytes());
            for (kind, at, s, name) in fts {
                put_ref(&mut out, *kind, *at, *name);
                put_str(&mut out, &s.fts.encode());
            }
        }
        // M16: and the trigram count before the branches, by the same rule.
        if !trigram.is_empty() || !self.branched.is_empty() || more {
            out.extend_from_slice(&(trigram.len() as u32).to_le_bytes());
            for (kind, at, s, name) in trigram {
                put_ref(&mut out, *kind, *at, *name);
                out.extend_from_slice(&(s.trigram.len() as u32).to_le_bytes());
                for n in &s.trigram {
                    put_str(&mut out, n);
                }
            }
        }
        if !self.branched.is_empty() || more {
            out.extend_from_slice(&(self.branched.len() as u32).to_le_bytes());
            for (name, epoch) in &self.branched {
                put_str(&mut out, name);
                out.extend_from_slice(&epoch.to_le_bytes());
            }
        }
        // M22: the replications; M25's quarantine after them, by the same rule.
        if more {
            out.extend_from_slice(&(self.replications.len() as u32).to_le_bytes());
            for (name, r) in &self.replications {
                put_str(&mut out, name);
                put_str(&mut out, &r.source.store);
                out.extend_from_slice(&r.source.tenant.0.to_le_bytes());
                put_str(&mut out, &r.source.index);
                out.push(u8::from(r.running) | (u8::from(r.applied.is_some()) << 1));
                out.extend_from_slice(&r.run.to_le_bytes());
                if let Some((epoch, fp)) = r.applied {
                    out.extend_from_slice(&epoch.to_le_bytes());
                    out.extend_from_slice(&fp.to_le_bytes());
                }
                out.extend_from_slice(&r.rejected.to_le_bytes());
            }
        }
        if !self.quarantine.is_empty() || dicts {
            out.extend_from_slice(&(self.quarantine.len() as u32).to_le_bytes());
            for (name, objects) in &self.quarantine {
                put_str(&mut out, name);
                out.extend_from_slice(&(objects.len() as u32).to_le_bytes());
                for (key, rows) in objects {
                    put_str(&mut out, key);
                    out.extend_from_slice(&rows.to_le_bytes());
                }
            }
        }
        // ⚠️ M32: one byte per segment, in the index's order -- not a key per segment, which
        // would grow every HEAD every query reads by ~80 bytes a segment.
        if dicts {
            out.extend_from_slice(&(self.indexes.len() as u32).to_le_bytes());
            for (name, refs) in &self.indexes {
                put_str(&mut out, name);
                out.extend_from_slice(&(refs.len() as u32).to_le_bytes());
                out.extend(refs.iter().map(|r| Dicts::byte(r.dicts)));
            }
        }
        // ⚠️ M45: a varint per segment, 0 for unknown -- three bytes for a megabyte segment,
        // where a fixed `u64` would be eight on every HEAD every query reads.
        if lens {
            out.extend_from_slice(&(self.indexes.len() as u32).to_le_bytes());
            for (name, refs) in &self.indexes {
                put_str(&mut out, name);
                out.extend_from_slice(&(refs.len() as u32).to_le_bytes());
                for r in refs {
                    put_varint(&mut out, r.len.unwrap_or(0));
                }
            }
        }
        if ext {
            out.extend_from_slice(&(self.externals.len() as u32).to_le_bytes());
            for (name, e) in &self.externals {
                put_str(&mut out, name);
                out.push(match e.kind {
                    ExternalKind::Parquet => 0,
                    ExternalKind::Lance => 1,
                });
                put_str(&mut out, &e.source);
                put_str(&mut out, &e.column);
                put_str(&mut out, &e.codes);
                out.extend_from_slice(&e.rows.to_le_bytes());
                out.extend_from_slice(&e.source_len.to_le_bytes());
            }
        }
        if wide {
            out.extend_from_slice(&(widths.len() as u32).to_le_bytes());
            for (kind, at, s, name) in widths {
                put_ref(&mut out, *kind, *at, *name);
                out.extend_from_slice(&(s.fields.len() as u32).to_le_bytes());
                for (field, width) in &s.fields {
                    put_str(&mut out, field);
                    out.extend_from_slice(&width.to_le_bytes());
                }
            }
        }
        if outs {
            out.extend_from_slice(&(self.outcomes.len() as u32).to_le_bytes());
            for (epoch, key) in &self.outcomes {
                out.extend_from_slice(&epoch.to_le_bytes());
                put_str(&mut out, key);
            }
        }
        if since {
            out.extend_from_slice(&self.keys_since.unwrap_or(u64::MAX).to_le_bytes());
        }
        if texted {
            out.extend_from_slice(&(texts.len() as u32).to_le_bytes());
            for (kind, at, s, name) in texts {
                put_ref(&mut out, *kind, *at, *name);
                out.extend_from_slice(&(s.texts.len() as u32).to_le_bytes());
                for (field, fts) in &s.texts {
                    put_str(&mut out, field);
                    put_str(&mut out, &fts.encode());
                }
            }
        }
        if types {
            out.extend_from_slice(&(typed.len() as u32).to_le_bytes());
            for (kind, at, s, name) in typed {
                put_ref(&mut out, *kind, *at, *name);
                out.extend_from_slice(&(s.types.len() as u32).to_le_bytes());
                for (attr, k) in &s.types {
                    put_str(&mut out, attr);
                    out.push(kind_code(*k));
                }
            }
        }
        if !times.is_empty() {
            out.extend_from_slice(&(times.len() as u32).to_le_bytes());
            for (name, (created, written)) in times {
                put_str(&mut out, name);
                out.extend_from_slice(&created.to_le_bytes());
                out.extend_from_slice(&written.to_le_bytes());
            }
        }
        out
    }

    /// A digest of the running replications' `(name, run)` set, or `None` when none runs
    /// (M22): what a holder of the tenant's register entry compares to know it must re-read.
    #[must_use]
    pub fn replication_gen(&self) -> Option<u64> {
        let mut bytes = Vec::new();
        for (name, r) in self.replications.iter().filter(|(_, r)| r.running) {
            bytes.extend_from_slice(name.as_bytes());
            bytes.push(0);
            bytes.extend_from_slice(&r.run.to_le_bytes());
        }
        (!bytes.is_empty()).then(|| fnv1a(&bytes))
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
                    dicts: None,
                    len: None,
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
                    metric: Metric::DotProduct,
                    fts: pstore_format::text::FullText::default(),
                    trigram: Vec::new(),
                    fields: Default::default(),
                    types: Default::default(),
                    texts: BTreeMap::new(),
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
        if c.at_end() {
            return Ok(h);
        }
        // ⚠️ A bare `u64` with no count in front, so a HEAD truncated one to seven bytes into
        // it is `CorruptHead` rather than a horizon of zero — `take` gives that for free, and
        // a horizon read short is a horizon that refuses nothing.
        h.reaped_before = c.u64()?;
        if c.at_end() {
            return Ok(h);
        }
        for _ in 0..c.u32()? {
            let segment = c.string()?;
            let key = c.string()?;
            h.deletes.insert(segment, (key, c.u32()?));
        }
        if c.at_end() {
            return Ok(h);
        }
        for _ in 0..c.u32()? {
            let name = c.string()?;
            let metric = Metric::from_code(i64::from(c.u8()?)).ok_or(EngineError::CorruptHead)?;
            // A metric belongs to a schema; one naming no schema is not a HEAD this wrote.
            h.schemas
                .get_mut(&name)
                .ok_or(EngineError::CorruptHead)?
                .metric = metric;
        }
        if c.at_end() {
            return Ok(h);
        }
        for _ in 0..c.u32()? {
            let name = c.string()?;
            let epoch = c.u64()?;
            let dims = c.u32()?;
            let text_field = c.string()?;
            let metric = Metric::from_code(i64::from(c.u8()?)).ok_or(EngineError::CorruptHead)?;
            h.dropped.push((
                name,
                epoch,
                IndexSchema {
                    dims,
                    text_field,
                    metric,
                    fts: pstore_format::text::FullText::default(),
                    trigram: Vec::new(),
                    fields: Default::default(),
                    types: Default::default(),
                    texts: BTreeMap::new(),
                },
            ));
        }
        if c.at_end() {
            return Ok(h);
        }
        for _ in 0..c.u32()? {
            let kind = c.u8()?;
            let schema = match kind {
                0 => {
                    let name = c.string()?;
                    h.schemas.get_mut(&name)
                }
                1 => {
                    let at = c.u32()? as usize;
                    h.dropped.get_mut(at).map(|(_, _, s)| s)
                }
                _ => None,
            }
            // Full text belongs to a schema; one naming none is not a HEAD this wrote.
            .ok_or(EngineError::CorruptHead)?;
            schema.fts = pstore_format::text::FullText::decode(&c.string()?)
                .ok_or(EngineError::CorruptHead)?;
        }
        // M15.2: each schema's trigram set, addressed as the full-text section addresses it.
        if c.at_end() {
            return Ok(h);
        }
        for _ in 0..c.u32()? {
            let kind = c.u8()?;
            let schema = match kind {
                0 => {
                    let name = c.string()?;
                    h.schemas.get_mut(&name)
                }
                1 => {
                    let at = c.u32()? as usize;
                    h.dropped.get_mut(at).map(|(_, _, s)| s)
                }
                _ => None,
            }
            .ok_or(EngineError::CorruptHead)?;
            let n = c.u32()?;
            let mut names = Vec::with_capacity((n as usize).min(64));
            for _ in 0..n {
                names.push(c.string()?);
            }
            schema.trigram = names;
        }
        if c.at_end() {
            return Ok(h);
        }
        for _ in 0..c.u32()? {
            let name = c.string()?;
            h.branched.insert(name, c.u64()?);
        }
        if c.at_end() {
            return Ok(h);
        }
        for _ in 0..c.u32()? {
            let name = c.string()?;
            let store = c.string()?;
            let tenant = u128::from(c.u64()?) | (u128::from(c.u64()?) << 64);
            let index = c.string()?;
            let flags = c.u8()?;
            let run = c.u64()?;
            let applied = if flags & 2 != 0 {
                Some((c.u64()?, c.u64()?))
            } else {
                None
            };
            let rejected = c.u64()?;
            h.replications.insert(
                name,
                Replication {
                    source: ReplicaSource {
                        store,
                        tenant: pstore_types::TenantId(tenant),
                        index,
                    },
                    running: flags & 1 != 0,
                    run,
                    applied,
                    rejected,
                },
            );
        }
        if c.at_end() {
            return Ok(h);
        }
        for _ in 0..c.u32()? {
            let name = c.string()?;
            let mut objects = Vec::new();
            for _ in 0..c.u32()? {
                let key = c.string()?;
                objects.push((key, c.u32()?));
            }
            h.quarantine.insert(name, objects);
        }
        // ⚠️ End of buffer here is "no dictionaries known" (M32): every HEAD before it ends at
        // the quarantine, and a decoder from before it stops there too and re-encodes without
        // this section -- every ref then asks, as it did.
        if c.at_end() {
            return Ok(h);
        }
        for _ in 0..c.u32()? {
            let name = c.string()?;
            let n = c.u32()? as usize;
            // A section that names an index HEAD does not, or disagrees with its segment
            // count, is not one this HEAD wrote: refused rather than misapplied.
            let refs = h.indexes.get_mut(&name).ok_or(EngineError::CorruptHead)?;
            if refs.len() != n {
                return Err(EngineError::CorruptHead);
            }
            for r in refs.iter_mut() {
                r.dicts = Dicts::of(c.u8()?)?;
            }
        }
        // ⚠️ M45: end of buffer is "no length known", as above. A decoder from before M45
        // returns before this section, ignoring it, and re-encodes without it.
        if c.at_end() {
            return Ok(h);
        }
        for _ in 0..c.u32()? {
            let name = c.string()?;
            let n = c.u32()? as usize;
            let refs = h.indexes.get_mut(&name).ok_or(EngineError::CorruptHead)?;
            if refs.len() != n {
                return Err(EngineError::CorruptHead);
            }
            for r in refs.iter_mut() {
                r.len = Some(c.varint()?).filter(|l| *l != 0);
            }
        }
        // ⚠️ M86: end of buffer here is "no external indexes".
        if c.at_end() {
            return Ok(h);
        }
        for _ in 0..c.u32()? {
            let name = c.string()?;
            let kind = match c.u8()? {
                0 => ExternalKind::Parquet,
                1 => ExternalKind::Lance,
                _ => return Err(EngineError::CorruptHead),
            };
            let e = ExternalRef {
                kind,
                source: c.string()?,
                column: c.string()?,
                codes: c.string()?,
                rows: c.u64()?,
                source_len: c.u64()?,
            };
            h.externals.insert(name, e);
        }
        // ⚠️ M98: end of buffer here is "no named field's width known", which the next fold
        // learns from its own rows.
        if c.at_end() {
            return Ok(h);
        }
        for _ in 0..c.u32()? {
            let kind = c.u8()?;
            let schema = match kind {
                0 => {
                    let name = c.string()?;
                    h.schemas.get_mut(&name)
                }
                1 => {
                    let at = c.u32()? as usize;
                    h.dropped.get_mut(at).map(|(_, _, s)| s)
                }
                _ => None,
            }
            .ok_or(EngineError::CorruptHead)?;
            for _ in 0..c.u32()? {
                let field = c.string()?;
                // `vector`'s width is `dims`, and a derived encoding's (M67) its parameters':
                // a record of either is not a HEAD this wrote.
                if field == pstore_format::DEFAULT_FIELD
                    || field.ends_with(pstore_format::fde::FIELD_SUFFIX)
                {
                    return Err(EngineError::CorruptHead);
                }
                schema.fields.insert(field, c.u32()?);
            }
        }
        // ⚠️ M129: end of buffer here is "no applied outcome listed".
        if c.at_end() {
            return Ok(h);
        }
        for _ in 0..c.u32()? {
            let epoch = c.u64()?;
            h.outcomes.push((epoch, c.string()?));
        }
        // M132: and here, no `keys_since`. ⚠️ M131 stopped here without checking for the end,
        // so it reads a HEAD that has one (and drops it on re-commit).
        if c.at_end() {
            return Ok(h);
        }
        // M152: `u64::MAX` is a forced `None`, wherever it appears.
        h.keys_since = Some(c.u64()?).filter(|s| *s != u64::MAX);
        // ⚠️ M152: end of buffer here is "no further text fields". A decoder from before it
        // refuses anything after `keys_since`, so every node reads this before any writes it.
        if c.at_end() {
            return Ok(h);
        }
        for _ in 0..c.u32()? {
            let kind = c.u8()?;
            let schema = match kind {
                0 => {
                    let name = c.string()?;
                    h.schemas.get_mut(&name)
                }
                1 => {
                    let at = c.u32()? as usize;
                    h.dropped.get_mut(at).map(|(_, _, s)| s)
                }
                _ => None,
            }
            .ok_or(EngineError::CorruptHead)?;
            let n = c.u32()? as usize;
            if n > MAX_TEXTS {
                return Err(EngineError::CorruptHead);
            }
            for _ in 0..n {
                let field = c.string()?;
                // No HEAD this wrote names a field it could not have been declared with, or one
                // that is also the primary.
                if field.is_empty()
                    || field.starts_with('$')
                    || field == schema.text_field
                    || schema.texts.contains_key(&field)
                {
                    return Err(EngineError::CorruptHead);
                }
                let fts = pstore_format::text::FullText::decode(&c.string()?)
                    .ok_or(EngineError::CorruptHead)?;
                schema.texts.insert(field, fts);
            }
        }
        // M173: end of buffer here is "no recorded types"; they follow the text fields, which
        // they force.
        if c.at_end() {
            return Ok(h);
        }
        for _ in 0..c.u32()? {
            let kind = c.u8()?;
            let schema = match kind {
                0 => {
                    let name = c.string()?;
                    h.schemas.get_mut(&name)
                }
                1 => {
                    let at = c.u32()? as usize;
                    h.dropped.get_mut(at).map(|(_, _, s)| s)
                }
                _ => None,
            }
            .ok_or(EngineError::CorruptHead)?;
            for _ in 0..c.u32()? {
                let attr = c.string()?;
                let k = pstore_format::typed::Kind::ALL
                    .get(c.u8()? as usize)
                    .copied()
                    .ok_or(EngineError::CorruptHead)?;
                schema.types.insert(attr, k);
            }
        }
        // M148: and here, no times. A decoder from before it refuses anything after the
        // recorded types, so every node reads this before any writes it.
        if c.at_end() {
            return Ok(h);
        }
        for _ in 0..c.u32()? {
            let name = c.string()?;
            let created = c.u64()?;
            h.times.insert(name, (created, c.u64()?));
        }
        if !c.at_end() {
            return Err(EngineError::CorruptHead);
        }
        Ok(h)
    }

    /// Records that GC has deleted everything buried at or below `horizon`.
    ///
    /// ⚠️ **Never lowers it.** `gc`'s horizon is derived from whatever retention its caller
    /// passed, so a wider retention computes a smaller number; letting it land would reopen a
    /// window whose objects are already deleted. The sequence cannot occur through `gc` today
    /// — a pass with nothing due returns before it commits — which is exactly why this is a
    /// function with its own test rather than a line inside `gc` that nothing can reach.
    pub fn record_reap(&mut self, horizon: u64) {
        // A dropped schema outlives its index until nothing before the drop is answerable
        // (M9f.2): `as_of` refuses every epoch below the horizon, so at `horizon >= epoch` no
        // query can ask for it.
        self.dropped
            .retain(|(_, epoch, _)| *epoch > horizon.max(self.reaped_before));
        // M16: below the horizon nothing is answerable, so "absent before" says nothing.
        let floor = horizon.max(self.reaped_before);
        self.branched.retain(|_, epoch| *epoch > floor);
        self.reaped_before = self.reaped_before.max(horizon);
    }

    /// This manifest as it stood at `epoch`.
    ///
    /// ⚠️ **Reconstruction, not an archive.** A segment's key carries the epoch it became live
    /// at, and the graveyard records the epoch each key stopped being referenced at, so the
    /// manifest of any past epoch is arithmetic over the current one: **zero extra writes on
    /// the commit path, zero extra reads here.**
    ///
    /// ⚠️ Exact in `indexes` and **nothing else**. The graveyard records keys, not row counts,
    /// so a resurrected `SegmentRef` carries `rows: 0`; `schemas`, `schema_rejects` and the
    /// watermarks are the present's. A query reads none of them — the segment footer has the
    /// truth — which is why this is query-only.
    ///
    /// # Errors
    /// [`TimeTravel::Reaped`] below the horizon, [`TimeTravel::Future`] above the present.
    pub fn as_of(&self, epoch: Epoch) -> Result<Self, TimeTravel> {
        if epoch.0 > self.epoch.0 {
            return Err(TimeTravel::Future {
                asked: epoch.0,
                current: self.epoch.0,
            });
        }
        if epoch.0 < self.reaped_before {
            return Err(TimeTravel::Reaped {
                asked: epoch.0,
                horizon: self.reaped_before,
            });
        }
        let mut out = self.clone();
        out.epoch = epoch;
        // Live segments born at or before the epoch.
        for refs in out.indexes.values_mut() {
            refs.retain(|r| key_epoch(&r.key).is_some_and(|born| born <= epoch.0));
        }
        // ⚠️ Plus what was alive then and has since been buried — **buried strictly after the
        // epoch**, because a key buried AT `E` was already gone from the manifest at `E`.
        for (buried, keys) in &self.graveyard {
            if *buried <= epoch.0 {
                continue;
            }
            for key in keys {
                // M16: a marker is a borrowed segment its branch stopped naming, and belongs
                // to the branch -- not to the index its path names.
                if let Some((segment, index)) = unscoped(key) {
                    if key_epoch(segment).is_some_and(|born| born <= epoch.0) {
                        out.indexes.entry(index).or_default().push(SegmentRef {
                            key: segment.to_owned(),
                            rows: 0,
                            dicts: None,
                            len: None,
                        });
                    }
                    continue;
                }
                // ⚠️ The graveyard holds **WAL bundles too**, and a bundle key ends in a
                // 16-digit zero-padded *sequence number* sitting exactly where a loose parser
                // would read an epoch. The index comes from the key's own path, so a bundle
                // simply has no index to belong to.
                let (Some(index), Some(born)) = (key_index(key).map(str::to_owned), key_epoch(key))
                else {
                    continue;
                };
                if born <= epoch.0 {
                    out.indexes.entry(index).or_default().push(SegmentRef {
                        key: key.clone(),
                        rows: 0,
                        dicts: None,
                        len: None,
                    });
                }
            }
        }
        // M16: a branch did not exist below the epoch it was made at.
        out.indexes.retain(|name, refs| {
            !refs.is_empty() && self.branched.get(name).is_none_or(|b| *b <= epoch.0)
        });
        for refs in out.indexes.values_mut() {
            refs.sort_by(|a, b| a.key.cmp(&b.key));
            // A segment two burials resurrect is one segment.
            refs.dedup_by(|a, b| a.key == b.key);
        }
        // ⚠️ **Each segment's delete vector as it stood at the epoch** (M9c): the one written at
        // or before it -- the current one, or one buried after it. The present's vector applied
        // to the past would hide rows that were live then. There is **at most one**: a vector
        // is buried at the epoch its successor is written, so two written at or before the
        // epoch cannot both be buried after it.
        let mut best: BTreeMap<String, (String, u32)> = BTreeMap::new();
        let mut consider = |key: &str, rows: u32| {
            let Some((segment, epoch)) = dv_of(key) else {
                return;
            };
            if epoch <= out.epoch.0 {
                best.insert(segment.to_owned(), (key.to_owned(), rows));
            }
        };
        for (key, rows) in self.deletes.values() {
            consider(key, *rows);
        }
        for (buried, keys) in &self.graveyard {
            if *buried <= out.epoch.0 {
                continue;
            }
            for key in keys {
                // The count of a buried vector is not kept: the query reads the vector itself.
                consider(key, 0);
            }
        }
        // M16: live as each index records its vectors -- a borrowed segment's scoped.
        let live: std::collections::BTreeSet<String> = out
            .indexes
            .iter()
            .flat_map(|(index, refs)| refs.iter().map(move |r| dv_ref(index, &r.key)))
            .collect();
        out.deletes = best
            .into_iter()
            .filter(|(segment, _)| live.contains(segment.as_str()))
            .collect();
        Ok(out)
    }

    /// Whether this lane's sequence has already been folded into a committed epoch.
    #[must_use]
    pub fn is_folded(&self, lane: LaneId, seq: Seq) -> bool {
        self.watermarks.get(&lane.0).is_some_and(|w| *w > seq.0)
    }
}

/// Unsigned LEB128 (M45): seven bits a byte, low first, the top bit set on every byte but
/// the last.
fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v & 0x7F) as u8 + 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

/// A recorded type's byte in HEAD (M173): its place in [`pstore_format::typed::Kind::ALL`].
fn kind_code(k: pstore_format::typed::Kind) -> u8 {
    pstore_format::typed::Kind::ALL
        .iter()
        .position(|x| *x == k)
        .unwrap_or_default() as u8
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
    fn u8(&mut self) -> Result<u8, EngineError> {
        self.take(1)?
            .first()
            .copied()
            .ok_or(EngineError::CorruptHead)
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
    /// [`put_varint`]'s inverse, refusing anything it would not have written: past 10 bytes,
    /// past `u64`, or longer than the shortest form, so a decode re-encodes byte for byte.
    fn varint(&mut self) -> Result<u64, EngineError> {
        let mut v = 0u64;
        for i in 0..10 {
            let b = self.u8()?;
            // The tenth byte holds bit 63 alone.
            if i == 9 && b > 1 {
                return Err(EngineError::CorruptHead);
            }
            v += u64::from(b & 0x7F) << (7 * i);
            if b < 0x80 {
                // A last byte of 0 after another is a longer form of a shorter number.
                if i > 0 && b == 0 {
                    return Err(EngineError::CorruptHead);
                }
                return Ok(v);
            }
        }
        Err(EngineError::CorruptHead)
    }
    fn string(&mut self) -> Result<String, EngineError> {
        let n = self.u32()? as usize;
        String::from_utf8(self.take(n)?.to_vec()).map_err(|_| EngineError::CorruptHead)
    }
}

/// The epoch a segment key was born at: `…/seg/L0/{epoch:020}-{lane:016x}.seg`.
pub(crate) fn key_epoch(key: &str) -> Option<u64> {
    key.rsplit_once('/')?.1.split('-').next()?.parse().ok()
}

/// The index a segment key belongs to, or `None` if it is not a segment key at all.
///
/// ⚠️ The prefix is what separates a segment from a WAL bundle, which lives under `…/wal/` and
/// whose filename is a sequence number in the same shape as an epoch.
fn key_index(key: &str) -> Option<&str> {
    // Only a segment's own key: a delete vector (M9c) and every sidecar share its path.
    if !key.ends_with(".seg") {
        return None;
    }
    // ⚠️ The LAST `/seg/` (M16 code review): an index name may hold one, and its segments
    // would otherwise read as borrowed from the index named by the prefix before it.
    let (before, _) = key.rsplit_once("/seg/")?;
    let (_, index) = before.split_once("/idx/")?;
    (!index.is_empty()).then_some(index)
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
pub(crate) async fn read<S: BlobStore + ?Sized>(
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
    // ⚠️ **The ABA guard is this check, not the nonce** (M105, OQ-6). A content-derived tag,
    // single-part or multipart, repeats exactly when the bytes do, and the epoch is what keeps
    // two HEADs from being byte-identical. Every caller advances it; this makes that a
    // property of the one place they all pass through rather than of each of them.
    // `Refused` although it is the engine's bug, not a client's: no caller reaches it.
    if next.epoch <= at.head.epoch {
        return Err(EngineError::Refused(format!(
            "a HEAD at epoch {} does not advance on the {} it was read at",
            next.epoch.0, at.head.epoch.0
        )));
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_scoped_segment_key_unscopes() {
        let seg = "0046/tnt/70/idx/a/seg/L0/00000000000000000001-0000000000000001.seg";
        let key = scoped("b.x", seg);
        assert_eq!(unscoped(&key), Some((seg, "b.x".to_owned())));
        // Not a segment before the tag, an odd-length tag, a tag that is not hex, and a
        // delete vector built on a scoped key: none is a scoped key.
        assert_eq!(unscoped("0046/tnt/70/wal/1/7.br-6162"), None);
        assert_eq!(unscoped(&format!("{seg}.br-616")), None);
        assert_eq!(unscoped(&format!("{seg}.br-zz")), None);
        assert_eq!(unscoped(&dv_key(&key, 3, 1)), None);
    }

    /// M120.9: one buffer is what a `String` a byte gave (M119), for every byte a name can
    /// hold -- a low one included, whose hex needs its leading zero. The key is durable.
    #[test]
    fn a_scoped_key_is_the_byte_by_byte_one() {
        let seg = "0046/tnt/70/idx/a/seg/L0/00000000000000000001-0000000000000001.seg";
        let names: Vec<String> = vec![
            (0u8..=0x7f).map(char::from).collect(),
            "\u{1}\u{f}\u{10}".into(),
            "é\u{2603}\u{1F600}".into(),
            String::new(),
        ];
        for name in &names {
            let by_byte: String = name.bytes().map(|b| format!("{b:02x}")).collect();
            assert_eq!(scoped(name, seg), format!("{seg}.br-{by_byte}"), "{name:?}");
        }
    }

    fn hex(b: &[u8]) -> String {
        b.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// A HEAD with a branch and a running replication, and no quarantine.
    fn sample() -> Head {
        let mut h = Head {
            epoch: pstore_types::Epoch(7),
            ..Head::default()
        };
        h.branched.insert("b".to_owned(), 3);
        h.replications.insert(
            "r".to_owned(),
            Replication {
                source: ReplicaSource {
                    store: String::new(),
                    tenant: pstore_types::TenantId(5),
                    index: "s".to_owned(),
                },
                running: true,
                run: 2,
                applied: None,
                rejected: 0,
            },
        );
        h
    }

    /// One index, `a`, of two segments: the first's dictionaries known, the second's not.
    fn with_dicts() -> Head {
        let mut h = Head {
            epoch: pstore_types::Epoch(11),
            ..Head::default()
        };
        h.indexes.insert(
            "a".to_owned(),
            vec![
                SegmentRef {
                    key: "k/1.seg".to_owned(),
                    rows: 4,
                    dicts: Some(Dicts {
                        sparse: false,
                        text: true,
                        fde: false,
                    }),
                    len: None,
                },
                SegmentRef {
                    key: "k/2.seg".to_owned(),
                    rows: 5,
                    dicts: None,
                    len: None,
                },
            ],
        );
        h
    }

    #[test]
    fn the_dicts_section_round_trips_and_refuses_malformed_input() -> Result<(), EngineError> {
        // M32: round-trips, beside every other optional section too.
        let h = with_dicts();
        assert_eq!(Head::decode(&h.encode())?, h);
        let mut all = sample();
        all.indexes = h.indexes.clone();
        all.quarantine
            .insert("q".to_owned(), vec![("k/1.q".to_owned(), 1)]);
        assert_eq!(Head::decode(&all.encode())?, all);

        // Nothing known: byte-identical to the parent's encoding (`an_empty_quarantine...`
        // pins the empty case; this one has a segment).
        let mut none = with_dicts();
        for r in none.indexes.values_mut().flatten() {
            r.dicts = None;
        }
        assert_eq!(
            hex(&none.encode()),
            "0b00000000000000000000000000000001000000010000006102000000070000006b2f312e73656704000000070000006b2f322e73656705000000000000000000000000000000000000000000000000000000000000000000000000000000"
        );

        // The section is the last 15 bytes: a count of 1, the name `a`, a count of 2, two flags.
        let good = h.encode();
        let body = good
            .get(..good.len() - 15)
            .ok_or(EngineError::CorruptHead)?;
        let section = |name: &str, flags: &[u8]| {
            let mut b = body.to_vec();
            b.extend_from_slice(&1u32.to_le_bytes());
            b.extend_from_slice(&(name.len() as u32).to_le_bytes());
            b.extend_from_slice(name.as_bytes());
            b.extend_from_slice(&(flags.len() as u32).to_le_bytes());
            b.extend_from_slice(flags);
            b
        };
        assert_eq!(Head::decode(&section("a", &[2, 0xFF]))?, h);
        // M71.2: bit 2 is the multi-vector clusters, and the three bits are independent.
        for (flag, want) in [
            (4u8, (false, false, true)),
            (5, (true, false, true)),
            (6, (false, true, true)),
            (7, (true, true, true)),
        ] {
            let d = Head::decode(&section("a", &[flag, 0xFF]))?;
            let got = d
                .indexes
                .get("a")
                .and_then(|r| r.first())
                .and_then(|r| r.dicts)
                .ok_or(EngineError::CorruptHead)?;
            assert_eq!((got.sparse, got.text, got.fde), want, "flag {flag}");
            assert_eq!(d.encode(), section("a", &[flag, 0xFF]));
        }
        for (why, bad) in [
            ("a count that disagrees", section("a", &[2])),
            // More flags than segments: without the count check, the extra byte is left over
            // and the section reads as valid.
            ("a count above the segments'", section("a", &[2, 0xFF, 0])),
            ("an index HEAD does not name", section("b", &[2, 0xFF])),
            ("a flag byte out of range", section("a", &[8, 0xFF])),
        ] {
            assert!(
                matches!(Head::decode(&bad), Err(EngineError::CorruptHead)),
                "{why} was accepted"
            );
        }

        // Truncated at the section, as a decoder from before M32 re-encodes it: unknown.
        let old = Head::decode(body)?;
        assert!(old.indexes.values().flatten().all(|r| r.dicts.is_none()));
        Ok(())
    }

    /// M45: [`with_dicts`]' index with lengths `lens`, in its segments' order.
    fn with_lens(lens: [Option<u64>; 2]) -> Head {
        let mut h = with_dicts();
        for (r, l) in h.indexes.values_mut().flatten().zip(lens) {
            r.len = l;
        }
        h
    }

    #[test]
    fn the_lengths_section_round_trips_and_refuses_malformed_input() -> Result<(), EngineError> {
        for l in [1, 127, 128, 1 << 32, u64::MAX] {
            let h = with_lens([Some(l), None]);
            assert_eq!(Head::decode(&h.encode())?, h, "{l}");
        }
        // With no dictionaries known, the lengths still force that section (code review): a
        // section skipped there would be read as the dictionaries.
        let mut bare = with_lens([Some(9), None]);
        for r in bare.indexes.values_mut().flatten() {
            r.dicts = None;
        }
        assert_eq!(Head::decode(&bare.encode())?, bare);
        // Beside every other optional section too.
        let mut all = sample();
        all.indexes = with_lens([Some(5), Some(300)]).indexes;
        all.quarantine
            .insert("q".to_owned(), vec![("k/1.q".to_owned(), 1)]);
        assert_eq!(Head::decode(&all.encode())?, all);

        // None known: byte for byte what the encoder before M45 wrote (the dicts test pins it).
        let none = with_lens([None, None]);
        assert_eq!(none.encode(), with_dicts().encode());

        // The section is the last 4 + (4 + 1) + 4 + 2 bytes for two one-byte lengths.
        let h = with_lens([Some(9), Some(10)]);
        let good = h.encode();
        let body = good
            .get(..good.len() - 15)
            .ok_or(EngineError::CorruptHead)?;
        let section = |name: &str, n: u32, lens: &[u8]| {
            let mut b = body.to_vec();
            b.extend_from_slice(&1u32.to_le_bytes());
            b.extend_from_slice(&(name.len() as u32).to_le_bytes());
            b.extend_from_slice(name.as_bytes());
            b.extend_from_slice(&n.to_le_bytes());
            b.extend_from_slice(lens);
            b
        };
        assert_eq!(Head::decode(&section("a", 2, &[9, 10]))?, h);
        // 0 is unknown; the largest length takes ten bytes, the last of them 1.
        let max = [0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x01];
        let mut both = max.to_vec();
        both.push(0);
        assert_eq!(
            Head::decode(&section("a", 2, &both))?,
            with_lens([Some(u64::MAX), None])
        );
        let mut over = max[..9].to_vec();
        over.extend_from_slice(&[2, 0]);
        let mut eleven = max[..9].to_vec();
        eleven.extend_from_slice(&[0x81, 0x01, 0]);
        for (why, bad) in [
            ("a truncated varint", section("a", 2, &[9, 0x80])),
            ("an 11-byte varint", section("a", 2, &eleven)),
            ("a varint past u64", section("a", 2, &over)),
            (
                "a varint longer than its shortest form",
                section("a", 2, &[9, 0x80, 0x00]),
            ),
            ("a count that disagrees", section("a", 1, &[9])),
            ("an index HEAD does not name", section("b", 2, &[9, 10])),
        ] {
            assert!(
                matches!(Head::decode(&bad), Err(EngineError::CorruptHead)),
                "{why} was accepted"
            );
        }

        // Truncated at the section, as a decoder from before M45 re-encodes it: unknown.
        let old = Head::decode(body)?;
        assert!(old.indexes.values().flatten().all(|r| r.len.is_none()));
        Ok(())
    }

    #[test]
    fn lengths_cost_three_bytes_a_megabyte_segment() {
        let refs = |len: Option<u64>| {
            (0..100)
                .map(|i| SegmentRef {
                    key: format!("k/{i}.seg"),
                    rows: 10,
                    dicts: Some(Dicts::default()),
                    len,
                })
                .collect::<Vec<_>>()
        };
        let mut known = Head::default();
        known.indexes.insert("idx".to_owned(), refs(Some(1 << 20)));
        let mut unknown = Head::default();
        unknown.indexes.insert("idx".to_owned(), refs(None));
        assert_eq!(
            known.encode().len() - unknown.encode().len(),
            4 + (4 + 3) + 4 + 3 * 100
        );
    }

    #[test]
    fn an_empty_quarantine_changes_no_byte() {
        // M25: the parent commit's encoder wrote exactly these bytes for the same HEADs.
        let bare = Head {
            epoch: pstore_types::Epoch(7),
            ..Head::default()
        };
        assert_eq!(
            hex(&bare.encode()),
            "0700000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000"
        );
        assert_eq!(
            hex(&sample().encode()),
            "070000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000100000001000000620300000000000000010000000100000072000000000500000000000000000000000000000001000000730102000000000000000000000000000000"
        );
    }

    #[test]
    fn a_head_without_a_quarantine_decodes() -> Result<(), EngineError> {
        let h = Head::decode(&sample().encode())?;
        assert!(h.quarantine.is_empty());
        assert_eq!(h.replications, sample().replications);
        Ok(())
    }

    #[test]
    fn a_quarantine_alone_round_trips() -> Result<(), EngineError> {
        // The only optional section: every earlier count must be written as 0 before it.
        let mut h = Head {
            epoch: pstore_types::Epoch(9),
            ..Head::default()
        };
        h.quarantine.insert(
            "docs".to_owned(),
            vec![("k/1.q".to_owned(), 2), ("k/2_1.q".to_owned(), 5)],
        );
        let back = Head::decode(&h.encode())?;
        assert_eq!(back.quarantine, h.quarantine);
        assert!(back.replications.is_empty() && back.branched.is_empty());
        // And beside a replication.
        let mut both = sample();
        both.quarantine = h.quarantine.clone();
        let back = Head::decode(&both.encode())?;
        assert_eq!(back.quarantine, h.quarantine);
        assert_eq!(back.replications, both.replications);
        Ok(())
    }

    fn external(kind: ExternalKind, n: u64) -> ExternalRef {
        ExternalRef {
            kind,
            source: format!("lake/ds{n}.parquet"),
            column: format!("emb{n}"),
            codes: format!("0046/tnt/70/ext/a/{n}.codes"),
            rows: 1000 + n,
            source_len: 123_456 + n,
        }
    }

    #[test]
    fn head_with_externals_round_trips_and_an_old_head_decodes() -> Result<(), EngineError> {
        // M86: an old HEAD has none, and decodes as none.
        let old = Head::decode(&sample().encode())?;
        assert!(old.externals.is_empty());
        // Externals alone: every earlier optional count is written as 0 before them.
        let mut h = Head {
            epoch: pstore_types::Epoch(9),
            ..Head::default()
        };
        h.externals
            .insert("a".to_owned(), external(ExternalKind::Parquet, 1));
        h.externals
            .insert("b".to_owned(), external(ExternalKind::Lance, 2));
        let back = Head::decode(&h.encode())?;
        assert_eq!(back.externals, h.externals);
        assert!(back.replications.is_empty() && back.quarantine.is_empty());
        // Beside everything else, segment lengths and dictionaries included.
        let mut all = sample();
        all.externals = h.externals.clone();
        let back = Head::decode(&all.encode())?;
        assert_eq!(back.externals, h.externals);
        assert_eq!(back.replications, all.replications);
        assert_eq!(back.indexes, all.indexes);
        // An empty set changes no byte of what was written before.
        assert_eq!(
            hex(&Head {
                externals: BTreeMap::new(),
                outcomes: Vec::new(),
                keys_since: None,
                ..sample()
            }
            .encode()),
            hex(&sample().encode())
        );
        Ok(())
    }

    /// M173 criterion 6: the recorded types round-trip, live and dropped, beside an absent
    /// `keys_since`, which reads back absent, after the text-field section (M152) they force;
    /// bytes after the types are refused.
    #[test]
    fn recorded_types_round_trip_after_keys_since() -> Result<(), EngineError> {
        use pstore_format::typed::Kind;
        let h = every_section();
        assert!(h.keys_since.is_none());
        let back = Head::decode(&h.encode())?;
        assert_eq!(back, h);
        let types = |h: &Head| {
            (
                h.schemas.get("a").map(|s| s.types.clone()),
                h.dropped.first().map(|(_, _, s)| s.types.clone()),
            )
        };
        let want = BTreeMap::from([
            ("owner".to_owned(), Kind::Uuid),
            ("seq".to_owned(), Kind::Uint64),
        ]);
        assert_eq!(types(&back), (Some(want.clone()), Some(want)));
        // Every kind, with `keys_since` present.
        let mut all = h.clone();
        all.keys_since = Some(7);
        if let Some(s) = all.schemas.get_mut("a") {
            s.types = Kind::ALL
                .into_iter()
                .map(|k| (k.name().to_owned(), k))
                .collect();
        }
        assert_eq!(Head::decode(&all.encode())?, all);
        // Without types, `keys_since` absent writes nothing, as before.
        let mut none = h.clone();
        for s in none.schemas.values_mut() {
            s.types.clear();
        }
        for (_, _, s) in &mut none.dropped {
            s.types.clear();
        }
        let bytes = none.encode();
        assert_eq!(Head::decode(&bytes)?, none);
        // The types force an empty text-field section (M152) before them; beside a text field
        // they follow it.
        let mut texted = h.clone();
        if let Some(s) = texted.schemas.get_mut("a") {
            s.texts
                .insert("notes".to_owned(), pstore_format::text::FullText::default());
        }
        assert_eq!(Head::decode(&texted.encode())?, texted);
        assert_eq!(types(&Head::decode(&texted.encode())?), types(&back));
        let mut longer = all.encode();
        longer.push(0);
        assert!(Head::decode(&longer).is_err());
        Ok(())
    }

    /// M132 criterion 10: `keys_since` round-trips; without it a HEAD encodes as M131's, and
    /// with it the M131 bytes are a prefix, so M131's decoder -- which stopped after the
    /// outcome list without checking for the end -- reads it. A cut inside it is refused.
    #[test]
    fn keys_since_round_trips_after_the_m131_bytes() -> Result<(), EngineError> {
        for outcomes in [vec![], vec![(3, "o3".to_owned()), (5, "o5".to_owned())]] {
            let m131 = Head {
                outcomes: outcomes.clone(),
                ..sample()
            };
            let with = Head {
                keys_since: Some(4),
                ..m131.clone()
            };
            let bytes = with.encode();
            assert_eq!(Head::decode(&bytes)?, with);
            if outcomes.is_empty() {
                // Its empty optional sections are forced before it, then the field.
                let back = Head::decode(&bytes)?;
                assert!(back.outcomes.is_empty());
            } else {
                let old = m131.encode();
                assert_eq!(
                    hex(bytes.get(..old.len()).ok_or(EngineError::CorruptHead)?),
                    hex(&old)
                );
                assert_eq!(bytes.len(), old.len() + 8);
                assert_eq!(Head::decode(&old)?.keys_since, None);
                for cut in old.len() + 1..bytes.len() {
                    assert!(
                        Head::decode(bytes.get(..cut).ok_or(EngineError::CorruptHead)?).is_err(),
                        "a cut at {cut} decoded"
                    );
                }
                let mut longer = bytes.clone();
                longer.push(0);
                assert!(Head::decode(&longer).is_err());
            }
        }
        Ok(())
    }

    #[test]
    fn a_damaged_externals_section_is_a_corrupt_head() -> Result<(), EngineError> {
        let mut h = Head::default();
        h.externals
            .insert("a".to_owned(), external(ExternalKind::Lance, 1));
        let full = h.encode();
        // Cut anywhere inside the section: never a valid HEAD with the registration missing.
        let start = Head::default().encode().len();
        for cut in start + 1..full.len() {
            let r = Head::decode(full.get(..cut).ok_or(EngineError::CorruptHead)?);
            assert!(
                r.map_or(true, |h| h.externals.is_empty()),
                "cut at {cut} of {}",
                full.len()
            );
        }
        // A kind that is neither Parquet nor Lance.
        let mut bad = full.clone();
        let src = b"lake/ds1.parquet";
        let at = bad
            .windows(src.len())
            .position(|w| w == src)
            .ok_or(EngineError::CorruptHead)?;
        // The byte before the source's 4-byte length is the kind.
        assert_eq!(bad.get(at - 4).copied(), Some(src.len() as u8));
        *bad.get_mut(at - 5).ok_or(EngineError::CorruptHead)? = 9;
        assert!(matches!(Head::decode(&bad), Err(EngineError::CorruptHead)));
        Ok(())
    }

    /// A HEAD with every section written: every optional one non-empty, every schema field
    /// off its default.
    fn every_section() -> Head {
        let mut h = sample();
        h.nonce = 0xfeed;
        h.indexes = with_lens([Some(5), Some(300)]).indexes;
        h.watermarks.insert(1, 9);
        let schema = IndexSchema {
            dims: 4,
            text_field: "body".to_owned(),
            metric: Metric::CosineDistance,
            fts: pstore_format::text::FullText {
                k1: 1.5,
                b: 0.25,
                ..Default::default()
            },
            trigram: vec!["title".to_owned(), "url".to_owned()],
            fields: BTreeMap::from([("late".to_owned(), 5)]),
            types: BTreeMap::from([
                ("owner".to_owned(), pstore_format::typed::Kind::Uuid),
                ("seq".to_owned(), pstore_format::typed::Kind::Uint64),
            ]),
            texts: Default::default(),
        };
        h.schemas.insert("a".to_owned(), schema.clone());
        h.schema_rejects.insert("a".to_owned(), 3);
        h.reaped_before = 6;
        h.graveyard
            .insert(5, vec!["k/0.seg".to_owned(), "k/0.seg.dv".to_owned()]);
        h.deletes
            .insert("k/1.seg".to_owned(), ("k/1.seg.dv".to_owned(), 2));
        h.dropped.push(("gone".to_owned(), 4, schema));
        h.quarantine
            .insert("a".to_owned(), vec![("k/1.q".to_owned(), 1)]);
        h.externals
            .insert("x".to_owned(), external(ExternalKind::Lance, 1));
        h
    }

    #[test]
    #[allow(
        clippy::indexing_slicing,
        reason = "every cut is below the length it is taken from"
    )]
    fn a_head_of_every_section_is_refused_cut_anywhere_but_a_section_boundary()
    -> Result<(), EngineError> {
        let h = every_section();
        let bytes = h.encode();
        assert_eq!(Head::decode(&bytes)?, h);
        let decodable: Vec<usize> = (0..bytes.len())
            .filter(|cut| Head::decode(&bytes[..*cut]).is_ok())
            .collect();
        // One decodable cut per optional section -- the fifteen `at_end` checks `decode`
        // makes -- and none inside a section: a `?` that read short as a default would add
        // one. (`a_truncated_head_is_refused_at_every_length_but_a_section_boundary` pins the
        // same for a HEAD whose later sections are empty.)
        // M173: the recorded types force the outcomes, `keys_since` and M152's text fields,
        // whose `at_end` checks now each find a cut inside the HEAD: 19.
        assert_eq!(decodable.len(), 19, "{decodable:?} of {}", bytes.len());
        for cut in &decodable {
            assert_ne!(
                Head::decode(&bytes[..*cut])?,
                h,
                "a cut at {cut} read as the whole"
            );
        }
        // The first is where every HEAD before M7d ended: its segments are whole (their
        // dictionaries and lengths are later sections), and the schemas absent.
        let old = Head::decode(&bytes[..decodable[0]])?;
        let segs = |h: &Head| -> Vec<(String, u32)> {
            h.indexes
                .values()
                .flatten()
                .map(|r| (r.key.clone(), r.rows))
                .collect()
        };
        assert_eq!(segs(&old), segs(&h));
        assert_eq!(
            (&old.watermarks, &old.graveyard),
            (&h.watermarks, &h.graveyard)
        );
        assert!(old.schemas.is_empty() && old.externals.is_empty());
        Ok(())
    }

    #[test]
    fn a_suffixed_name_parses_as_its_base() {
        // M23: a refused name takes `_{n:x}` before its extension, and every parser reads the
        // epoch, the index and the segment exactly as from the name without it.
        let base = "0046/tnt/70/idx/a/seg/L0/00000000000000000007-0000000000000001";
        let (seg, seg1) = (format!("{base}.seg"), format!("{base}_1.seg"));
        assert_eq!(key_epoch(&seg1), Some(7));
        assert_eq!(key_index(&seg1), key_index(&seg));
        assert_eq!(key_index(&seg1), Some("a"));
        let dv = dv_key(&seg, 9, 1);
        let dv1 = format!("{}_a.dv", dv.trim_end_matches(".dv"));
        assert_eq!(dv_of(&dv1), Some((seg.as_str(), 9)));
        assert_eq!(dv_of(&dv1), dv_of(&dv));
        assert_eq!(key_index(&dv1), None);
        assert_eq!(unscoped(&seg1), None);
    }

    /// M152 criterion 2: a HEAD carrying every optional section before the further text
    /// fields' -- a non-default analyzer, a trigram set, named widths, a dropped schema,
    /// dictionaries, lengths, an external and outcomes -- with or without `keys_since`.
    fn rich(keys_since: Option<u64>) -> Head {
        let mut h = with_dicts();
        h.epoch = pstore_types::Epoch(21);
        if let Some(r) = h.indexes.get_mut("a").and_then(|r| r.first_mut()) {
            r.len = Some(4096);
        }
        let fts = pstore_format::text::FullText {
            k1: 2.0,
            b: 0.3,
            ..Default::default()
        };
        let schema = IndexSchema {
            dims: 3,
            text_field: "body".to_owned(),
            metric: Metric::CosineDistance,
            fts,
            trigram: vec!["body".to_owned(), "tag".to_owned()],
            fields: BTreeMap::from([("img".to_owned(), 4)]),
            ..IndexSchema::default()
        };
        h.schemas.insert("a".to_owned(), schema.clone());
        h.dropped.push(("gone".to_owned(), 9, schema));
        h.branched.insert("a".to_owned(), 2);
        h.externals
            .insert("x".to_owned(), external(ExternalKind::Parquet, 3));
        h.outcomes.push((20, "o20".to_owned()));
        h.keys_since = keys_since;
        h
    }

    /// `rich(None)` and `rich(Some(17))` as the parent's encoder wrote them: captured at
    /// 4af59f2 with `cargo test -p pstore-engine --lib print_rich_for_capture -- --nocapture`,
    /// a test that printed `hex(&rich(..).encode())` and was then replaced by this one.
    const RICH_NONE: &str = concat!(
        "1500000000000000000000000000000001000000010000006102000000070000006b2f312e736567",
        "04000000070000006b2f322e73656705000000000000000000000001000000010000006103000000",
        "04000000626f64790000000000000000000000000000000001000000010000006101010000000400",
        "0000676f6e6509000000000000000300000004000000626f64790102000000000100000061210000",
        "0076313b656e676c6973683b303030303b34303030303030303b3365393939393961010000000021",
        "00000076313b656e676c6973683b303030303b34303030303030303b336539393939396102000000",
        "0001000000610200000004000000626f64790300000074616701000000000200000004000000626f",
        "64790300000074616701000000010000006102000000000000000000000000000000010000000100",
        "0000610200000002ff0100000001000000610200000080200001000000010000007800100000006c",
        "616b652f6473332e7061727175657404000000656d623319000000303034362f746e742f37302f65",
        "78742f612f332e636f646573eb0300000000000043e2010000000000020000000001000000610100",
        "000003000000696d670400000001000000000100000003000000696d670400000001000000140000",
        "0000000000030000006f3230",
    );
    const RICH_SOME: &str = concat!(
        "1500000000000000000000000000000001000000010000006102000000070000006b2f312e736567",
        "04000000070000006b2f322e73656705000000000000000000000001000000010000006103000000",
        "04000000626f64790000000000000000000000000000000001000000010000006101010000000400",
        "0000676f6e6509000000000000000300000004000000626f64790102000000000100000061210000",
        "0076313b656e676c6973683b303030303b34303030303030303b3365393939393961010000000021",
        "00000076313b656e676c6973683b303030303b34303030303030303b336539393939396102000000",
        "0001000000610200000004000000626f64790300000074616701000000000200000004000000626f",
        "64790300000074616701000000010000006102000000000000000000000000000000010000000100",
        "0000610200000002ff0100000001000000610200000080200001000000010000007800100000006c",
        "616b652f6473332e7061727175657404000000656d623319000000303034362f746e742f37302f65",
        "78742f612f332e636f646573eb0300000000000043e2010000000000020000000001000000610100",
        "000003000000696d670400000001000000000100000003000000696d670400000001000000140000",
        "0000000000030000006f32301100000000000000",
    );

    /// M152 criterion 2: a HEAD with no further text fields is byte for byte the parent's.
    #[test]
    fn a_head_without_further_fields_writes_the_parents_bytes() {
        assert_eq!(hex(&rich(None).encode()), RICH_NONE);
        assert_eq!(hex(&rich(Some(17)).encode()), RICH_SOME);
    }

    /// The further fields `multi` declares in M152's criterion 1.
    fn further() -> BTreeMap<String, pstore_format::text::FullText> {
        use pstore_format::text::{Analyzer, FullText, Language};
        BTreeMap::from([
            (
                "title".to_owned(),
                FullText {
                    analyzer: Analyzer {
                        language: Language::English,
                        stemming: true,
                        remove_stopwords: true,
                        ..Analyzer::default()
                    },
                    k1: 2.0,
                    b: 0.3,
                },
            ),
            (
                "tags".to_owned(),
                FullText {
                    analyzer: Analyzer {
                        case_sensitive: true,
                        ..Analyzer::default()
                    },
                    k1: 0.8,
                    b: 1.0,
                },
            ),
        ])
    }

    /// `rich(keys_since)` with further fields on its live schema and on its dropped one.
    fn texted(keys_since: Option<u64>) -> Head {
        let mut h = rich(keys_since);
        if let Some(s) = h.schemas.get_mut("a") {
            s.texts = further();
        }
        if let Some((_, _, s)) = h.dropped.first_mut() {
            s.texts = further().into_iter().take(1).collect();
        }
        h
    }

    /// M152 criterion 2: further fields round-trip, live and dropped, and a forced
    /// `keys_since` of `None` reads back as `None`.
    #[test]
    fn further_fields_round_trip_live_and_dropped() -> Result<(), EngineError> {
        for keys_since in [None, Some(17)] {
            let h = texted(keys_since);
            let back = Head::decode(&h.encode())?;
            assert_eq!(back, h);
            assert_eq!(back.keys_since, keys_since);
            assert_eq!(back.dropped.first().map(|d| d.2.texts.len()), Some(1));
        }
        // Alone, so every earlier optional section is forced before it.
        let mut alone = Head::default();
        alone.schemas.insert(
            "a".to_owned(),
            IndexSchema {
                texts: further(),
                ..IndexSchema::default()
            },
        );
        assert_eq!(Head::decode(&alone.encode())?, alone);
        // Only on a dropped schema.
        let mut dropped = Head::default();
        dropped.dropped.push((
            "gone".to_owned(),
            3,
            IndexSchema {
                texts: further(),
                ..IndexSchema::default()
            },
        ));
        assert_eq!(Head::decode(&dropped.encode())?, dropped);
        Ok(())
    }

    /// `rich(Some(17))`'s bytes, then a further-fields section of one record for live `a`.
    fn with_section(names: &[&str]) -> Vec<u8> {
        let mut out = rich(Some(17)).encode();
        out.extend_from_slice(&1u32.to_le_bytes());
        out.push(0);
        put_str(&mut out, "a");
        out.extend_from_slice(&(names.len() as u32).to_le_bytes());
        for n in names {
            put_str(&mut out, n);
            put_str(&mut out, &pstore_format::text::FullText::default().encode());
        }
        out
    }

    /// M152 criterion 5: a cut anywhere inside the section, and each impossible record, is
    /// `CorruptHead`.
    #[test]
    fn a_damaged_further_fields_section_is_a_corrupt_head() -> Result<(), EngineError> {
        let old = rich(Some(17)).encode();
        let bytes = texted(Some(17)).encode();
        assert_eq!(
            hex(bytes.get(..old.len()).ok_or(EngineError::CorruptHead)?),
            hex(&old),
            "the section is not last"
        );
        for cut in old.len() + 1..bytes.len() {
            assert!(
                Head::decode(bytes.get(..cut).ok_or(EngineError::CorruptHead)?).is_err(),
                "a cut at {cut} decoded"
            );
        }
        let mut longer = bytes.clone();
        longer.push(0);
        assert!(Head::decode(&longer).is_err());
        // The fixture is well formed: one name decodes.
        let fields = |b: &[u8]| Head::decode(b).map(|h| h.schemas.get("a").map(|s| s.texts.len()));
        assert_eq!(fields(&with_section(&["t0"]))?, Some(1));
        let nine: Vec<String> = (0..9).map(|i| format!("t{i}")).collect();
        let nine: Vec<&str> = nine.iter().map(String::as_str).collect();
        assert_eq!(
            fields(&with_section(
                nine.get(..8).ok_or(EngineError::CorruptHead)?
            ))?,
            Some(8)
        );
        for (why, names) in [
            ("nine names", nine.clone()),
            ("an empty name", vec![""]),
            ("a reserved name", vec!["$x"]),
            ("a repeated name", vec!["t", "t"]),
            ("the primary's name", vec!["body"]),
        ] {
            assert!(
                matches!(
                    Head::decode(&with_section(&names)),
                    Err(EngineError::CorruptHead)
                ),
                "{why} decoded"
            );
        }
        Ok(())
    }

    /// M148 criterion 6: `times` round-trips beside an absent and a present `keys_since`,
    /// with and without outcomes; a HEAD without it encodes as M147's; and `encode` keeps
    /// only the entries of indexes that exist (rows, rejects or a quarantine).
    #[test]
    fn times_round_trip_beside_keys_since() -> Result<(), EngineError> {
        for (keys_since, outcomes) in [
            (None, vec![]),
            (Some(4), vec![]),
            (None, vec![(3, "o3".to_owned())]),
            (Some(4), vec![(3, "o3".to_owned())]),
        ] {
            let mut m147 = Head {
                keys_since,
                outcomes,
                ..sample()
            };
            m147.indexes.insert("a".to_owned(), Vec::new());
            m147.schema_rejects.insert("r".to_owned(), 1);
            m147.quarantine
                .insert("q".to_owned(), vec![("k".to_owned(), 1)]);
            let mut with = m147.clone();
            for (i, n) in ["a", "r", "q"].into_iter().enumerate() {
                with.times.insert(n.to_owned(), (i as u64, 10 + i as u64));
            }
            let back = Head::decode(&with.encode())?;
            assert_eq!(back, with, "{keys_since:?}");
            // Without times, M147's bytes exactly: the times entry of a name that does not
            // exist is never written.
            let mut ghost = m147.clone();
            ghost.times.insert("gone".to_owned(), (1, 2));
            assert_eq!(hex(&ghost.encode()), hex(&m147.encode()), "{keys_since:?}");
            assert_eq!(Head::decode(&m147.encode())?, m147);
            // A ghost beside a real entry is dropped, the real one kept.
            let mut both = with.clone();
            both.times.insert("gone".to_owned(), (1, 2));
            assert_eq!(Head::decode(&both.encode())?, with);
            let bytes = with.encode();
            let mut longer = bytes.clone();
            longer.push(0);
            assert!(Head::decode(&longer).is_err());
            for cut in m147.encode().len()..bytes.len() {
                let part = bytes.get(..cut).ok_or(EngineError::CorruptHead)?;
                // A cut leaving exactly M147's form is M147's HEAD; every other is refused.
                if let Ok(h) = Head::decode(part) {
                    assert!(h.times.is_empty(), "a cut at {cut} decoded times");
                }
            }
        }
        // After M152's further text fields, live and dropped.
        for keys_since in [None, Some(17)] {
            let mut h = texted(keys_since);
            h.times.insert("a".to_owned(), (5, 6));
            let back = Head::decode(&h.encode())?;
            assert_eq!(back, h, "{keys_since:?}");
            assert_eq!(back.times.len(), 1);
        }
        Ok(())
    }
}
