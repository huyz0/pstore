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
    /// Last, so the classes are dropped before it is shut down.
    _workers: Workers,
}

/// The runtime `foyer`'s flushers, reclaimers and file I/O run on, shut down with the tier.
///
/// ⚠️ **Not the caller's** (M118). foyer-storage 0.22.3 spawns a reclaim while it holds its
/// block state lock, a runtime shutting down drops that task at once, and the task's drop
/// takes the same lock again: a self-deadlock on the thread shutting the runtime down. On
/// the caller's runtime that is the test's thread, or `main`'s, so a panic or an early
/// return before `close` hung instead of failing. Here it is a worker nothing waits for.
///
/// ⚠️ **Shut down, never left to drop.** `foyer`'s flushers never exit, so a runtime held
/// only by its tasks outlived its tier -- ten threads leaked per core opened -- and dropping
/// a runtime inside another's async context panics.
struct Workers(Option<tokio::runtime::Runtime>);

impl Drop for Workers {
    fn drop(&mut self) {
        if let Some(rt) = self.0.take() {
            rt.shutdown_background();
        }
    }
}

/// Two workers: they only queue. The file I/O runs on the runtime's blocking pool.
const WORKERS: usize = 2;

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
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(WORKERS)
            .thread_name("pstore-cache")
            .enable_all()
            .build()
            .map_err(|e| format!("cache runtime: {e}"))?;
        let spawner = foyer::Spawner::from(rt.handle().clone());
        let workers = Workers(Some(rt));
        let pinned = tier(&root.join("pinned"), p, c.block, &spawner).await?;
        let meta = match tier(&root.join("meta"), m, c.block, &spawner).await {
            Ok(t) => t,
            Err(e) => {
                let _ = pinned.close().await;
                return Err(e);
            }
        };
        let bulk = match tier(&root.join("bulk"), b, c.block, &spawner).await {
            Ok(t) => t,
            Err(e) => {
                let _ = pinned.close().await;
                let _ = meta.close().await;
                return Err(e);
            }
        };
        Ok(Self {
            pinned,
            meta,
            bulk,
            _workers: workers,
        })
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

    /// Waits for the writes queued so far to reach the disk.
    pub(crate) async fn wait(&self) {
        for t in [&self.pinned, &self.meta, &self.bulk] {
            t.storage().wait().await;
        }
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

async fn tier(
    dir: &std::path::Path,
    bytes: usize,
    block: usize,
    spawner: &foyer::Spawner,
) -> Result<Tier, String> {
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
        .with_spawner(spawner.clone())
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
        let key: std::sync::Arc<str> = String::from_utf8(key)
            .map_err(|e| {
                foyer::Error::io_error(std::io::Error::new(std::io::ErrorKind::InvalidData, e))
            })?
            .into();
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
    #![allow(
        clippy::unwrap_used,
        clippy::indexing_slicing,
        reason = "assertions in tests are the reporting mechanism"
    )]
    use super::{Class, DiskConfig, Id, Tiers};
    use std::hash::BuildHasher as _;

    /// A tier in a fresh directory under the system's temporary one.
    async fn open(name: &str) -> (Tiers, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("pstore-tiers-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let c = DiskConfig::new(&dir, 1, 8 << 20, "A").with_block(1 << 20);
        (Tiers::open(&c).await.unwrap(), dir)
    }

    /// M118 criterion 1: a runtime shutting down drops `foyer`'s tasks on the thread doing it,
    /// and `foyer` can deadlock there. So none of its tasks is on the caller's runtime.
    #[tokio::test]
    async fn foyer_runs_on_the_tiers_workers() {
        let (t, dir) = open("workers").await;
        for class in [&t.pinned, &t.meta, &t.bulk] {
            let name = class
                .storage()
                .spawner()
                .spawn(async { std::thread::current().name().map(str::to_owned) })
                .await
                .unwrap();
            assert_eq!(name.as_deref(), Some("pstore-cache"));
        }
        t.close().await;
        drop(t);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// M118 criterion 2: `foyer`'s flushers never exit, so a runtime left to its tasks outlives
    /// the tier. Dropped, the tier's workers stop: a task spawned after never runs.
    #[tokio::test]
    async fn a_dropped_tier_stops_its_workers() {
        let (t, dir) = open("stops").await;
        let spawner = t.bulk.storage().spawner().clone();
        t.close().await;
        drop(t);
        let ran = tokio::time::timeout(std::time::Duration::from_secs(5), spawner.spawn(async {}))
            .await
            .unwrap();
        assert!(ran.is_err(), "a task ran on a dropped tier's runtime");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// M28 criterion 6: `foyer` finds a recovered entry by this hash, so it is pinned to bytes
    /// this crate defines. Each literal is `XxHash64`, seed 0, of the `Id`'s 40-byte encoding,
    /// computed by a reference implementation of the published algorithm, outside this code.
    #[test]
    fn the_disk_hash_is_the_encodings() {
        let hash = |id: Id| foyer::DefaultHasher::default().hash_one(id);
        let key = || std::sync::Arc::<str>::from("tnt/1/seg/a.seg");
        assert_eq!(hash(Id::Range(key(), 4096, 8192)), 0x9c77_4f0d_9145_8c19);
        assert_eq!(hash(Id::Whole(key())), 0xc2ed_b142_b10c_2a82);
        assert_eq!(hash(Id::Suffix(key(), 65536)), 0x881b_2410_f45f_4894);
    }

    /// A recovered disk entry is found by decoding its key, so every shape must round-trip,
    /// `estimated_size` must be the encoding's length (foyer budgets by it), and a short,
    /// mistagged or non-UTF-8 key must be refused rather than read as some other entry.
    #[test]
    fn every_shape_round_trips_and_a_bad_key_is_refused() {
        use foyer::Code as _;
        let key = || std::sync::Arc::<str>::from("tnt/1/seg/a.seg");
        for id in [
            Id::Range(key(), 4096, 8192),
            Id::Whole(key()),
            Id::Suffix(key(), 65536),
        ] {
            let mut wire = Vec::new();
            id.encode(&mut wire).unwrap();
            assert_eq!(wire.len(), id.estimated_size(), "{id:?}");
            assert_eq!(Id::decode(&mut wire.as_slice()).unwrap(), id);
            for cut in 0..wire.len() {
                assert!(
                    Id::decode(&mut &wire[..cut]).is_err(),
                    "{id:?} cut at {cut}"
                );
            }
        }
        let mut wire = Vec::new();
        Id::Whole(key()).encode(&mut wire).unwrap();
        wire[0] = 3;
        let refused = Id::decode(&mut wire.as_slice()).unwrap_err().to_string();
        assert!(refused.contains("unknown cache id tag 3"), "{refused}");
        wire[0] = 1;
        wire[9] = 0xff;
        assert!(Id::decode(&mut wire.as_slice()).is_err(), "non-UTF-8 key");
    }
    /// Floods every class with eight times its share, waits `linger`, and returns with the
    /// tier open: a reclaim in flight, and a flusher waiting for its block. Whether the
    /// runtime, dropped then, returned within 20 s.
    fn returns(round: usize, linger: std::time::Duration) -> bool {
        let (tx, rx) = std::sync::mpsc::channel();
        // ⚠️ A thread of its own: a run that hangs is abandoned, not joined.
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .unwrap();
            let dir = rt.block_on(async {
                let (t, dir) = open(&format!("flood-{round}")).await;
                let v = bytes::Bytes::from(vec![7u8; (1 << 18) - 4096]);
                for i in 0..256 {
                    for class in [Class::Pinned, Class::Meta, Class::Bulk] {
                        t.insert(&Id::Range(format!("k{i}").into(), 0, i), &v, class);
                    }
                }
                tokio::time::sleep(linger).await;
                dir
            });
            drop(rt);
            let _ = std::fs::remove_dir_all(dir);
            let _ = tx.send(());
        });
        rx.recv_timeout(std::time::Duration::from_secs(20)).is_ok()
    }

    /// M118 criterion 3: a runtime dropped with the tier open returns. Before M118, 25 of 100
    /// such drops hung, in `foyer`'s `on_reclaim_finish` waiting on its own lock.
    #[test]
    fn a_runtime_dropped_mid_flood_returns() {
        let hung = (0..20).find(|&r| !returns(r, std::time::Duration::from_micros(r as u64 * 20)));
        assert_eq!(
            hung, None,
            "a runtime dropped with the tier open never returned"
        );
    }
}
