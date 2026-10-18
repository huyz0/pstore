//! A Lance 2.1 vector page (M90, backlog row 73's last piece).
//!
//! Lance 2.1 replaces 2.0's `ArrayEncoding` with a **page layout**. A null-free
//! `FixedSizeList<float32>` is written one of two ways, both read here and nothing else:
//!
//! - **`FullZip`** (rows of 256 bytes or more): one buffer, the rows back to back.
//! - **`MiniBlock`** (smaller rows): two buffers. The first is a `u16` a chunk, `words << 4 |
//!   log2(values)`; the second is the chunks, each an 8-byte header followed by its rows padded to
//!   8 bytes. A chunk's `words` is its padded rows in 8-byte words, so every chunk's offset is
//!   arithmetic over the first buffer, which a build reads once.
//!
//! Anything else (nulls, a general compression, a dictionary, a layer other than a null-free
//! item) is refused by name. The layout was read from files written by one library, across 130
//! shapes of (rows, dimensions); see `docs/milestones/M90/SPEC.md`.

use super::{LanceError, Shape, Wire, bytes, fields, packed};
use pstore_blob::{BlobStore, Key};

const LAYOUT_URL: &[u8] = b"/lance.encodings21.PageLayout";
/// `RepDefLayer` of an item with no nulls, as the one entry of a layout's `layers`.
const ALL_VALID_ITEM: &[u8] = &[1];
/// A chunk's header: two `u16`s (no levels, the rows' size) and padding to 8 bytes.
const CHUNK_HEADER: u64 = 8;
const WORD: u64 = 8;
/// The most bytes of chunk index read for a page: 2 a chunk, as many chunks as the extent cap.
const MAX_INDEX: u64 = 2 << 22;
/// Values in a bit-packed chunk (M102).
const PACKED_VALUES: u64 = super::fastlanes::VALUES as u64;

/// A run of rows of one page.
pub(super) struct Piece {
    pub rows: u64,
    pub offset: u64,
    /// 0 for rows stored flat at `offset`. Otherwise the run is one bit-packed chunk (M102) of
    /// this many bytes, header included, at `offset`.
    pub packed: u64,
}

fn bad(why: &'static str) -> LanceError {
    LanceError::Malformed(why)
}

fn refused<T>(why: &str) -> Result<T, LanceError> {
    Err(LanceError::Unsupported(why.to_owned()))
}

/// `Flat(bits)`, with no buffer compression, and nothing more (M101).
fn plain_flat(compression: &[u8], bits: u32) -> bool {
    let Some(fs) = fields(compression) else {
        return false;
    };
    let [(1, Wire::Bytes(flat))] = fs.as_slice() else {
        return false;
    };
    fields(flat)
        .is_some_and(|f| matches!(f.as_slice(), [(1, Wire::Varint(b))] if *b == u64::from(bits)))
}

/// The value compression `shape` is stored with when nothing compresses it.
fn plain(compression: &[u8], shape: Shape) -> bool {
    match shape {
        Shape::Vector(dims) => plain_floats(compression, dims),
        Shape::Scalar(bits) => plain_flat(compression, bits),
    }
}

/// `InlineBitpacking(bits)` (field 5), the compression pylance 13 gives most integer columns
/// (M102), and nothing more.
fn bit_packed(compression: &[u8], bits: u32) -> bool {
    let Some(fs) = fields(compression) else {
        return false;
    };
    let [(5, Wire::Bytes(packing))] = fs.as_slice() else {
        return false;
    };
    fields(packing)
        .is_some_and(|f| matches!(f.as_slice(), [(1, Wire::Varint(b))] if *b == u64::from(bits)))
}

/// Why a scalar's mini-block page is not read (M101), named more closely than a vector's: a
/// layer other than an all-valid item holds nulls, and `InlineBitpacking` is read only at the
/// value's own width (M102).
fn scalar_refusal(layout: &[(u64, Wire<'_>)], bits: u32) -> Option<&'static str> {
    let nulls = layout
        .iter()
        .any(|(f, w)| *f == 6 && !matches!(w, Wire::Bytes(ALL_VALID_ITEM)));
    if nulls {
        return Some("a page holds nulls: only a column with none is read as an attribute");
    }
    let vc = layout.iter().find_map(|(f, w)| match (f, w) {
        (3, Wire::Bytes(vc)) => Some(*vc),
        _ => None,
    })?;
    let packed = fields(vc).is_some_and(|c| matches!(c.as_slice(), [(5, _)]));
    (packed && !bit_packed(vc, bits))
        .then_some("a page's values are bit-packed at another width than the column's")
}

/// `FixedSizeList(dims) -> Flat(32 bits)`, with no buffer compression, and nothing more.
fn plain_floats(compression: &[u8], dims: u32) -> bool {
    let Some(fs) = fields(compression) else {
        return false;
    };
    let [(11, Wire::Bytes(fsl))] = fs.as_slice() else {
        return false;
    };
    let Some(fsl) = fields(fsl) else {
        return false;
    };
    let [(1, Wire::Varint(d)), (2, Wire::Bytes(items))] = fsl.as_slice() else {
        return false;
    };
    let Some(items) = fields(items) else {
        return false;
    };
    let [(1, Wire::Bytes(flat))] = items.as_slice() else {
        return false;
    };
    let Some(flat) = fields(flat) else {
        return false;
    };
    *d == u64::from(dims) && matches!(flat.as_slice(), [(1, Wire::Varint(32))])
}

/// The page's `PageLayout` message: `PageEncoding.direct(2).encoding(1)` is an `Any`.
fn layout<'a>(page: &[(u64, Wire<'a>)]) -> Result<Vec<(u64, Wire<'a>)>, LanceError> {
    let any = bytes(page, 4)
        .and_then(fields)
        .and_then(|e| bytes(&e, 2).and_then(fields))
        .and_then(|d| bytes(&d, 1).and_then(fields))
        .ok_or_else(|| LanceError::Unsupported("a page has no direct encoding".to_owned()))?;
    if bytes(&any, 1) != Some(LAYOUT_URL) {
        return refused("a page is not a Lance 2.1 page layout");
    }
    bytes(&any, 2)
        .and_then(fields)
        .ok_or(bad("page layout protobuf"))
}

/// The runs of rows of one 2.1 page of `n` rows of `shape`, read from the page's protobuf
/// `page` (and, for `MiniBlock`, its chunk index) in a file of `len` bytes.
pub(super) async fn pieces<S: BlobStore>(
    store: &S,
    key: &Key,
    page: &[(u64, Wire<'_>)],
    shape: Shape,
    n: u64,
    len: u64,
) -> Result<Vec<Piece>, LanceError> {
    let (offsets, sizes) = (
        packed(page, 1).ok_or(bad("page offsets"))?,
        packed(page, 2).ok_or(bad("page sizes"))?,
    );
    if offsets.len() != sizes.len()
        || offsets
            .iter()
            .zip(&sizes)
            .any(|(o, s)| !inside(*o, *s, len))
    {
        return Err(bad("a page's buffers run past the file"));
    }
    let row = shape.width();
    match layout(page)?.as_slice() {
        [(3, Wire::Bytes(full))] => {
            let fs = fields(full).ok_or(bad("page layout protobuf"))?;
            let [
                (3, Wire::Varint(bits)),
                (5, Wire::Varint(visible)),
                (6, Wire::Varint(items)),
                (7, Wire::Bytes(vc)),
                (8, Wire::Bytes(ALL_VALID_ITEM)),
            ] = fs.as_slice()
            else {
                return refused("a page's full-zip layout is not a null-free, uncompressed run");
            };
            if *bits != row * 8 || *visible != n || *items != n || !plain(vc, shape) {
                return refused("a page's full-zip layout is not a null-free, uncompressed run");
            }
            let ([offset], [size]) = (offsets.as_slice(), sizes.as_slice()) else {
                return refused("a full-zip page has more than one buffer");
            };
            if n.checked_mul(row) != Some(*size) {
                return Err(shape.size_error());
            }
            Ok(vec![Piece {
                rows: n,
                offset: *offset,
                packed: 0,
            }])
        }
        [(1, Wire::Bytes(mini))] => {
            let fs = fields(mini).ok_or(bad("page layout protobuf"))?;
            let bits = match shape {
                Shape::Scalar(bits) => Some(bits),
                Shape::Vector(_) => None,
            };
            if let Some(why) = bits.and_then(|b| scalar_refusal(&fs, b)) {
                return refused(why);
            }
            let [
                (3, Wire::Bytes(vc)),
                (6, Wire::Bytes(ALL_VALID_ITEM)),
                (7, Wire::Varint(1)),
                (9, Wire::Varint(items)),
            ] = fs.as_slice()
            else {
                return refused("a page's mini-block layout is not a null-free, uncompressed run");
            };
            // FastLanes packs 32- and 64-bit values here; any other width is refused.
            let packed = bits.is_some_and(|b| matches!(b, 32 | 64) && bit_packed(vc, b));
            if *items != n || !(packed || plain(vc, shape)) {
                return refused("a page's mini-block layout is not a null-free, uncompressed run");
            }
            let ([index_at, data_at], [index_len, data_len]) =
                (offsets.as_slice(), sizes.as_slice())
            else {
                return Err(bad("a mini-block page has not two buffers"));
            };
            // A longer index than any page may have is cut, and then fails to account for the rows.
            let raw = store
                .get_range(key, *index_at..*index_at + (*index_len).min(MAX_INDEX))
                .await?;
            chunks_of(&raw, n, row, *data_at, *data_len, packed)
        }
        _ => refused("a page layout is neither mini-block nor full-zip"),
    }
}

fn inside(at: u64, size: u64, len: u64) -> bool {
    at.checked_add(size).is_some_and(|e| e <= len)
}

/// The chunks a `MiniBlock` page's index describes, each as a run of rows: `data_at` is the
/// buffer of chunks, `data_len` its size. A `packed` page's chunks are bit-packed (M102): each
/// holds at most 1,024 rows in its width and at most a value's bits of every row, and is read
/// whole.
fn chunks_of(
    index: &[u8],
    n: u64,
    row: u64,
    data_at: u64,
    data_len: u64,
    packed: bool,
) -> Result<Vec<Piece>, LanceError> {
    let entries = index.chunks_exact(2);
    if !entries.remainder().is_empty() {
        return Err(bad("a page's chunk index is not a list of 16-bit entries"));
    }
    let (mut pieces, mut left, mut at) = (Vec::new(), n, data_at);
    let last = (index.len() / 2).saturating_sub(1);
    for (k, entry) in entries.enumerate() {
        let entry = u16::from_le_bytes(<[u8; 2]>::try_from(entry).map_err(|_| bad("chunk index"))?);
        let (words, log) = (u64::from(entry >> 4), u32::from(entry & 15));
        let values = if k == last {
            // The rest; a full last chunk names its size, a short one says 0.
            if log != 0 && 1u64 << log != left {
                return Err(bad("the last chunk's size disagrees with the page"));
            }
            left
        } else if log == 0 {
            return Err(bad("a chunk before the last declares no size"));
        } else {
            1u64 << log
        };
        if values == 0 {
            return Err(bad("a chunk holds no rows"));
        }
        let padded = if packed {
            // The width, then up to a value's bits of each of 1,024 values.
            let most = (row + row * PACKED_VALUES).next_multiple_of(WORD);
            if values > PACKED_VALUES || words * WORD < row || words * WORD > most {
                return Err(bad(
                    "a bit-packed chunk's size is not a width and its values",
                ));
            }
            words * WORD
        } else {
            let padded = values
                .checked_mul(row)
                .and_then(|b| b.checked_next_multiple_of(WORD))
                .ok_or(bad("a chunk's size"))?;
            if padded != words * WORD {
                return Err(bad("a chunk's size is not its rows padded to 8 bytes"));
            }
            padded
        };
        let start = at
            .checked_add(CHUNK_HEADER)
            .ok_or(bad("a chunk's offset"))?;
        pieces.push(if packed {
            Piece {
                rows: values,
                offset: at,
                packed: CHUNK_HEADER + padded,
            }
        } else {
            Piece {
                rows: values,
                offset: start,
                packed: 0,
            }
        });
        at = start.checked_add(padded).ok_or(bad("a chunk's offset"))?;
        left = left
            .checked_sub(values)
            .ok_or(bad("a chunk is past the page's rows"))?;
    }
    if left != 0 || at != data_at + data_len {
        return Err(bad("a page's chunks do not hold its rows and bytes"));
    }
    Ok(pieces)
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    reason = "assertions in tests are the reporting mechanism"
)]
mod tests {
    use super::{Piece, bit_packed, chunks_of, fields, plain_flat, plain_floats, scalar_refusal};

    /// A length-delimited protobuf field. Every body here is under 128 bytes.
    fn msg(field: u8, body: &[u8]) -> Vec<u8> {
        let mut out = vec![field << 3 | 2, u8::try_from(body.len()).unwrap_or(0)];
        out.extend_from_slice(body);
        out
    }

    fn varint(field: u8, v: u8) -> Vec<u8> {
        vec![field << 3, v]
    }

    /// `FixedSizeList(dims) -> Flat(bits)`, the compression a plain float32 page carries.
    fn compression(dims: u8, bits: u8) -> Vec<u8> {
        let flat = varint(1, bits);
        let items = msg(1, &flat);
        let fsl = [varint(1, dims), msg(2, &items)].concat();
        msg(11, &fsl)
    }

    /// Only plain float32s of the declared width are read as rows: anything else in the
    /// compression would be decoded as floats it is not, and answer queries wrongly.
    #[test]
    fn only_a_flat_32_bit_list_of_the_declared_width_is_plain() {
        assert!(plain_floats(&compression(4, 32), 4));
        let flat = varint(1, 32);
        let fsl = |items: &[u8]| msg(11, &[varint(1, 4), msg(2, items)].concat());
        for (what, bad) in [
            ("another width", compression(8, 32)),
            ("64-bit values", compression(4, 64)),
            ("malformed", vec![11 << 3 | 2, 5]),
            ("another encoding", msg(12, &[])),
            (
                "a second encoding",
                [compression(4, 32), msg(12, &[])].concat(),
            ),
            ("a malformed list", msg(11, &[1 << 3])),
            ("a list with no items", msg(11, &varint(1, 4))),
            (
                "a list with more",
                msg(
                    11,
                    &[varint(1, 4), msg(2, &msg(1, &flat)), varint(3, 1)].concat(),
                ),
            ),
            ("malformed items", fsl(&[1 << 3 | 2, 9])),
            (
                "items with more",
                fsl(&[msg(1, &flat), varint(2, 1)].concat()),
            ),
            ("items not flat", fsl(&msg(2, &flat))),
            ("a malformed flat", fsl(&msg(1, &[1 << 3]))),
            (
                "a flat with more",
                fsl(&msg(1, &[flat.clone(), varint(2, 1)].concat())),
            ),
        ] {
            assert!(!plain_floats(&bad, 4), "{what}");
        }
    }

    /// `InlineBitpacking(bits)`.
    fn packing(bits: u8) -> Vec<u8> {
        msg(5, &varint(1, bits))
    }

    #[test]
    fn only_inline_bitpacking_at_the_values_width_is_bit_packed() {
        assert!(bit_packed(&packing(32), 32));
        assert!(bit_packed(&packing(64), 64));
        for (what, bad) in [
            ("another width", packing(64)),
            ("flat", msg(1, &varint(1, 32))),
            ("a second encoding", [packing(32), msg(12, &[])].concat()),
            (
                "packing with more",
                msg(5, &[varint(1, 32), varint(2, 1)].concat()),
            ),
            ("malformed packing", msg(5, &[1 << 3])),
            ("malformed", vec![5 << 3 | 2, 5]),
        ] {
            assert!(!bit_packed(&bad, 32), "{what}");
        }
        // A scalar page packed at another width is refused by name; at its own, not here.
        let layout = |vc: &[u8]| msg(3, vc);
        let why = |vc: &[u8], bits| scalar_refusal(&fields(&layout(vc)).unwrap(), bits);
        assert!(why(&packing(16), 32).is_some_and(|w| w.contains("another width")));
        assert_eq!(why(&packing(32), 32), None);
        assert_eq!(why(&msg(1, &varint(1, 32)), 32), None);
    }

    /// A scalar is read flat only as `Flat(bits)` at its own width (M101): bit-packed values
    /// read as flat ones would be other numbers.
    #[test]
    fn only_flat_at_the_values_width_is_a_plain_scalar() {
        let flat = |bits| msg(1, &varint(1, bits));
        assert!(plain_flat(&flat(32), 32));
        assert!(plain_flat(&flat(64), 64));
        for (what, bad) in [
            ("another width", flat(64)),
            ("bit-packed", packing(32)),
            ("a second encoding", [flat(32), msg(12, &[])].concat()),
            (
                "a flat with more",
                msg(1, &[varint(1, 32), varint(2, 1)].concat()),
            ),
            ("a malformed flat", msg(1, &[1 << 3])),
            ("malformed", vec![1 << 3 | 2, 5]),
        ] {
            assert!(!plain_flat(&bad, 32), "{what}");
        }
    }

    /// `(rows, offset, packed)` of each piece.
    fn runs(pieces: &[Piece]) -> Vec<(u64, u64, u64)> {
        pieces
            .iter()
            .map(|p| (p.rows, p.offset, p.packed))
            .collect()
    }

    /// A chunk index entry of `words` 8-byte words after the header, for `2^log` values.
    fn entry(words: u16, log: u16) -> [u8; 2] {
        (words << 4 | log).to_le_bytes()
    }

    #[test]
    fn a_bit_packed_chunk_is_a_width_and_at_most_its_values_bits() {
        // A full chunk of 1,024 rows in 2 words, then 500 in 3, of 4-byte values at byte 100.
        let index = [entry(2, 10), entry(3, 0)].concat();
        let got = chunks_of(&index, 1524, 4, 100, 24 + 32, true).unwrap();
        assert_eq!(runs(&got), [(1024, 100, 24), (500, 124, 32)]);
        // Flat chunks start past their header and hold only their rows.
        let flat = chunks_of(&[entry(1, 1), entry(1, 0)].concat(), 3, 4, 100, 32, false);
        assert_eq!(runs(&flat.unwrap()), [(2, 108, 0), (1, 124, 0)]);
        // An 8-byte value: its width fills one word, and all 64 bits of 1,024 values fill 1,025.
        for words in [1, 1025] {
            let one = chunks_of(&entry(words, 0), 7, 8, 0, 8 * (u64::from(words) + 1), true);
            assert!(one.is_ok(), "{words} words");
        }
        for (what, index, n) in [
            ("no room for the width", entry(0, 0), 7),
            ("more than every bit", entry(1026, 0), 7),
            ("more than 1,024 values", entry(9, 11), 2048),
        ] {
            let words = u64::from(u16::from_le_bytes(index) >> 4);
            let err = chunks_of(&index, n, 8, 0, 8 * (words + 1), true);
            assert!(err.is_err(), "{what}");
        }
    }
}
