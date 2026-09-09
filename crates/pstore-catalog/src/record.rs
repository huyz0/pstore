//! What the catalog stores about a tenant, and how it is put on the wire.

use crate::CatalogError;
use pstore_types::{Epoch, TenantId};

/// Whether the tenant still exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// Serving.
    Live,
    /// Tombstoned.
    ///
    /// ⚠️ **Kept in the run, filtered at read time.** Dropping a tombstone at fold time
    /// looks like a tidy-up and is a resurrection: the older `Live` record for the same
    /// tenant is still in the run beneath it, so the next enumeration reports a deleted
    /// tenant as live. It leaves only when the record it masks does.
    Deleted,
}

/// A tenant as of one of its HEAD epochs.
///
/// `epoch` is what orders two records for one tenant without a clock — which matters because
/// records arrive from any node and the catalog has no leader to serialise them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TenantRecord {
    /// Whose record this is.
    pub tenant: TenantId,
    /// The tenant HEAD epoch this describes.
    pub epoch: Epoch,
    /// The tenant's indexes, sorted. Names only — per-index config is not catalog state.
    pub indexes: Vec<String>,
    /// Live or tombstoned.
    pub state: State,
}

impl TenantRecord {
    /// A live record. `indexes` is sorted and deduplicated, so two observations of the same
    /// set compare equal however the caller ordered them.
    #[must_use]
    pub fn live(tenant: TenantId, epoch: Epoch, indexes: &[String]) -> Self {
        let mut indexes = indexes.to_vec();
        indexes.sort();
        indexes.dedup();
        Self {
            tenant,
            epoch,
            indexes,
            state: State::Live,
        }
    }

    /// A tombstone.
    #[must_use]
    pub fn deleted(tenant: TenantId, epoch: Epoch) -> Self {
        Self {
            tenant,
            epoch,
            indexes: Vec::new(),
            state: State::Deleted,
        }
    }

    /// What `Appender` compares to decide whether anything changed.
    ///
    /// ⚠️ **Epoch is deliberately not in it.** The epoch advances on every commit, so an
    /// identity that included it would make every commit an append — a request that scales
    /// with records, which is the thing the design forbids.
    pub(crate) fn identity(&self) -> u64 {
        let mut buf = Vec::new();
        put_state(&mut buf, self.state);
        for name in &self.indexes {
            put_str(&mut buf, name);
        }
        crate::keys::digest(&buf)
    }
}

pub(crate) fn encode_records(recs: &[TenantRecord]) -> Vec<u8> {
    let mut out = Vec::new();
    #[expect(
        clippy::cast_possible_truncation,
        reason = "a caller cannot hold u32::MAX records; decode bounds the count by the bytes"
    )]
    out.extend_from_slice(&(recs.len() as u32).to_le_bytes());
    for r in recs {
        out.extend_from_slice(&r.tenant.0.to_le_bytes());
        out.extend_from_slice(&r.epoch.0.to_le_bytes());
        put_state(&mut out, r.state);
        #[expect(
            clippy::cast_possible_truncation,
            reason = "a caller cannot hold u32::MAX names; decode bounds the count by the bytes"
        )]
        out.extend_from_slice(&(r.indexes.len() as u32).to_le_bytes());
        for name in &r.indexes {
            put_str(&mut out, name);
        }
    }
    out
}

pub(crate) fn decode_records(c: &mut Cur<'_>) -> Result<Vec<TenantRecord>, CatalogError> {
    let n = c.u32()?;
    // ⚠️ **Not `with_capacity(n)`, and there is no ceiling constant either.** `n` comes
    // straight off a blob, so reserving it is an out-of-memory abort driven by whatever is in
    // the bucket — and a guard against that is a number nobody can justify and no test can
    // observe, because a *loose* bound is only wrong for inputs the cursor refuses anyway.
    // Growing on demand needs neither: the loop cannot outrun the buffer, since every
    // iteration consumes at least 29 bytes and `take` refuses when they are not there.
    let mut out = Vec::new();
    for _ in 0..n {
        let tenant = TenantId(c.u128()?);
        let epoch = Epoch(c.u64()?);
        let state = match c.u8()? {
            0 => State::Live,
            1 => State::Deleted,
            _ => return Err(c.corrupt()),
        };
        let k = c.u32()?;
        let mut indexes = Vec::new();
        for _ in 0..k {
            indexes.push(c.string()?);
        }
        out.push(TenantRecord {
            tenant,
            epoch,
            indexes,
            state,
        });
    }
    Ok(out)
}

fn put_state(out: &mut Vec<u8>, s: State) {
    out.push(match s {
        State::Live => 0,
        State::Deleted => 1,
    });
}

pub(crate) fn put_str(out: &mut Vec<u8>, s: &str) {
    #[expect(
        clippy::cast_possible_truncation,
        reason = "index names are short; a longer one would fail to decode, not corrupt"
    )]
    out.extend_from_slice(&(s.len() as u32).to_le_bytes());
    out.extend_from_slice(s.as_bytes());
}

/// A bounds-checked cursor. Every read is `get`, never an index — a malformed object is an
/// error, never a panic on a path a node serves from.
pub(crate) struct Cur<'a> {
    pub(crate) b: &'a [u8],
    pub(crate) i: usize,
    pub(crate) what: &'a str,
}

impl Cur<'_> {
    pub(crate) fn corrupt(&self) -> CatalogError {
        CatalogError::Corrupt(self.what.to_owned())
    }
    fn take(&mut self, n: usize) -> Result<&[u8], CatalogError> {
        let end = self.i.checked_add(n).ok_or_else(|| self.corrupt())?;
        let out = self.b.get(self.i..end).ok_or_else(|| self.corrupt())?;
        self.i = end;
        Ok(out)
    }
    pub(crate) fn u8(&mut self) -> Result<u8, CatalogError> {
        Ok(self.take(1)?.first().copied().unwrap_or_default())
    }
    pub(crate) fn u32(&mut self) -> Result<u32, CatalogError> {
        let b: [u8; 4] = self.take(4)?.try_into().map_err(|_| self.corrupt())?;
        Ok(u32::from_le_bytes(b))
    }
    pub(crate) fn u64(&mut self) -> Result<u64, CatalogError> {
        let b: [u8; 8] = self.take(8)?.try_into().map_err(|_| self.corrupt())?;
        Ok(u64::from_le_bytes(b))
    }
    pub(crate) fn u128(&mut self) -> Result<u128, CatalogError> {
        let b: [u8; 16] = self.take(16)?.try_into().map_err(|_| self.corrupt())?;
        Ok(u128::from_le_bytes(b))
    }
    pub(crate) fn string(&mut self) -> Result<String, CatalogError> {
        let n = self.u32()? as usize;
        let b = self.take(n)?.to_vec();
        String::from_utf8(b).map_err(|_| self.corrupt())
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]
mod tests {
    use super::*;

    fn roundtrip(recs: &[TenantRecord]) -> Vec<TenantRecord> {
        let buf = encode_records(recs);
        let mut c = Cur {
            b: &buf,
            i: 0,
            what: "test",
        };
        decode_records(&mut c).expect("round-trips")
    }

    #[test]
    fn records_round_trip_including_the_tombstone() {
        let recs = vec![
            TenantRecord::live(TenantId(1), Epoch(4), &["b".into(), "a".into()]),
            TenantRecord::deleted(TenantId(2), Epoch(9)),
        ];
        let back = roundtrip(&recs);
        assert_eq!(back, recs);
        // Sorted at construction, so two observations of one set compare equal.
        assert_eq!(back[0].indexes, vec!["a".to_owned(), "b".to_owned()]);
    }

    #[test]
    fn a_truncated_record_is_an_error_not_a_panic() {
        let buf = encode_records(&[TenantRecord::live(TenantId(1), Epoch(1), &["x".into()])]);
        for cut in 1..buf.len() {
            let mut c = Cur {
                b: &buf[..cut],
                i: 0,
                what: "test",
            };
            assert!(decode_records(&mut c).is_err(), "cut at {cut} decoded");
        }
    }

    #[test]
    fn a_count_larger_than_the_bytes_is_refused() {
        // ⚠️ Both sides. A buffer whose count matches its bytes must DECODE, and one claiming
        // more must be refused — by the cursor running out, which is the only bound that
        // cannot be set wrong.
        let recs: Vec<_> = (0..3)
            .map(|i| TenantRecord::live(TenantId(i), Epoch(1), &[]))
            .collect();
        let exact = encode_records(&recs);
        let mut c = Cur {
            b: &exact,
            i: 0,
            what: "t",
        };
        assert_eq!(decode_records(&mut c).unwrap(), recs);

        for claimed in [4u32, 1 << 20, u32::MAX] {
            let mut absurd = exact.clone();
            absurd.splice(0..4, claimed.to_le_bytes());
            let mut c = Cur {
                b: &absurd,
                i: 0,
                what: "t",
            };
            assert!(decode_records(&mut c).is_err(), "claimed {claimed} decoded");
        }

        // Same for an index count that outruns what is left of the record.
        let mut names =
            encode_records(&[TenantRecord::live(TenantId(1), Epoch(1), &["ab".into()])]);
        let at = 4 + 16 + 8 + 1;
        names.splice(at..at + 4, u32::MAX.to_le_bytes());
        let mut c = Cur {
            b: &names,
            i: 0,
            what: "t",
        };
        assert!(decode_records(&mut c).is_err());
    }

    #[test]
    fn an_unknown_state_byte_is_refused() {
        // Forward compatibility is a decision, not a default: a state this reader does not
        // know is refused rather than guessed at, because guessing `Live` resurrects a
        // tenant a newer writer deleted.
        let mut rec = encode_records(&[TenantRecord::live(TenantId(1), Epoch(1), &[])]);
        rec[4 + 16 + 8] = 7;
        let mut c = Cur {
            b: &rec,
            i: 0,
            what: "t",
        };
        assert!(decode_records(&mut c).is_err());
    }

    #[test]
    fn a_name_that_is_not_utf8_is_refused() {
        let mut rec = encode_records(&[TenantRecord::live(TenantId(1), Epoch(1), &["ab".into()])]);
        let last = rec.len() - 1;
        rec[last] = 0xff;
        let mut c = Cur {
            b: &rec,
            i: 0,
            what: "t",
        };
        assert!(decode_records(&mut c).is_err());
    }

    #[test]
    fn identity_ignores_the_epoch_and_notices_everything_else() {
        let a = TenantRecord::live(TenantId(1), Epoch(1), &["x".into()]);
        let later = TenantRecord::live(TenantId(1), Epoch(99), &["x".into()]);
        // The whole point: a commit that changes nothing about the index set is not an append.
        assert_eq!(a.identity(), later.identity());
        let added = TenantRecord::live(TenantId(1), Epoch(1), &["x".into(), "y".into()]);
        assert_ne!(a.identity(), added.identity());
        assert_ne!(
            a.identity(),
            TenantRecord::deleted(TenantId(1), Epoch(1)).identity()
        );
    }
}
