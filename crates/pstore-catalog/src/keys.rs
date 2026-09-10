//! Where a tenant's catalog record lives, computed rather than looked up.
//!
//! Every key here is a pure function of `(tenant, width)` or `(bucket, epoch, digest)`.
//! Nothing is discovered, which is what makes enumeration a fixed fan-out instead of a LIST.

use pstore_blob::Key;
use pstore_types::{Epoch, TenantId};

/// Buckets in a deployment that has never been widened.
///
/// `tenancy-scale-model.md` §8: 1M tenant records over 16,384 buckets is ~61 records each,
/// a ~73 KB run, and one parallel round.
pub const DEFAULT_WIDTH: u32 = 16_384;

/// The most buckets a deployment can have.
///
/// ⚠️ Set by the **key format**, not by taste: bucket keys are `{bucket:04x}`, and
/// `key-layout.md` naming rule 3 requires fixed width so ordering is byte-order. A 65,537th
/// bucket needs five digits, which is a different key space — so a split past this point
/// is a format change as well as a reassignment.
pub const MAX_WIDTH: u32 = 65_536;

/// How many buckets a deployment is sharded into.
///
/// A newtype because the value is load-bearing in two directions: zero would divide by zero,
/// and anything past [`MAX_WIDTH`] would not fit the key format. Both are refused at
/// construction so no call site has to remember.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Width(u32);

impl Default for Width {
    fn default() -> Self {
        Self(DEFAULT_WIDTH)
    }
}

impl Width {
    /// A width, or `None` if it is zero or wider than [`MAX_WIDTH`].
    #[must_use]
    pub const fn new(n: u32) -> Option<Self> {
        if n == 0 || n > MAX_WIDTH {
            None
        } else {
            Some(Self(n))
        }
    }

    /// The number of buckets.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }

    /// Every bucket id, which is what enumeration fans out over.
    pub fn all(self) -> impl Iterator<Item = u32> {
        0..self.0
    }
}

/// The bucket a tenant's record belongs to.
///
/// ⚠️ **Hashed, not truncated.** Tenant ids are a `u128` a caller chooses, so any structure
/// in them — a discriminator in the high bits, an allocator with a stride — becomes bucket
/// skew if the bucket is a slice of the id rather than a hash of it. Truncation is uniform
/// over the one id family a test is likeliest to use (`0, 1, 2, …`), which is why the test
/// for this uses three.
#[must_use]
pub fn bucket_of(tenant: TenantId, width: Width) -> u32 {
    // A `u64` remainder of a `u32` modulus always fits a `u32`.
    #[expect(
        clippy::cast_possible_truncation,
        reason = "hash % width < width <= MAX_WIDTH"
    )]
    {
        (hash(&tenant.0.to_le_bytes()) % u64::from(width.get())) as u32
    }
}

/// The deployment's one catalog register: `{epoch, width}`, and nothing else.
///
/// Small and cold on purpose. `key-layout.md`'s mutable-object census bills this at "bucket
/// width change (very rare)", and it stays true only because the per-bucket pointers absorb
/// every other change.
#[must_use]
pub fn root_key() -> Key {
    Key::new("cat/root")
}

/// A bucket's pointer: the run it names, and the changes not yet in that run.
#[must_use]
pub fn head_key(bucket: u32) -> Key {
    Key::new(format!("{bucket:04x}/cat/b/HEAD"))
}

/// An immutable run.
///
/// ⚠️ **The digest is in the key and it is load-bearing.** Two folders that read the same
/// head but see different `pending` — an append landed between their reads — produce
/// different runs at the same `run_epoch`. With an epoch-only key the CAS loser's write can
/// land *second*, leaving the head pointing at a run that is missing a record which is no
/// longer pending either. Different content, different key.
#[must_use]
pub fn run_key(bucket: u32, run_epoch: Epoch, digest: u64) -> Key {
    Key::new(format!(
        "{bucket:04x}/cat/b/{:020}-{digest:016x}",
        run_epoch.0
    ))
}

/// Everything a bucket owns, for the one path allowed to enumerate.
///
/// ⚠️ The bucket's `HEAD` shares this prefix with its runs. That is why a sweeper parses what
/// it finds rather than assuming, and keeps what it cannot parse.
#[must_use]
pub fn bucket_prefix(bucket: u32) -> Key {
    Key::new(format!("{bucket:04x}/cat/b/"))
}

/// `(run_epoch, digest)` from a run key, or `None` if it is not one.
///
/// ⚠️ **The inverse of [`run_key`], and it must stay that way.** A sweeper decides what to
/// delete on the strength of this, so a key shape that changes on one side and not the other
/// silently changes what is deleted. `run_key_round_trips_through_parse` pins the pair.
#[must_use]
pub fn parse_run_key(bucket: u32, key: &Key) -> Option<(Epoch, u64)> {
    let rest = key.as_str().strip_prefix(bucket_prefix(bucket).as_str())?;
    let (epoch, digest) = rest.split_once('-')?;
    // Fixed widths, checked: `{:020}` and `{:016x}` are what make byte order epoch order, and
    // a shorter field means this is some other object that happens to contain a dash.
    if epoch.len() != 20 || digest.len() != 16 {
        return None;
    }
    Some((
        Epoch(epoch.parse().ok()?),
        u64::from_str_radix(digest, 16).ok()?,
    ))
}

/// The FNV-1a 64-bit prime, `1099511628211`.
const FNV_PRIME: u64 = 0x100_0000_01b3;

/// FNV-1a with the full murmur3 finalizer.
///
/// Deliberately written out rather than taken from a crate: bucket assignment must agree
/// **byte for byte across processes and versions**, and `RandomState` is seeded per process.
/// The same function as `pstore-cluster`'s placement hash, copied rather than shared because
/// the catalog does not depend on the cluster layer — see this crate's docs.
fn hash(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(FNV_PRIME);
    }
    h ^= h >> 33;
    h = h.wrapping_mul(0xff51_afd7_ed55_8ccd);
    h ^= h >> 33;
    h = h.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
    h ^= h >> 33;
    h
}

/// A content digest, for the run key and for the change check in `Appender`.
#[must_use]
pub(crate) fn digest(bytes: &[u8]) -> u64 {
    hash(bytes)
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "assertions in tests are the reporting mechanism"
)]
mod tests {

    #[test]
    fn run_key_round_trips_through_parse() {
        // ⚠️ A sweeper deletes on the strength of `parse_run_key`, so a key shape that changes
        // on one side and not the other silently changes what is deleted. The pair is pinned.
        for (bucket, epoch, digest) in [
            (0u32, 0u64, 0u64),
            (1, 1, 0xdead_beef),
            (0xffff, u64::MAX, u64::MAX),
            (7, 9_999_999_999_999_999_999, 0x1234_5678_9abc_def0),
        ] {
            let k = run_key(bucket, Epoch(epoch), digest);
            assert_eq!(
                parse_run_key(bucket, &k),
                Some((Epoch(epoch), digest)),
                "{k:?}"
            );
            // ⚠️ And it does not answer for another bucket's key, which would let a sweep of
            // one bucket delete another's runs.
            assert_eq!(parse_run_key(bucket.wrapping_add(1), &k), None, "{k:?}");
        }
        // Everything else under the prefix is kept, which is the safe default.
        //
        // ⚠️ **Both one-sided cases**, because the width check is an `||` and a mutation sweep
        // found `&&` indistinguishable without them: every near-miss above gets BOTH fields
        // wrong, so the two operators agree. A key with a valid 20-digit epoch and a short
        // digest is the one that separates them — and under `&&` it parses, which lets the
        // sweeper delete an object that is not a run at all.
        for other in [
            "HEAD",
            "notes.txt",
            "0000-0000",
            "not-a-run",
            "00000000000000000001-abcd",
            "1-0123456789abcdef",
        ] {
            let k = Key::new(format!("{}{other}", bucket_prefix(3).as_str()));
            assert_eq!(parse_run_key(3, &k), None, "{other}");
        }
    }
    use super::*;

    #[test]
    fn width_refuses_zero_and_anything_past_the_key_format() {
        assert_eq!(Width::new(0), None);
        assert_eq!(Width::new(MAX_WIDTH + 1), None);
        assert_eq!(Width::new(MAX_WIDTH).map(Width::get), Some(MAX_WIDTH));
        assert_eq!(Width::default().get(), DEFAULT_WIDTH);
    }

    /// ⚠️ **The one test that makes the hash a format rather than an implementation.**
    /// Bucket assignment is not an internal detail: it decides which object a tenant's record
    /// lives in, so a change to the mixer silently re-buckets every tenant in an existing
    /// deployment and enumeration then reports what is left. A statistical test cannot see
    /// that — a weaker mixer still spreads well enough to pass an imbalance bound — so the
    /// values are pinned. **If this fails, the fix is never to update the constants**; it is
    /// to restore the function, or to ship a re-bucketing pass.
    #[test]
    fn the_hash_is_pinned_so_a_deployment_never_re_buckets() {
        let w = Width::new(256).expect("256 is a width");
        let got: Vec<u32> = [0u128, 1, 2, 1 << 64, 65_536, u128::MAX]
            .into_iter()
            .map(|id| bucket_of(TenantId(id), w))
            .collect();
        assert_eq!(got, vec![10, 97, 18, 177, 54, 111]);
    }

    #[test]
    fn keys_are_fixed_width_and_sort_in_epoch_order() {
        let w = Width::new(256).expect("256 is a width");
        let b = bucket_of(TenantId(7), w);
        assert!(b < 256);
        // Zero-padded, so byte order is numeric order -- naming rule 3.
        assert!(run_key(1, Epoch(2), 0xab).as_str() < run_key(1, Epoch(10), 0x01).as_str());
        assert_eq!(head_key(0x2f).as_str(), "002f/cat/b/HEAD");
    }
}
