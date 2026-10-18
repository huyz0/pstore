//! A Lance 2.1 `InlineBitpacking` mini-block chunk (M102).
//!
//! pylance 13 bit-packs most 2.1 integer, date and timestamp columns. A chunk is an 8-byte
//! header (a `u16` of level bytes, which is 0 for a column with no nulls, a `u16` of value bytes,
//! and padding), then the values: the chunk's bit width `W` as a value of the column's own width
//! `T` (32 or 64 bits), then 1,024 values of `W` bits in FastLanes' transposed order, `W × 128`
//! bytes. The chunk is padded to 8 bytes. A last chunk of fewer rows is still packed as 1,024.
//!
//! FastLanes' order, for `L = 1024 / T` lanes: the `row`-th `W`-bit field of lane `lane` is
//! value `ORDER[row / 8] × 16 + (row % 8) × 128 + lane`, and a lane's bits run through its words
//! `L × k + lane`, low bit first. The layout was read from pylance 13's files and checked by
//! decoding every packed column of a 2,600-row dataset back exactly; see
//! `docs/milestones/M102/SPEC.md`.

/// The value FastLanes' transposition visits each group of 8 rows in.
const ORDER: [usize; 8] = [0, 4, 2, 6, 1, 5, 3, 7];
/// Values in a packed chunk, however many rows it holds.
pub(crate) const VALUES: usize = 1024;
/// A chunk's header: two `u16`s and padding.
const HEADER: usize = 8;

/// The 1,024 values of the packed chunk `chunk` (header included), each `width` (4 or 8) bytes
/// little-endian, or `None` for a chunk that does not hold exactly what its header says: a
/// level byte, a bit width past the value's, or a body of another length.
pub(crate) fn unpack(chunk: &[u8], width: usize) -> Option<Vec<u8>> {
    let t = width.checked_mul(8).filter(|t| *t == 32 || *t == 64)?;
    let levels = u16::from_le_bytes(chunk.get(0..2)?.try_into().ok()?);
    let vlen = usize::from(u16::from_le_bytes(chunk.get(2..4)?.try_into().ok()?));
    if levels != 0 || chunk.len() != (HEADER + vlen).next_multiple_of(8) {
        return None;
    }
    let body = chunk.get(HEADER..HEADER + vlen)?;
    let w = usize::try_from(word(body, 0, width)?)
        .ok()
        .filter(|w| *w <= t)?;
    if vlen != width + w * (VALUES / 8) {
        return None;
    }
    let words = body.get(width..)?;
    let lanes = VALUES / t;
    let mask = if w == 64 { u64::MAX } else { (1u64 << w) - 1 };
    let mut out = vec![0u8; VALUES * width];
    for lane in 0..lanes {
        for row in 0..t {
            let (k, shift) = ((row * w) / t, (row * w) % t);
            let mut v = 0u64;
            if w > 0 {
                v = word(words, lanes * k + lane, width)? >> shift;
                // A field that straddles two of the lane's words takes its high bits from the next.
                if shift + w > t {
                    v |= word(words, lanes * (k + 1) + lane, width)? << (t - shift);
                }
            }
            let at = (ORDER.get(row / 8)? * 16 + (row % 8) * 128 + lane) * width;
            out.get_mut(at..at + width)?
                .copy_from_slice((v & mask).to_le_bytes().get(..width)?);
        }
    }
    Some(out)
}

/// The `i`th little-endian value of `width` bytes in `b`.
fn word(b: &[u8], i: usize, width: usize) -> Option<u64> {
    let raw = b.get(i.checked_mul(width)?..i.checked_add(1)?.checked_mul(width)?)?;
    let mut buf = [0u8; 8];
    buf.get_mut(..width)?.copy_from_slice(raw);
    Some(u64::from_le_bytes(buf))
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]
mod tests {
    use super::{ORDER, VALUES, unpack};

    /// A chunk of `values` packed at `w` bits in `width`-byte values, by FastLanes' order as the
    /// module states it, written independently of the unpacker: bit by bit.
    fn pack(values: &[u64], w: usize, width: usize) -> Vec<u8> {
        let t = width * 8;
        let lanes = VALUES / t;
        let mut words = vec![0u64; w * lanes];
        for lane in 0..lanes {
            for row in 0..t {
                let v = values[ORDER[row / 8] * 16 + (row % 8) * 128 + lane];
                for bit in 0..w {
                    let b = row * w + bit;
                    words[lanes * (b / t) + lane] |= ((v >> bit) & 1) << (b % t);
                }
            }
        }
        let mut body = (w as u64).to_le_bytes()[..width].to_vec();
        for word in words {
            body.extend_from_slice(&word.to_le_bytes()[..width]);
        }
        let mut chunk = vec![0, 0];
        chunk.extend_from_slice(&u16::try_from(body.len()).unwrap().to_le_bytes());
        chunk.extend_from_slice(&[0; 4]);
        chunk.extend_from_slice(&body);
        chunk.resize(chunk.len().next_multiple_of(8), 0);
        chunk
    }

    /// 1,024 values of at most `w` bits, each distinct where `w` allows it.
    fn values(w: usize) -> Vec<u64> {
        let mask = if w == 64 { u64::MAX } else { (1u64 << w) - 1 };
        (0..VALUES as u64)
            .map(|i| i.wrapping_mul(0x9E37_79B9_7F4A_7C15).rotate_left(17) & mask)
            .collect()
    }

    fn as_values(raw: &[u8], width: usize) -> Vec<u64> {
        raw.chunks_exact(width)
            .map(|c| {
                let mut b = [0u8; 8];
                b[..width].copy_from_slice(c);
                u64::from_le_bytes(b)
            })
            .collect()
    }

    #[test]
    fn the_fastlanes_order_is_unpacked_at_every_width() {
        for (width, w) in [
            (4, 0),
            (4, 1),
            (4, 7),
            (4, 31),
            (4, 32),
            (8, 0),
            (8, 33),
            (8, 64),
        ] {
            let want = values(w);
            let got = unpack(&pack(&want, w, width), width).expect("a packed chunk");
            assert_eq!(as_values(&got, width), want, "{w} bits in {width} bytes");
        }
        // The top value of every width survives: no bit is lost at a word's edge.
        for w in [13, 21, 32] {
            let top = vec![(1u64 << w) - 1; VALUES];
            assert_eq!(as_values(&unpack(&pack(&top, w, 4), 4).unwrap(), 4), top);
        }
    }

    #[test]
    fn a_chunk_that_disagrees_with_its_header_is_refused() {
        let good = pack(&values(7), 7, 4);
        assert!(unpack(&good, 4).is_some());
        // A width past the value's, with the header and body sized for it: only the width
        // check refuses these.
        for (width, w) in [(4, 33u8), (8, 65)] {
            let mut wide = pack(&values(usize::from(w) - 1), usize::from(w) - 1, width);
            wide.extend_from_slice(&[0; VALUES / 8]);
            let vlen = u16::from_le_bytes([wide[2], wide[3]]) + 128;
            wide[2..4].copy_from_slice(&vlen.to_le_bytes());
            wide[8] = w;
            assert_eq!(usize::from(vlen), width + usize::from(w) * 128);
            assert_eq!(unpack(&wide, width), None, "{w} bits in {width} bytes");
        }
        // A width the body is not sized for.
        let mut other = good.clone();
        other[8] = 6;
        assert_eq!(
            unpack(&other, 4),
            None,
            "a width that disagrees with the body"
        );
        // A short body, a padded one, a level byte, and a header that lies about the length.
        assert_eq!(unpack(&good[..good.len() - 8], 4), None, "short");
        assert_eq!(
            unpack(&[good.clone(), vec![0; 8]].concat(), 4),
            None,
            "long"
        );
        let mut levels = good.clone();
        levels[0] = 1;
        assert_eq!(unpack(&levels, 4), None, "levels");
        let mut len = good.clone();
        len[2] -= 4;
        assert_eq!(unpack(&len, 4), None, "a header past its values");
        // Only a 32- or 64-bit value is packed.
        assert_eq!(unpack(&good, 2), None);
        assert_eq!(unpack(&[], 4), None);
        assert_eq!(unpack(&good[..3], 4), None);
    }
}
