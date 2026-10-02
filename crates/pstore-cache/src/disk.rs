//! The disk tier (M20): one `foyer` hybrid cache per class, behind the memory tier.
//!
//! ⚠️ **Validated by store, not by key.** A cached key's bytes never change (M20's spec,
//! Risks), so an entry needs no validation of its own. What changes is the store behind a
//! directory: a bucket wiped and recreated, or a directory pointed elsewhere. So the tier
//! records which store filled it, and empties itself -- deleting the files, not merely
//! skipping recovery -- before it serves a store it has no record of.
//!
//! ⚠️ **Deviates from D-23** ("index the directory in a small file; do not scan it"):
//! `foyer`'s recovery reads every block header on open. It is bounded in M20's ledger.

use crate::cache::Id;
use bytes::Bytes;
use pstore_blob::Class;
use std::path::PathBuf;

/// Where a core's disk tier lives, how large it is, and which store it caches.
#[derive(Debug, Clone)]
pub struct DiskConfig {
    dir: PathBuf,
    lane: u64,
    bytes: usize,
    identity: String,
    block: usize,
}

/// The default block: an entry larger than this is kept in memory only.
pub const DEFAULT_BLOCK: usize = 4 << 20;

impl DiskConfig {
    /// A tier of `bytes` under `<dir>/lane-<lane>`, for the store `identity` names.
    ///
    /// ⚠️ One directory per lane: a lane has a single writer, so two processes never share
    /// `foyer`'s files, and no lock is needed.
    #[must_use]
    pub fn new(
        dir: impl Into<PathBuf>,
        lane: u64,
        bytes: usize,
        identity: impl Into<String>,
    ) -> Self {
        Self {
            dir: dir.into(),
            lane,
            bytes,
            identity: identity.into(),
            block: DEFAULT_BLOCK,
        }
    }

    /// A different block size, for tests whose budgets are a few blocks.
    #[must_use]
    pub fn with_block(mut self, block: usize) -> Self {
        self.block = block;
        self
    }
}

/// The three class tiers, in `<dir>/lane-<n>/{pinned,meta,bulk}`.
pub(crate) struct Tiers {
    pinned: Tier,
    meta: Tier,
    bulk: Tier,
}

type Tier = foyer::HybridCache<Id, Bytes>;

/// What each class's `foyer` instance keeps in memory: ours sits in front of it, so this is
/// the least it will take, not a second budget.
const FOYER_MEMORY: usize = 64 << 10;

impl Tiers {
    /// Opens, or empties and opens, the tier `c` names.
    ///
    /// # Errors
    /// Why the directory could not be used. The caller bypasses the disk; it never fails.
    pub(crate) async fn open(c: &DiskConfig) -> Result<Self, String> {
        let root = c.dir.join(format!("lane-{}", c.lane));
        std::fs::create_dir_all(&root).map_err(|e| format!("{}: {e}", root.display()))?;
        let identity = root.join("identity");
        // ⚠️ The entry format is part of what the directory records: an entry passes its
        // checksum before its key is decoded, so another format's key would be decoded as
        // this one's -- misread lengths, and an allocation that aborts the process.
        let want = format!("{FORMAT}\n{}", c.identity);
        let recorded = std::fs::read_to_string(&identity).ok();
        if recorded.as_deref() != Some(want.as_str()) {
            // ⚠️ Delete, then sync, then record. Recorded first, a crash before the delete
            // would leave another store's entries under this store's name.
            // Any failure to delete is fatal, so the tier bypasses rather than serve what it
            // could not remove. One writer per lane directory, so nothing races the check.
            for class in CLASSES.map(|c| root.join(c)).iter().filter(|d| d.exists()) {
                std::fs::remove_dir_all(class)
                    .map_err(|e| format!("emptying {}: {e}", class.display()))?;
            }
            sync_dir(&root);
            write_synced(&identity, want.as_bytes())
                .map_err(|e| format!("{}: {e}", identity.display()))?;
            sync_dir(&root);
        }
        let (p, m, b) = crate::cache::shares(c.bytes);
        let pinned = tier(&root.join("pinned"), p, c.block).await?;
        let meta = match tier(&root.join("meta"), m, c.block).await {
            Ok(t) => t,
            Err(e) => {
                let _ = pinned.close().await;
                return Err(e);
            }
        };
        let bulk = match tier(&root.join("bulk"), b, c.block).await {
            Ok(t) => t,
            Err(e) => {
                let _ = pinned.close().await;
                let _ = meta.close().await;
                return Err(e);
            }
        };
        Ok(Self { pinned, meta, bulk })
    }

    fn of(&self, class: Class) -> &Tier {
        match class {
            Class::Pinned => &self.pinned,
            Class::Meta => &self.meta,
            Class::Bulk | Class::Scan => &self.bulk,
        }
    }

    /// ⚠️ A disk error is a miss: the read goes to the store, as it would uncached. A torn
    /// or corrupt entry is one too -- `foyer` checks each entry's checksum and key on load.
    pub(crate) async fn get(&self, id: &Id, class: Class) -> Option<Bytes> {
        match self.of(class).get(id).await {
            Ok(Some(e)) => Some(e.value().clone()),
            Ok(None) | Err(_) => None,
        }
    }

    /// Written on admission. ⚠️ An entry larger than a block is kept in memory only: `foyer`
    /// refuses it, and an insert the tier cannot make is dropped, never an error.
    pub(crate) fn insert(&self, id: &Id, bytes: &Bytes, class: Class) {
        self.of(class).insert(id.clone(), bytes.clone());
    }

    /// Waits for the writes in flight, then closes each class.
    pub(crate) async fn close(&self) {
        for t in [&self.pinned, &self.meta, &self.bulk] {
            let _ = t.close().await;
        }
    }
}

const CLASSES: [&str; 3] = ["pinned", "meta", "bulk"];

/// The version of the entry format below: bump it with any change to [`Id`]'s encoding, or to
/// its hash. ⚠️ `/1` hashed a derived `Hash`; `/2` hashes the encoding (M28), so a directory
/// filled under `/1` is emptied rather than left as entries no lookup can find.
const FORMAT: &str = "pstore-cache/2";

async fn tier(dir: &std::path::Path, bytes: usize, block: usize) -> Result<Tier, String> {
    let why = |e: &dyn std::fmt::Display| format!("{}: {e}", dir.display());
    // Whole blocks, and at least two: one being written, one to evict.
    let capacity = (bytes / block).max(2) * block;
    use foyer::DeviceBuilder as _;
    let device = foyer::FsDeviceBuilder::new(dir)
        .with_capacity(capacity)
        .build()
        .map_err(|e| why(&e))?;
    foyer::HybridCacheBuilder::new()
        .with_policy(foyer::HybridCachePolicy::WriteOnInsertion)
        .memory(FOYER_MEMORY)
        .with_weighter(|_: &Id, v: &Bytes| v.len())
        .storage()
        .with_engine_config(foyer::BlockEngineConfig::new(device).with_block_size(block))
        .with_recover_mode(foyer::RecoverMode::Quiet)
        .build()
        .await
        .map_err(|e| why(&e))
}

fn write_synced(path: &std::path::Path, body: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    let mut f = std::fs::File::create(path)?;
    f.write_all(body)?;
    f.sync_all()
}

/// Makes a directory's entries durable. ⚠️ Unix only: Windows cannot open a directory as a
/// file, and makes a rename or delete durable without being asked.
fn sync_dir(dir: &std::path::Path) {
    #[cfg(unix)]
    if let Ok(d) = std::fs::File::open(dir) {
        let _ = d.sync_all();
    }
    #[cfg(not(unix))]
    let _ = dir;
}

impl Id {
    /// A tag, the key, and two numbers: what [`foyer::Code::encode`] writes, and what the
    /// hash is taken over.
    fn parts(&self) -> (u8, &str, u64, u64) {
        match self {
            Id::Range(k, a, b) => (0, k, *a, *b),
            Id::Whole(k) => (1, k, 0, 0),
            Id::Suffix(k, n) => (2, k, *n, 0),
        }
    }
}

/// ⚠️ **The encoding's bytes, in order, and nothing else** (M28). `foyer` files a disk entry
/// under `XxHash64` of this, and recovers it by the same hash: a derived `Hash` feeds an
/// `isize` discriminant and native-endian integers, which Rust does not promise to keep, so
/// a toolchain upgrade could silently flush every disk tier. Streamed, not concatenated:
/// `XxHash64` gives the same value however its input is split.
impl std::hash::Hash for Id {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        let (tag, key, a, b) = self.parts();
        state.write(&[tag]);
        state.write(&(key.len() as u64).to_le_bytes());
        state.write(key.as_bytes());
        state.write(&a.to_le_bytes());
        state.write(&b.to_le_bytes());
    }
}

/// The foyer key: `Id` encoded **structurally**, a tag then its fields, so no two shapes and
/// no crafted key string can collide.
impl foyer::Code for Id {
    fn encode(&self, w: &mut impl std::io::Write) -> foyer::Result<()> {
        let io = |e: std::io::Error| foyer::Error::io_error(e);
        let (tag, key, a, b) = self.parts();
        w.write_all(&[tag]).map_err(io)?;
        w.write_all(&(key.len() as u64).to_le_bytes()).map_err(io)?;
        w.write_all(key.as_bytes()).map_err(io)?;
        w.write_all(&a.to_le_bytes()).map_err(io)?;
        w.write_all(&b.to_le_bytes()).map_err(io)
    }

    fn decode(r: &mut impl std::io::Read) -> foyer::Result<Self> {
        let io = |e: std::io::Error| foyer::Error::io_error(e);
        let u64s = |r: &mut dyn std::io::Read| -> foyer::Result<u64> {
            let mut b = [0u8; 8];
            r.read_exact(&mut b).map_err(io)?;
            Ok(u64::from_le_bytes(b))
        };
        let mut tag = [0u8; 1];
        r.read_exact(&mut tag).map_err(io)?;
        let len = usize::try_from(u64s(r)?).map_err(|e| {
            foyer::Error::io_error(std::io::Error::new(std::io::ErrorKind::InvalidData, e))
        })?;
        let mut key = vec![0u8; len];
        r.read_exact(&mut key).map_err(io)?;
        let key = String::from_utf8(key).map_err(|e| {
            foyer::Error::io_error(std::io::Error::new(std::io::ErrorKind::InvalidData, e))
        })?;
        let (a, b) = (u64s(r)?, u64s(r)?);
        match tag[0] {
            0 => Ok(Id::Range(key, a, b)),
            1 => Ok(Id::Whole(key)),
            2 => Ok(Id::Suffix(key, a)),
            t => Err(foyer::Error::io_error(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("unknown cache id tag {t}"),
            ))),
        }
    }

    fn estimated_size(&self) -> usize {
        let key = match self {
            Id::Range(k, ..) | Id::Whole(k) | Id::Suffix(k, _) => k.len(),
        };
        1 + 8 + key + 16
    }
}

#[cfg(test)]
mod tests {
    use super::Id;
    use std::hash::BuildHasher as _;

    /// M28 criterion 6: `foyer` finds a recovered entry by this hash, so it is pinned to bytes
    /// this crate defines. Each literal is `XxHash64`, seed 0, of the `Id`'s 40-byte encoding,
    /// computed by a reference implementation of the published algorithm, outside this code.
    #[test]
    fn the_disk_hash_is_the_encodings() {
        let hash = |id: Id| foyer::DefaultHasher::default().hash_one(id);
        let key = || "tnt/1/seg/a.seg".to_owned();
        assert_eq!(hash(Id::Range(key(), 4096, 8192)), 0x9c77_4f0d_9145_8c19);
        assert_eq!(hash(Id::Whole(key())), 0xc2ed_b142_b10c_2a82);
        assert_eq!(hash(Id::Suffix(key(), 65536)), 0x881b_2410_f45f_4894);
    }
}
