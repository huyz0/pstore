//! A `list<float>` column whose lists all have the same length (M91, backlog row 74).
//!
//! The vector columns real pipelines write as Parquet are `list<float>`: a top-level group holding
//! a repeated group holding a `float` leaf, with a repetition and a definition level a value. If
//! every list has the same length `D` and no list or item is null, a page's values are
//! `rows × D` contiguous `f32` behind two level streams, which is the shape [`super::locate`]
//! already reads for a fixed-length column. The levels are read once and checked; anything else is
//! refused with the reason.

use super::{
    CODEC_NONE, ChunkMeta, ENC_PLAIN, ENC_RLE, Footer, HEADER_WINDOW, Located, OPTIONAL, PAGE_V1,
    PAGE_V2, ParquetError, REQUIRED, page_header, unsupported,
};
use crate::external::{Extent, within_extent_cap};
use pstore_blob::{BlobStore, Key};

fn bad(why: &'static str) -> ParquetError {
    ParquetError::Malformed(why)
}

const REPEATED: i64 = 2;
const TYPE_FLOAT: i64 = 4;
/// The most elements one page may claim: what the fixed-length reader allows, in floats.
const MAX_ELEMENTS: u64 = 64 << 20;

/// Calls `f` with each of the `n` levels in `levels` (Parquet's RLE / bit-packed hybrid at
/// `width` bits, no length prefix), in order. `true` if the stream is well-formed, holds exactly
/// `n` levels with nothing after, and `f` never returned `false`.
fn each_level(levels: &[u8], n: u64, width: u32, mut f: impl FnMut(u32) -> bool) -> bool {
    if width == 0 || width > 8 {
        return false;
    }
    let (mut at, mut seen) = (0usize, 0u64);
    while seen < n {
        let (mut header, mut shift) = (0u64, 0u32);
        loop {
            let Some(b) = levels.get(at) else {
                return false;
            };
            at += 1;
            let Some(part) = u64::from(b & 0x7F).checked_shl(shift) else {
                return false;
            };
            header |= part;
            if b & 0x80 == 0 {
                break;
            }
            shift += 7;
            if shift > 35 {
                return false;
            }
        }
        let units = header >> 1;
        if units == 0 {
            return false;
        }
        if header & 1 == 1 {
            // `units` groups of eight values, `width` bits each, least significant bit first.
            let bytes = units.saturating_mul(u64::from(width));
            let Some(packed) = usize::try_from(bytes)
                .ok()
                .and_then(|b| levels.get(at..at.checked_add(b)?))
            else {
                return false;
            };
            at += packed.len();
            let take = units.saturating_mul(8).min(n - seen);
            for k in 0..take {
                let mut v = 0u32;
                for bit in 0..width {
                    let pos = k * u64::from(width) + u64::from(bit);
                    let Some(byte) = usize::try_from(pos / 8).ok().and_then(|p| packed.get(p))
                    else {
                        return false;
                    };
                    v |= u32::from(byte >> (pos % 8) & 1) << bit;
                }
                if !f(v) {
                    return false;
                }
            }
            seen += take;
        } else {
            // One value, repeated, in `ceil(width / 8)` = 1 byte.
            let Some(&v) = levels.get(at) else {
                return false;
            };
            at += 1;
            let take = units.min(n - seen);
            for _ in 0..take {
                if !f(u32::from(v)) {
                    return false;
                }
            }
            seen += take;
        }
    }
    at == levels.len()
}

/// The number of rows a page's repetition levels describe and the length of their lists:
/// `Some((rows, length))` if the page starts a row, holds `n` levels, and every list has the same
/// length; `None` otherwise (a list that spans the page's end counts as short).
fn row_shape(levels: &[u8], n: u64) -> Option<(u64, u64)> {
    let (mut rows, mut current, mut length) = (0u64, 0u64, None::<u64>);
    let mut ok = true;
    let well_formed = each_level(levels, n, 1, |r| {
        match r {
            0 => {
                if rows > 0 {
                    if length.is_some_and(|l| l != current) {
                        ok = false;
                    }
                    length = Some(current);
                }
                rows += 1;
                current = 1;
            }
            1 => {
                if rows == 0 {
                    ok = false;
                }
                current += 1;
            }
            _ => ok = false,
        }
        ok
    });
    if !well_formed || !ok || rows == 0 {
        return None;
    }
    // The last row closes the page.
    if length.is_some_and(|l| l != current) {
        return None;
    }
    Some((rows, current))
}

/// Whether every one of the `n` definition levels in `levels` is `max` (nothing is null).
fn all_defined(levels: &[u8], n: u64, width: u32, max: u32) -> bool {
    each_level(levels, n, width, |d| d == max)
}

/// The bits needed for levels up to `max`.
fn bit_width(max: u32) -> u32 {
    u32::BITS - max.leading_zeros()
}

/// Locates a `list<float>` column; `top` is the schema index of its top-level group.
pub(super) async fn locate_list<S: BlobStore>(
    store: &S,
    key: &Key,
    len: u64,
    f: &Footer,
    column: &str,
    top: usize,
    elements: usize,
) -> Result<Located, ParquetError> {
    let (outer, list, leaf) = match (
        f.schema.get(top),
        f.schema.get(top + 1),
        f.schema.get(top + 2),
    ) {
        (Some(o), Some(l), Some(e)) if elements == 3 => (o, l, e),
        _ => {
            return unsupported("the column is nested deeper than a list of values");
        }
    };
    // A subtree of three elements whose top has one child is a chain: top, list, leaf.
    if outer.children != 1 {
        return unsupported("the column is a group that is not a list of values");
    }
    if list.repetition != Some(REPEATED) {
        return unsupported("the column's inner group is not a repeated list");
    }
    if leaf.ty != Some(TYPE_FLOAT) {
        return unsupported("the list does not hold float32 values");
    }
    let nullable = |e: &super::Leaf| match e.repetition {
        Some(REQUIRED) => Ok(0u32),
        Some(OPTIONAL) => Ok(1),
        _ => unsupported("the list or its items are repeated"),
    };
    let max_def = nullable(outer)? + 1 + nullable(leaf)?;
    let def_width = bit_width(max_def);

    let mut extents: Vec<Extent> = Vec::new();
    let (mut first_row, mut dims) = (0u64, None::<u64>);
    for group in &f.groups {
        let (m, rows) = group
            .as_ref()
            .ok_or_else(|| ParquetError::Unsupported(format!("a row group lacks {column:?}")))?;
        let rows = u64::try_from(*rows).map_err(|_| bad("negative rows"))?;
        chunk_ok(m)?;
        let start = u64::try_from(m.data_page_offset.ok_or(bad("no data page offset"))?)
            .map_err(|_| bad("negative offset"))?;
        let end = start
            .checked_add(u64::try_from(m.total_compressed).map_err(|_| bad("negative size"))?)
            .filter(|e| *e <= len)
            .ok_or(bad("a chunk ends past the file"))?;
        let want = u64::try_from(m.num_values).map_err(|_| bad("negative count"))?;
        let (mut at, mut got_elements, mut got_rows) = (start, 0u64, 0u64);
        while got_elements < want {
            if at >= end {
                return Err(bad("pages end before the chunk's values"));
            }
            let head = store
                .get_range(key, at..at.saturating_add(HEADER_WINDOW).min(len))
                .await?;
            let h = page_header(&head).ok_or(bad("page header"))?;
            if h.kind != PAGE_V1 && h.kind != PAGE_V2 {
                return unsupported("a list column has a dictionary page");
            }
            if h.encoding != ENC_PLAIN {
                return unsupported("a data page is not PLAIN-encoded");
            }
            let n = page_values(h.values)?;
            let on_disk = u64::try_from(h.compressed).map_err(|_| bad("page size"))?;
            let body = at + h.header_len as u64;
            if body.checked_add(on_disk).is_none_or(|e| e > end) {
                return Err(bad("a page runs past its chunk"));
            }
            // The two level streams, v1 behind 4-byte lengths inside the body, v2 before the
            // values with their lengths in the header.
            let (rep_at, rep_len, def_at, def_len) = if let Some(v) = &h.v2 {
                let rep = u64::try_from(v.repetition_len).map_err(|_| bad("level length"))?;
                let def = u64::try_from(v.definition_len).map_err(|_| bad("level length"))?;
                (body, rep, body + rep, def)
            } else {
                let rep = u32_at(store, key, body).await?;
                let def_at = body + 4 + rep;
                let def = u32_at(store, key, def_at).await?;
                (body + 4, rep, def_at + 4, def)
            };
            let lead = rep_len
                .checked_add(def_len)
                .and_then(|l| l.checked_add(if h.v2.is_some() { 0 } else { 8 }))
                .filter(|l| *l <= on_disk)
                .ok_or(bad("definition or repetition levels run past their page"))?;
            let rep = store.get_range(key, rep_at..rep_at + rep_len).await?;
            let def = store.get_range(key, def_at..def_at + def_len).await?;
            if !all_defined(&def, n, def_width, max_def) {
                return unsupported("a list column holds null lists or null items");
            }
            let (page_rows, length) = row_shape(&rep, n).ok_or(ParquetError::Unsupported(
                "a page's lists are not all one length, or a list spans pages".to_owned(),
            ))?;
            match dims {
                None => dims = Some(length),
                Some(d) if d != length => {
                    return unsupported("the lists do not all have the same length");
                }
                Some(_) => {}
            }
            // A raw page is exactly its levels and its floats.
            let sized = n.checked_mul(4) == Some(on_disk - lead);
            if !sized {
                return unsupported("a list page is compressed or carries another layout");
            }
            extents.push(Extent {
                first_row: first_row + got_rows,
                rows: page_rows,
                offset: body + lead,
                compressed: 0,
                codec: 0,
                plain_len: 0,
                dict: None,
                object: 0,
            });
            if !within_extent_cap(extents.len()) {
                return unsupported("the column splits into too many pages");
            }
            got_elements += n;
            got_rows += page_rows;
            at = body + on_disk;
        }
        if got_elements != want {
            return Err(bad("pages overshoot the chunk's values"));
        }
        if got_rows != rows {
            return Err(bad("pages do not hold the row group's rows"));
        }
        first_row += rows;
    }
    let dims = dims
        .and_then(|d| u32::try_from(d).ok())
        .ok_or(bad("no lists"))?;
    Ok(Located {
        len,
        dims,
        extents,
        rows: first_row,
    })
}

/// A page's value count, which must be positive and no more than a page may claim.
fn page_values(values: i64) -> Result<u64, ParquetError> {
    u64::try_from(values)
        .ok()
        .filter(|n| (1..=MAX_ELEMENTS).contains(n))
        .ok_or(bad("a page claims no values or too many"))
}

/// A chunk of a list column is plain, uncompressed, and has no dictionary.
fn chunk_ok(m: &ChunkMeta) -> Result<(), ParquetError> {
    if m.ty != Some(TYPE_FLOAT) {
        return unsupported("a chunk's type is not float");
    }
    if m.codec != Some(CODEC_NONE) {
        return unsupported("a list column's pages are compressed");
    }
    if m.dictionary_page_offset.is_some_and(|d| d > 0) {
        return unsupported("a list column has a dictionary page");
    }
    if m.encodings.iter().any(|e| *e != ENC_PLAIN && *e != ENC_RLE) {
        return unsupported("a list column uses an encoding other than PLAIN");
    }
    Ok(())
}

/// A little-endian `u32` at `at`, as a length; the caller checks it against the page.
async fn u32_at<S: BlobStore>(store: &S, key: &Key, at: u64) -> Result<u64, ParquetError> {
    let raw = store.get_range(key, at..at + 4).await?;
    let n = u32::from_le_bytes(<[u8; 4]>::try_from(raw.as_ref()).map_err(|_| bad("level length"))?);
    Ok(u64::from(n))
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]
mod tests {
    use super::*;

    fn levels(width: u32, f: impl FnMut(u32) -> bool, bytes: &[u8], n: u64) -> bool {
        each_level(bytes, n, width, f)
    }

    fn collect(bytes: &[u8], n: u64, width: u32) -> Option<Vec<u32>> {
        let mut out = Vec::new();
        levels(
            width,
            |v| {
                out.push(v);
                true
            },
            bytes,
            n,
        )
        .then_some(out)
    }

    #[test]
    fn a_page_claims_between_one_and_the_cap_values() {
        assert!(page_values(0).is_err());
        assert!(page_values(-1).is_err());
        assert_eq!(page_values(1).unwrap(), 1);
        assert_eq!(page_values(MAX_ELEMENTS as i64).unwrap(), MAX_ELEMENTS);
        assert!(page_values(MAX_ELEMENTS as i64 + 1).is_err());
    }

    #[test]
    fn a_dictionary_offset_of_zero_means_none() {
        let ok = || ChunkMeta {
            ty: Some(TYPE_FLOAT),
            codec: Some(CODEC_NONE),
            encodings: vec![ENC_PLAIN, ENC_RLE],
            ..ChunkMeta::default()
        };
        assert!(chunk_ok(&ok()).is_ok());
        let mut zero = ok();
        zero.dictionary_page_offset = Some(0);
        assert!(chunk_ok(&zero).is_ok());
        let mut some = ok();
        some.dictionary_page_offset = Some(1);
        assert!(chunk_ok(&some).is_err());
    }

    #[test]
    fn bit_widths_are_those_of_the_largest_level() {
        assert_eq!(bit_width(1), 1);
        assert_eq!(bit_width(2), 2);
        assert_eq!(bit_width(3), 2);
        assert_eq!(bit_width(4), 3);
    }

    #[test]
    fn levels_decode_in_rle_and_bit_packed_runs() {
        // RLE: 5 x 2 at width 2 (header 5 << 1); then a packed group of eight 1-bit levels.
        assert_eq!(collect(&[5 << 1, 2], 5, 2), Some(vec![2; 5]));
        assert_eq!(
            collect(&[3, 0b1010_0101], 8, 1),
            Some(vec![1, 0, 1, 0, 0, 1, 0, 1]),
            "least significant bit first"
        );
        // Two-bit values packed: 0,1,2,3 repeated twice (1 group of 8 = 2 bytes).
        assert_eq!(
            collect(&[3, 0b1110_0100, 0b1110_0100], 8, 2),
            Some(vec![0, 1, 2, 3, 0, 1, 2, 3])
        );
        // Mixed, cut by `n`, and padding bits past `n` are not read.
        assert_eq!(
            collect(&[2 << 1, 1, 3, 0b0000_0110], 6, 1),
            Some(vec![1, 1, 0, 1, 1, 0])
        );
        // Eight bits is the widest; a run cut by `n` after earlier values is cut at `n`.
        assert_eq!(collect(&[5 << 1, 200], 5, 8), Some(vec![200; 5]));
        assert_eq!(
            collect(&[2 << 1, 1, 50 << 1, 2], 5, 2),
            Some(vec![1, 1, 2, 2, 2])
        );
        // A run header may take six bytes (padded) but not seven.
        assert_eq!(
            collect(&[0x94, 0x80, 0x80, 0x80, 0x80, 0x00, 1], 10, 1),
            Some(vec![1; 10])
        );
        assert_eq!(
            collect(&[0x94, 0x80, 0x80, 0x80, 0x80, 0x80, 0x00, 1], 10, 1),
            None
        );
        // Refusals: trailing bytes, too few, a zero run, a header that never ends, widths out of
        // range, a run without its value, a packed run without its bytes.
        assert_eq!(collect(&[5 << 1, 1, 0], 5, 1), None);
        assert_eq!(collect(&[5 << 1, 1], 6, 1), None);
        assert_eq!(collect(&[0, 1], 1, 1), None);
        assert_eq!(collect(&[0x80], 1, 1), None);
        assert_eq!(collect(&[5 << 1, 1], 5, 0), None);
        assert_eq!(collect(&[5 << 1, 1], 5, 9), None);
        assert_eq!(collect(&[5 << 1], 5, 1), None);
        assert_eq!(collect(&[3], 8, 1), None);
        // `f` returning false stops it.
        assert!(!each_level(&[5 << 1, 1], 5, 1, |_| false));
    }

    #[test]
    fn repetition_levels_give_the_row_length() {
        // 3 rows of 4: 0 1 1 1 repeated, as a packed group of 8 plus a group of 8 (needs 12 bits).
        let rows3x4 = [5, 0b1110_1110, 0b1110_1110];
        assert_eq!(row_shape(&rows3x4, 12), Some((3, 4)));
        // One row of 5, written as an RLE 0 then an RLE of four 1s.
        assert_eq!(row_shape(&[1 << 1, 0, 4 << 1, 1], 5), Some((1, 5)));
        // Rows of 2 and 3: the lists differ.
        assert_eq!(
            row_shape(&[1 << 1, 0, 1 << 1, 1, 1 << 1, 0, 2 << 1, 1], 7),
            None
        );
        // A page that does not begin a row (starts with a 1), and a level above 1.
        assert_eq!(row_shape(&[3 << 1, 1], 3), None);
        assert_eq!(row_shape(&[2 << 1, 2], 2), None);
        // A last row shorter than the others spans the page's end.
        assert_eq!(
            row_shape(&[1 << 1, 0, 2 << 1, 1, 1 << 1, 0, 1 << 1, 1], 5),
            None
        );
        // A stream that breaks after a row has begun is refused, not truncated into a shape.
        assert_eq!(row_shape(&[1 << 1, 0, 1 << 1], 2), None);
        // Nothing, and a malformed stream.
        assert_eq!(row_shape(&[], 0), None);
        assert_eq!(row_shape(&[0x80], 1), None);
    }

    #[test]
    fn definition_levels_must_all_be_the_maximum() {
        assert!(all_defined(&[6 << 1, 3], 6, 2, 3));
        assert!(
            !all_defined(&[6 << 1, 2], 6, 2, 3),
            "a level below the maximum"
        );
        assert!(
            !all_defined(&[3 << 1, 3, 3 << 1, 2], 6, 2, 3),
            "a null later on"
        );
        assert!(
            !all_defined(&[6 << 1, 3], 7, 2, 3),
            "fewer levels than values"
        );
    }
}
