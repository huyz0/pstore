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

use super::{LanceError, Wire, bytes, fields, packed};
use pstore_blob::{BlobStore, Key};

const LAYOUT_URL: &[u8] = b"/lance.encodings21.PageLayout";
/// `RepDefLayer` of an item with no nulls, as the one entry of a layout's `layers`.
const ALL_VALID_ITEM: &[u8] = &[1];
/// A chunk's header: two `u16`s (no levels, the rows' size) and padding to 8 bytes.
const CHUNK_HEADER: u64 = 8;
const WORD: u64 = 8;
/// The most bytes of chunk index read for a page: 2 a chunk, as many chunks as the extent cap.
const MAX_INDEX: u64 = 2 << 22;

/// A run of rows of one page.
pub(super) struct Piece {
    pub rows: u64,
    pub offset: u64,
}

fn bad(why: &'static str) -> LanceError {
    LanceError::Malformed(why)
}

fn refused<T>(why: &str) -> Result<T, LanceError> {
    Err(LanceError::Unsupported(why.to_owned()))
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

/// The runs of rows of one 2.1 page of `n` rows of `dims` float32s, read from the page's
/// protobuf `page` (and, for `MiniBlock`, its chunk index) in a file of `len` bytes.
pub(super) async fn pieces<S: BlobStore>(
    store: &S,
    key: &Key,
    page: &[(u64, Wire<'_>)],
    dims: u32,
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
    let row = u64::from(dims) * 4;
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
            if *bits != row * 8 || *visible != n || *items != n || !plain_floats(vc, dims) {
                return refused("a page's full-zip layout is not a null-free, uncompressed run");
            }
            let ([offset], [size]) = (offsets.as_slice(), sizes.as_slice()) else {
                return refused("a full-zip page has more than one buffer");
            };
            if n.checked_mul(row) != Some(*size) {
                return Err(bad("a page's size is not rows x dims x 4"));
            }
            Ok(vec![Piece {
                rows: n,
                offset: *offset,
            }])
        }
        [(1, Wire::Bytes(mini))] => {
            let fs = fields(mini).ok_or(bad("page layout protobuf"))?;
            let [
                (3, Wire::Bytes(vc)),
                (6, Wire::Bytes(ALL_VALID_ITEM)),
                (7, Wire::Varint(1)),
                (9, Wire::Varint(items)),
            ] = fs.as_slice()
            else {
                return refused("a page's mini-block layout is not a null-free, uncompressed run");
            };
            if *items != n || !plain_floats(vc, dims) {
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
            chunks_of(&raw, n, row, *data_at, *data_len)
        }
        _ => refused("a page layout is neither mini-block nor full-zip"),
    }
}

fn inside(at: u64, size: u64, len: u64) -> bool {
    at.checked_add(size).is_some_and(|e| e <= len)
}

/// The chunks a `MiniBlock` page's index describes, each as a run of rows: `data_at` is the
/// buffer of chunks, `data_len` its size.
fn chunks_of(
    index: &[u8],
    n: u64,
    row: u64,
    data_at: u64,
    data_len: u64,
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
        let padded = values
            .checked_mul(row)
            .and_then(|b| b.checked_next_multiple_of(WORD))
            .ok_or(bad("a chunk's size"))?;
        if padded != words * WORD {
            return Err(bad("a chunk's size is not its rows padded to 8 bytes"));
        }
        let start = at
            .checked_add(CHUNK_HEADER)
            .ok_or(bad("a chunk's offset"))?;
        pieces.push(Piece {
            rows: values,
            offset: start,
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
