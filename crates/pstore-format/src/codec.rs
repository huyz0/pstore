//! Byte-level encode and decode.
//!
//! Hand-written rather than delegated to a serialization crate, because **the layout is
//! the deliverable**: the footer must sit at a known suffix offset, blocks must be
//! independently addressable by byte range, and a format version must be readable before
//! anything else is trusted. A derive macro hides exactly the decisions that matter here.

use crate::FormatError;

/// Appends primitives in little-endian, the order the decoder reads them.
#[derive(Debug, Default)]
pub(crate) struct Enc(pub Vec<u8>);

impl Enc {
    pub(crate) fn u8(&mut self, v: u8) {
        self.0.push(v);
    }
    pub(crate) fn u16(&mut self, v: u16) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    pub(crate) fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    pub(crate) fn u64(&mut self, v: u64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    pub(crate) fn i64(&mut self, v: i64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    pub(crate) fn f32(&mut self, v: f32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    pub(crate) fn f64(&mut self, v: f64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    pub(crate) fn bytes(&mut self, v: &[u8]) {
        // Length-prefixed, so a decoder never has to guess where a field ends.
        self.u32(v.len() as u32);
        self.0.extend_from_slice(v);
    }
    pub(crate) fn raw(&mut self, v: &[u8]) {
        self.0.extend_from_slice(v);
    }
    pub(crate) fn len(&self) -> usize {
        self.0.len()
    }
}

/// Reads primitives, refusing rather than panicking when the input runs out.
///
/// Every method returns `Result`: a truncated or tampered segment must be an error, not a
/// wrong answer served as data.
#[derive(Debug)]
pub(crate) struct Dec<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Dec<'a> {
    pub(crate) fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    /// Whether every byte has been read.
    pub(crate) fn at_end(&self) -> bool {
        self.pos >= self.buf.len()
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], FormatError> {
        let end = self.pos.checked_add(n).ok_or(FormatError::Truncated)?;
        let out = self.buf.get(self.pos..end).ok_or(FormatError::Truncated)?;
        self.pos = end;
        Ok(out)
    }

    pub(crate) fn u8(&mut self) -> Result<u8, FormatError> {
        self.take(1)?.first().copied().ok_or(FormatError::Truncated)
    }
    pub(crate) fn u16(&mut self) -> Result<u16, FormatError> {
        Ok(u16::from_le_bytes(
            self.take(2)?
                .try_into()
                .map_err(|_| FormatError::Truncated)?,
        ))
    }
    pub(crate) fn u32(&mut self) -> Result<u32, FormatError> {
        Ok(u32::from_le_bytes(
            self.take(4)?
                .try_into()
                .map_err(|_| FormatError::Truncated)?,
        ))
    }
    pub(crate) fn u64(&mut self) -> Result<u64, FormatError> {
        Ok(u64::from_le_bytes(
            self.take(8)?
                .try_into()
                .map_err(|_| FormatError::Truncated)?,
        ))
    }
    pub(crate) fn i64(&mut self) -> Result<i64, FormatError> {
        Ok(i64::from_le_bytes(
            self.take(8)?
                .try_into()
                .map_err(|_| FormatError::Truncated)?,
        ))
    }
    pub(crate) fn f32(&mut self) -> Result<f32, FormatError> {
        Ok(f32::from_le_bytes(
            self.take(4)?
                .try_into()
                .map_err(|_| FormatError::Truncated)?,
        ))
    }
    pub(crate) fn f64(&mut self) -> Result<f64, FormatError> {
        Ok(f64::from_le_bytes(
            self.take(8)?
                .try_into()
                .map_err(|_| FormatError::Truncated)?,
        ))
    }
    pub(crate) fn bytes(&mut self) -> Result<&'a [u8], FormatError> {
        let n = self.u32()? as usize;
        self.take(n)
    }
    pub(crate) fn string(&mut self) -> Result<String, FormatError> {
        String::from_utf8(self.bytes()?.to_vec()).map_err(|_| FormatError::Corrupt("bad utf8"))
    }
    pub(crate) fn raw(&mut self, n: usize) -> Result<&'a [u8], FormatError> {
        self.take(n)
    }
}

/// FNV-1a over the index section, so tampering is caught before it is served as data.
///
/// Not cryptographic and not meant to be: it detects corruption, not an adversary.
pub(crate) fn checksum(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x1000_0000_01b3);
    }
    h
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    reason = "assertions in tests are the reporting mechanism"
)]
mod tests {
    use super::*;

    #[test]
    fn every_primitive_round_trips() {
        let mut e = Enc::default();
        e.u8(7);
        e.u16(1000);
        e.u32(100_000);
        e.u64(10_000_000_000);
        e.i64(-42);
        e.f32(1.5);
        e.bytes(b"hello");
        e.raw(b"tail");
        let mut d = Dec::new(&e.0);
        assert_eq!(d.u8().unwrap(), 7);
        assert_eq!(d.u16().unwrap(), 1000);
        assert_eq!(d.u32().unwrap(), 100_000);
        assert_eq!(d.u64().unwrap(), 10_000_000_000);
        assert_eq!(d.i64().unwrap(), -42);
        assert!((d.f32().unwrap() - 1.5).abs() < f32::EPSILON);
        assert_eq!(d.bytes().unwrap(), b"hello");
        assert_eq!(d.raw(4).unwrap(), b"tail");
    }

    #[test]
    fn every_primitive_refuses_a_short_buffer_rather_than_panicking() {
        // A decoder that indexed instead of checking would panic on a truncated segment,
        // turning a recoverable read error into a process abort.
        assert!(Dec::new(&[]).u8().is_err());
        assert!(Dec::new(&[1]).u16().is_err());
        assert!(Dec::new(&[1, 2, 3]).u32().is_err());
        assert!(Dec::new(&[1; 7]).u64().is_err());
        assert!(Dec::new(&[1; 7]).i64().is_err());
        assert!(Dec::new(&[1, 2, 3]).f32().is_err());
        assert!(Dec::new(&[1, 2, 3]).bytes().is_err());
        assert!(
            Dec::new(&[9, 0, 0, 0, 1]).bytes().is_err(),
            "length beyond the buffer"
        );
        assert!(Dec::new(&[1, 2]).raw(5).is_err());
    }

    #[test]
    fn a_string_field_rejects_invalid_utf8() {
        let mut e = Enc::default();
        e.bytes(&[0xff, 0xfe]);
        assert!(matches!(
            Dec::new(&e.0).string().unwrap_err(),
            FormatError::Corrupt(_)
        ));
    }

    #[test]
    fn the_checksum_changes_when_any_byte_does() {
        assert_ne!(checksum(b"abc"), checksum(b"abd"));
        assert_ne!(checksum(b"abc"), checksum(b"acb"), "order must matter");
        assert_eq!(checksum(b"abc"), checksum(b"abc"));
    }
}
