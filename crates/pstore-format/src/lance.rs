//! Finding a vector column in a Lance dataset (M82, backlog row 73).
//!
//! A Lance 2.0 dataset (manifest `DataFile` version 2.0, data-file footer 0.3) names its data files in a **manifest** (protobuf); a data file ends in
//! a footer that leads to each column's **page table** (protobuf). A fixed-size-list `float32`
//! column with no nulls is stored as raw little-endian floats, so each page is one contiguous
//! run, exactly as an uncompressed Parquet page is.
//!
//! Only that shape qualifies, and every other is refused **with the reason**: a dataset split over
//! several data files (an [`ExternalColumn`](crate::external::ExternalColumn) addresses one
//! object), deleted rows (a row number would no longer be a position), a newer file format, a
//! column with nulls or another encoding. Silently reading one as floats would return noise.
//!
//! The manifest and the page tables are untrusted bytes: the reader is bounded (depth, sizes,
//! offsets checked against the object) and returns `None` for anything it does not understand.

use crate::external::Extent;
use crate::parquet::Located;
use pstore_blob::{BlobError, BlobStore, Key};

/// Why a Lance column could not be located.
#[derive(Debug, thiserror::Error)]
pub enum LanceError {
    /// Not a Lance manifest or data file, or its metadata is damaged.
    #[error("not a readable lance dataset: {0}")]
    Malformed(&'static str),
    /// A well-formed dataset whose column a ranged read cannot reach.
    #[error("column cannot be read in place: {0}")]
    Unsupported(String),
    /// The store failed.
    #[error("lance object: {0}")]
    Blob(#[from] BlobError),
}

fn unsupported<T>(why: impl Into<String>) -> Result<T, LanceError> {
    Err(LanceError::Unsupported(why.into()))
}

/// The most manifest bytes read: a manifest names every fragment, so it is bounded.
const MAX_MANIFEST: u64 = 8 << 20;
/// The most column-metadata bytes read.
const MAX_COLUMN_META: u64 = 8 << 20;
const MAGIC: &[u8; 4] = b"LANC";
mod v21;

const ENCODING_URL: &[u8] = b"/lance.encodings.ArrayEncoding";

/// One protobuf field's value. Fixed-width values are skipped, as nothing here reads one.
enum Wire<'a> {
    Varint(u64),
    Bytes(&'a [u8]),
}

/// A protobuf message's fields, in order, or `None` if it is malformed.
fn fields(mut b: &[u8]) -> Option<Vec<(u64, Wire<'_>)>> {
    let mut out = Vec::new();
    while !b.is_empty() {
        let (tag, rest) = varint(b)?;
        b = rest;
        let field = tag >> 3;
        if field == 0 {
            return None;
        }
        match tag & 7 {
            0 => {
                let (v, rest) = varint(b)?;
                b = rest;
                out.push((field, Wire::Varint(v)));
            }
            1 => b = b.get(8..)?,
            2 => {
                let (len, rest) = varint(b)?;
                let (body, rest) = rest.split_at_checked(usize::try_from(len).ok()?)?;
                b = rest;
                out.push((field, Wire::Bytes(body)));
            }
            5 => b = b.get(4..)?,
            _ => return None,
        }
    }
    Some(out)
}

fn varint(b: &[u8]) -> Option<(u64, &[u8])> {
    let mut v = 0u64;
    for (i, byte) in b.iter().take(10).enumerate() {
        v |= u64::from(byte & 0x7F).checked_shl(u32::try_from(i).ok()?.checked_mul(7)?)?;
        if byte & 0x80 == 0 {
            return Some((v, b.get(i + 1..)?));
        }
    }
    None
}

/// A packed run of varints (`repeated uint64`), or one varint written unpacked.
fn packed(fs: &[(u64, Wire<'_>)], field: u64) -> Option<Vec<u64>> {
    let mut out = Vec::new();
    for (f, w) in fs {
        if *f != field {
            continue;
        }
        match w {
            Wire::Varint(v) => out.push(*v),
            Wire::Bytes(run) => {
                let mut b = *run;
                while !b.is_empty() {
                    let (v, rest) = varint(b)?;
                    out.push(v);
                    b = rest;
                }
            }
        }
    }
    Some(out)
}

fn bytes<'a>(fs: &[(u64, Wire<'a>)], field: u64) -> Option<&'a [u8]> {
    fs.iter().find_map(|(f, w)| match w {
        Wire::Bytes(b) if *f == field => Some(*b),
        _ => None,
    })
}

fn number(fs: &[(u64, Wire<'_>)], field: u64) -> u64 {
    fs.iter()
        .find_map(|(f, w)| match w {
            Wire::Varint(v) if *f == field => Some(*v),
            _ => None,
        })
        .unwrap_or(0)
}

/// The manifest of the version a dataset's `_versions/latest_version_hint.json` names (M89).
///
/// ⚠️ **A hint, not the truth**: Lance itself treats the file as an optimisation and lists
/// `_versions/` to be sure, and a LIST is forbidden on a read path here. So this is the newest
/// version *the hint names*, which a writer that has not refreshed it can leave one behind; a
/// version older than that is still a consistent snapshot. A hint naming a manifest that is gone
/// (garbage-collected) is an error, never a fallback.
///
/// # Errors
/// [`LanceError::Malformed`] for a hint that is not `{"version": N}` or names no manifest; a
/// store failure (including a missing hint).
pub async fn latest_manifest<S: BlobStore>(store: &S, dataset: &str) -> Result<Key, LanceError> {
    let bad = LanceError::Malformed;
    let dir = dataset.trim_end_matches('/');
    let hint = Key::new(if dir.is_empty() {
        "_versions/latest_version_hint.json".to_owned()
    } else {
        format!("{dir}/_versions/latest_version_hint.json")
    });
    let len = store.head(&hint).await?;
    if len == 0 || len > MAX_HINT {
        return Err(bad("the version hint is empty or too large"));
    }
    let raw = store.get_range(&hint, 0..len).await?;
    let text = std::str::from_utf8(&raw).map_err(|_| bad("the version hint is not text"))?;
    let version = parse_version(text).ok_or(bad("the version hint is not {\"version\": N}"))?;
    // V2 naming counts down from the largest u64 so a listing sorts newest first.
    let name = u64::MAX - version;
    let manifest = Key::new(if dir.is_empty() {
        format!("_versions/{name}.manifest")
    } else {
        format!("{dir}/_versions/{name}.manifest")
    });
    store.head(&manifest).await?;
    Ok(manifest)
}

/// The most hint bytes read.
const MAX_HINT: u64 = 1024;

/// `N` of exactly `{"version": N}` (whitespace allowed), where `N` is a positive integer.
fn parse_version(text: &str) -> Option<u64> {
    let inner = text.trim().strip_prefix('{')?.strip_suffix('}')?.trim();
    let rest = inner
        .strip_prefix("\"version\"")?
        .trim_start()
        .strip_prefix(':')?;
    let digits = rest.trim();
    // `parse` accepts a leading `+`; only digits are a version here. (An empty run fails to parse.)
    if !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse::<u64>().ok().filter(|v| *v > 0)
}

/// The data file a dataset's column lives in, and its pages.
///
/// `manifest` is the key of a `_versions/<n>.manifest` object; the data file is resolved as
/// `<dataset>/data/<path>`.
///
/// # Errors
/// [`LanceError::Malformed`] for damaged metadata; [`LanceError::Unsupported`], naming the
/// reason, for a column a ranged read cannot reach; a store failure.
pub async fn locate<S: BlobStore>(
    store: &S,
    manifest: &Key,
    column: &str,
) -> Result<Dataset, LanceError> {
    let bad = LanceError::Malformed;
    let total = store.head(manifest).await?;
    if !(16..=MAX_MANIFEST).contains(&total) {
        return Err(bad("the manifest is empty or too large"));
    }
    let raw = store.get_range(manifest, 0..total).await?;
    let foot = raw.get(raw.len() - 16..).ok_or(bad("short manifest"))?;
    if foot.get(12..) != Some(MAGIC.as_slice()) {
        return Err(bad("no LANC magic on the manifest"));
    }
    let at = foot
        .get(..8)
        .and_then(|b| <[u8; 8]>::try_from(b).ok())
        .map(u64::from_le_bytes)
        .and_then(|p| usize::try_from(p).ok())
        .ok_or(bad("manifest position"))?;
    let len = raw
        .get(at..at.saturating_add(4))
        .and_then(|b| <[u8; 4]>::try_from(b).ok())
        .map(u32::from_le_bytes)
        .and_then(|l| usize::try_from(l).ok())
        .ok_or(bad("manifest length"))?;
    let body = at
        .checked_add(4)
        .and_then(|s| raw.get(s..s.checked_add(len)?))
        .ok_or(bad("the manifest runs past the object"))?;
    let m = fields(body).ok_or(bad("manifest protobuf"))?;

    // The leaf: a top-level field of this name (parent -1), a float32 fixed-size list.
    let mut leaf = None;
    for (f, w) in &m {
        let (1, Wire::Bytes(fb)) = (*f, w) else {
            continue;
        };
        let fs = fields(fb).ok_or(bad("field protobuf"))?;
        if bytes(&fs, 2) == Some(column.as_bytes()) && number(&fs, 4) == u64::MAX {
            leaf = Some((number(&fs, 3), bytes(&fs, 5).unwrap_or_default().to_vec()));
        }
    }
    let Some((field_id, logical)) = leaf else {
        return unsupported(format!("no top-level column named {column:?}"));
    };
    let dims: u32 = std::str::from_utf8(&logical)
        .ok()
        .and_then(|s| s.strip_prefix("fixed_size_list:float:"))
        .and_then(|d| d.parse().ok())
        .filter(|d| *d > 0)
        .ok_or_else(|| {
            LanceError::Unsupported(format!(
                "the column is {:?}, not a float32 fixed-size list",
                String::from_utf8_lossy(&logical)
            ))
        })?;

    // <dataset>/_versions/<n>.manifest  ->  <dataset>/data/<path>
    let key = manifest.as_str();
    let dataset = key
        .rsplit_once('/')
        .and_then(|(dir, _)| {
            dir.strip_suffix("/_versions")
                .or_else(|| (dir == "_versions").then_some(""))
        })
        .ok_or(bad("the manifest is not under _versions/"))?;
    let under = |dir: &str, name: &str| {
        if dataset.is_empty() {
            Key::new(format!("{dir}/{name}"))
        } else {
            Key::new(format!("{dataset}/{dir}/{name}"))
        }
    };

    // Every fragment holds the column in exactly one data file (M87).
    let fragments: Vec<_> = m
        .iter()
        .filter_map(|(f, w)| match w {
            Wire::Bytes(b) if *f == 2 => Some(*b),
            _ => None,
        })
        .collect();
    if fragments.is_empty() {
        return unsupported("the dataset has no fragments");
    }
    let (mut objects, mut extents, mut base) = (Vec::new(), Vec::new(), 0u64);
    for fragment in fragments {
        let frag = fields(fragment).ok_or(bad("fragment protobuf"))?;
        let rows = number(&frag, 4);
        let mut file = None;
        for (f, w) in &frag {
            let (2, Wire::Bytes(db)) = (*f, w) else {
                continue;
            };
            let df = fields(db).ok_or(bad("data file protobuf"))?;
            let ids = packed(&df, 2).ok_or(bad("data file fields"))?;
            let Some(slot) = ids.iter().position(|i| *i == field_id) else {
                continue;
            };
            if file.is_some() {
                return unsupported("the column is in more than one data file of a fragment");
            }
            let cols = packed(&df, 3).ok_or(bad("data file columns"))?;
            let col = cols
                .get(slot)
                .copied()
                .ok_or(bad("no column for the field"))?;
            let path = bytes(&df, 1).ok_or(bad("data file path"))?;
            file = Some((
                path.to_vec(),
                col,
                number(&df, 4),
                number(&df, 5),
                number(&df, 6),
            ));
        }
        let Some((path, col, major, minor, size)) = file else {
            return unsupported("a fragment has no data file holding the column");
        };
        if (major, minor) != (2, 0) && (major, minor) != (2, 1) {
            return unsupported(format!(
                "the data file is format {major}.{minor}, and only 2.0 and 2.1 are read in place"
            ));
        }
        let path = std::str::from_utf8(&path).map_err(|_| bad("data file path"))?;
        if path.is_empty() || path.contains("..") || path.starts_with('/') {
            return Err(bad("data file path"));
        }
        let data = under("data", path);
        let located = locate_file(
            store,
            &data,
            usize::try_from(col).map_err(|_| bad("column"))?,
            dims,
            rows,
            size,
            minor == 1,
        )
        .await?;
        // The rows a deletion file removes stay in the numbering and leave the answer: gaps,
        // as a null's are (M88).
        let deleted = match bytes(&frag, 3) {
            Some(d) => {
                let d = fields(d).ok_or(bad("deletion file protobuf"))?;
                if number(&d, 1) != 0 {
                    return unsupported("a deletion file is a bitmap: only an Arrow list is read");
                }
                let name = format!(
                    "{}-{}-{}.arrow",
                    number(&frag, 1),
                    number(&d, 2),
                    number(&d, 3)
                );
                let at = under("_deletions", &name);
                let size = store.head(&at).await?;
                if size > MAX_DELETION_FILE {
                    return Err(bad("the deletion file is too large"));
                }
                let raw = store.get_range(&at, 0..size).await?;
                let rows_deleted = deleted_rows(&raw, rows).ok_or(bad("deletion file"))?;
                if number(&d, 4) != 0 && number(&d, 4) != rows_deleted.len() as u64 {
                    return Err(bad("the deletion file holds a different number of rows"));
                }
                rows_deleted
            }
            None => Vec::new(),
        };
        let object = u32::try_from(objects.len()).map_err(|_| bad("too many data files"))?;
        for e in located.extents {
            for piece in without(&e, &deleted, u64::from(dims) * 4) {
                extents.push(Extent {
                    first_row: base + piece.first_row,
                    object,
                    ..piece
                });
            }
        }
        if !crate::external::within_extent_cap(extents.len()) {
            return unsupported("the column splits into too many runs");
        }
        objects.push((data, located.len));
        base += rows;
    }
    Ok(Dataset {
        objects,
        located: Located {
            len: 0,
            dims,
            extents,
            rows: base,
        },
    })
}

/// A dataset's column, in place: its data files and the runs of rows (M87).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dataset {
    /// The data files, in row order, each with its length. A run's `object` indexes this list.
    pub objects: Vec<(Key, u64)>,
    /// The runs across all data files (`len` is unused: see each object's), and the dataset's
    /// row count, deleted rows included.
    pub located: Located,
}

/// The most bytes the deleted row numbers of one fragment may decode to.
const MAX_DELETION_ROWS_BYTES: usize = 256 << 20;
/// The largest deletion file read.
const MAX_DELETION_FILE: u64 = 64 << 20;

/// `extent` with the rows in `deleted` (sorted, unique, in the extent's own row numbers) cut out,
/// as the runs that remain.
fn without(extent: &Extent, deleted: &[u64], width: u64) -> Vec<Extent> {
    let end = extent.first_row + extent.rows;
    let start = deleted.partition_point(|d| *d < extent.first_row);
    let mut out = Vec::new();
    let mut from = extent.first_row;
    for d in deleted.get(start..).unwrap_or_default() {
        if *d >= end {
            break;
        }
        if *d > from {
            out.push(piece(extent, from, *d, width));
        }
        from = *d + 1;
    }
    if from < end {
        out.push(piece(extent, from, end, width));
    }
    out
}

/// Rows `from..to` of `extent`.
fn piece(extent: &Extent, from: u64, to: u64, width: u64) -> Extent {
    Extent {
        first_row: from,
        rows: to - from,
        offset: extent.offset + (from - extent.first_row) * width,
        ..*extent
    }
}

/// A FlatBuffers table field: the position of field `id` of the table at `table`, if present.
fn fb_field(b: &[u8], table: usize, id: usize) -> Option<usize> {
    let rel = i32::from_le_bytes(b.get(table..table.checked_add(4)?)?.try_into().ok()?);
    let vtable = usize::try_from(i64::try_from(table).ok()?.checked_sub(i64::from(rel))?).ok()?;
    let vsize = usize::from(u16::from_le_bytes(
        b.get(vtable..vtable.checked_add(2)?)?.try_into().ok()?,
    ));
    let slot = 4usize.checked_add(id.checked_mul(2)?)?;
    if slot.checked_add(2)? > vsize {
        return None;
    }
    let off = usize::from(u16::from_le_bytes(
        b.get(vtable + slot..vtable + slot + 2)?.try_into().ok()?,
    ));
    (off != 0).then(|| table + off)
}

fn fb_u32(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        b.get(at..at.checked_add(4)?)?.try_into().ok()?,
    ))
}

fn fb_i64(b: &[u8], at: usize) -> Option<i64> {
    Some(i64::from_le_bytes(
        b.get(at..at.checked_add(8)?)?.try_into().ok()?,
    ))
}

/// The table or vector a FlatBuffers offset field at `at` points to.
fn fb_indirect(b: &[u8], at: usize) -> Option<usize> {
    at.checked_add(usize::try_from(fb_u32(b, at)?).ok()?)
}

/// The rows an Arrow IPC **file** of one non-null `uint32` column lists (a Lance deletion file),
/// sorted and unique; `None` for anything else, or a row at or past `physical_rows`.
///
/// ⚠️ A hand-written reader of the few FlatBuffers it needs (footer, message, record batch): the
/// file is untrusted, every offset is checked, and compression, several columns, nulls, several
/// batches or another width are refused rather than guessed at.
fn deleted_rows(raw: &[u8], physical_rows: u64) -> Option<Vec<u64>> {
    deleted_rows_within(raw, physical_rows, MAX_DELETION_ROWS_BYTES)
}

/// [`deleted_rows`], with the most bytes the rows may decode to given.
fn deleted_rows_within(raw: &[u8], physical_rows: u64, max_bytes: usize) -> Option<Vec<u64>> {
    const MAGIC: &[u8; 6] = b"ARROW1";
    let n = raw.len();
    if raw.get(..6)? != MAGIC || raw.get(n.checked_sub(6)?..)? != MAGIC {
        return None;
    }
    let footer_len = usize::try_from(fb_u32(raw, n.checked_sub(10)?)?).ok()?;
    let footer = n.checked_sub(10)?.checked_sub(footer_len)?;
    let footer_table = fb_indirect(raw, footer)?;
    // Footer.recordBatches (field 3): a vector of 24-byte blocks.
    let batches = fb_indirect(raw, fb_field(raw, footer_table, 3)?)?;
    if fb_u32(raw, batches)? != 1 {
        return None;
    }
    let block = batches + 4;
    let offset = usize::try_from(fb_i64(raw, block)?).ok()?;
    let meta_len = usize::try_from(u32::from_le_bytes(
        raw.get(block + 8..block + 12)?.try_into().ok()?,
    ))
    .ok()?;
    // The message: a continuation marker, its size, then the FlatBuffers Message.
    if fb_u32(raw, offset)? != 0xFFFF_FFFF {
        return None;
    }
    let message = fb_indirect(raw, offset + 8)?;
    // Message.header_type (field 1) is RecordBatch (3); header (field 2) is its table.
    let ty = fb_field(raw, message, 1).and_then(|p| raw.get(p).copied())?;
    if ty != 3 {
        return None;
    }
    let batch = fb_indirect(raw, fb_field(raw, message, 2)?)?;
    // BodyCompression (field 3), when present: its codec (field 0) is 0 = LZ4_FRAME (refused) or
    // 1 = ZSTD, which Lance writes. (Its method has one value, per-buffer.)
    let zstd = match fb_field(raw, batch, 3) {
        None => false,
        Some(at) => {
            let table = fb_indirect(raw, at)?;
            let codec = fb_field(raw, table, 0).and_then(|p| raw.get(p).copied())?;
            if codec != 1 {
                return None;
            }
            true
        }
    };
    let count = u64::try_from(fb_i64(raw, fb_field(raw, batch, 0)?)?).ok()?;
    // One FieldNode { length, null_count } and two Buffers { offset, length }.
    let nodes = fb_indirect(raw, fb_field(raw, batch, 1)?)?;
    let buffers = fb_indirect(raw, fb_field(raw, batch, 2)?)?;
    if fb_u32(raw, nodes)? != 1 {
        return None;
    }
    if fb_u32(raw, buffers)? != 2 {
        return None;
    }
    if fb_i64(raw, nodes + 4)? != i64::try_from(count).ok()? || fb_i64(raw, nodes + 12)? != 0 {
        return None;
    }
    let body = offset.checked_add(meta_len)?;
    let data_off = usize::try_from(fb_i64(raw, buffers + 4 + 16)?).ok()?;
    let data_len = usize::try_from(fb_i64(raw, buffers + 4 + 16 + 8)?).ok()?;
    let want = usize::try_from(count.checked_mul(4)?).ok()?;
    if count > physical_rows || want > max_bytes {
        return None;
    }
    let start = body.checked_add(data_off)?;
    let stored = raw.get(start..start.checked_add(data_len)?)?;
    let plain: Vec<u8>;
    let data: &[u8] = if zstd {
        // Each compressed buffer is its uncompressed length (i64; -1 means stored as is), then
        // a zstd frame.
        let (len, frame) = stored.split_at_checked(8)?;
        let len = i64::from_le_bytes(len.try_into().ok()?);
        if len == -1 {
            frame
        } else {
            if usize::try_from(len).ok()? != want {
                return None;
            }
            plain = zstd::bulk::decompress(frame, want).ok()?;
            &plain
        }
    } else {
        stored
    };
    if data.len() != want {
        return None;
    }
    let mut rows: Vec<u64> = data
        .chunks_exact(4)
        .filter_map(|c| <[u8; 4]>::try_from(c).ok())
        .map(|c| u64::from(u32::from_le_bytes(c)))
        .collect();
    rows.sort_unstable();
    rows.dedup();
    (rows.last().is_none_or(|r| *r < physical_rows)).then_some(rows)
}

/// The pages of column `col` of one data file.
async fn locate_file<S: BlobStore>(
    store: &S,
    key: &Key,
    col: usize,
    dims: u32,
    rows: u64,
    declared_size: u64,
    v21: bool,
) -> Result<Located, LanceError> {
    let bad = LanceError::Malformed;
    let len = store.head(key).await?;
    if declared_size != 0 && declared_size != len {
        return Err(bad("the data file is not the size the manifest says"));
    }
    let tail = store.get_suffix(key, 40).await?;
    if tail.len() != 40 || tail.get(36..) != Some(MAGIC.as_slice()) {
        return Err(bad("no LANC magic on the data file"));
    }
    let u64_at = |at: usize| {
        tail.get(at..at + 8)
            .and_then(|b| <[u8; 8]>::try_from(b).ok())
            .map(u64::from_le_bytes)
    };
    let u16_at = |at: usize| {
        tail.get(at..at + 2)
            .and_then(|b| <[u8; 2]>::try_from(b).ok())
            .map(u16::from_le_bytes)
    };
    let cmo_pos = u64_at(8).ok_or(bad("footer"))?;
    let n_cols = tail
        .get(28..32)
        .and_then(|b| <[u8; 4]>::try_from(b).ok())
        .map(u32::from_le_bytes)
        .ok_or(bad("footer"))?;
    let footer = if v21 { (2, 1) } else { (0, 3) };
    if (u16_at(32), u16_at(34)) != (Some(footer.0), Some(footer.1)) {
        return unsupported("the data file's own version is not the one the manifest names");
    }
    if u32::try_from(col).map_or(true, |c| c >= n_cols) {
        return Err(bad("the column is past the file's columns"));
    }
    let entry_at = cmo_pos
        .checked_add(16 * col as u64)
        .filter(|e| e.saturating_add(16) <= len)
        .ok_or(bad("column metadata table"))?;
    let entry = store.get_range(key, entry_at..entry_at + 16).await?;
    let word = |at: usize| {
        entry
            .get(at..at + 8)
            .and_then(|b| <[u8; 8]>::try_from(b).ok())
            .map(u64::from_le_bytes)
    };
    let (pos, size) = word(0).zip(word(8)).ok_or(bad("column metadata entry"))?;
    if size > MAX_COLUMN_META || pos.checked_add(size).is_none_or(|e| e > len) {
        return Err(bad("column metadata runs past the file"));
    }
    let meta = store.get_range(key, pos..pos + size).await?;
    let cm = fields(&meta).ok_or(bad("column metadata protobuf"))?;

    let width = u64::from(dims) * 4;
    let (mut extents, mut first_row) = (Vec::new(), 0u64);
    for (f, w) in &cm {
        let (2, Wire::Bytes(pb)) = (*f, w) else {
            continue;
        };
        let page = fields(pb).ok_or(bad("page protobuf"))?;
        let (offsets, sizes) = (
            packed(&page, 1).ok_or(bad("page offsets"))?,
            packed(&page, 2).ok_or(bad("page sizes"))?,
        );
        let n = number(&page, 3);
        let pieces = if v21 {
            if n == 0 {
                return Err(bad("a page holds no rows"));
            }
            v21::pieces(store, key, &page, dims, n, len).await?
        } else {
            encoding_is_plain_floats(&page, dims)?;
            let ([offset], [bytes_len]) = (offsets.as_slice(), sizes.as_slice()) else {
                return unsupported("a page has more than one buffer");
            };
            if n == 0 || n.checked_mul(width) != Some(*bytes_len) {
                return Err(bad("a page's size is not rows x dims x 4"));
            }
            if offset.checked_add(*bytes_len).is_none_or(|e| e > len) {
                return Err(bad("a page runs past the file"));
            }
            vec![v21::Piece {
                rows: n,
                offset: *offset,
            }]
        };
        for piece in pieces {
            extents.push(Extent {
                first_row,
                rows: piece.rows,
                offset: piece.offset,
                compressed: 0,
                codec: 0,
                plain_len: 0,
                dict: None,
                object: 0,
            });
            first_row += piece.rows;
        }
    }
    if extents.is_empty() || first_row != rows {
        return Err(bad("the pages do not hold the fragment's rows"));
    }
    Ok(Located {
        len,
        dims,
        extents,
        rows: first_row,
    })
}

/// Accepts only `FixedSizeList(dims) -> Nullable(NoNull) -> Flat(32 bits)`, the encoding of a
/// float32 vector column with no nulls; anything else is refused with what it is.
fn encoding_is_plain_floats(page: &[(u64, Wire<'_>)], dims: u32) -> Result<(), LanceError> {
    let bad = LanceError::Malformed;
    let wrong = || {
        LanceError::Unsupported(
            "a page is not a plain, null-free float32 fixed-size list".to_owned(),
        )
    };
    // PageEncoding.direct(2) -> DirectEncoding.encoding(1) = Any { type_url, value }
    let any = bytes(page, 4)
        .and_then(fields)
        .and_then(|e| bytes(&e, 2).and_then(fields))
        .and_then(|d| bytes(&d, 1).and_then(fields))
        .ok_or_else(wrong)?;
    if bytes(&any, 1) != Some(ENCODING_URL) {
        return Err(wrong());
    }
    // The column: ArrayEncoding.nullable(2) = { no_nulls(1) = { values(1) } }
    let column = bytes(&any, 2)
        .and_then(fields)
        .and_then(|a| bytes(&a, 2).and_then(fields))
        .and_then(|n| bytes(&n, 1).and_then(fields))
        .and_then(|n| bytes(&n, 1).and_then(fields))
        .ok_or_else(wrong)?;
    // values: ArrayEncoding.fixed_size_list(3) = { dimension(1), items(2) }
    let fsl = bytes(&column, 3).and_then(fields).ok_or_else(wrong)?;
    if number(&fsl, 1) != u64::from(dims) {
        return Err(bad("a page's dimension disagrees with the manifest"));
    }
    // items: ArrayEncoding.nullable(2) = { no_nulls(1) = { values(1) } }
    let values = bytes(&fsl, 2)
        .and_then(fields)
        .and_then(|a| bytes(&a, 2).and_then(fields))
        .and_then(|n| bytes(&n, 1).and_then(fields))
        .and_then(|n| bytes(&n, 1).and_then(fields))
        .ok_or_else(wrong)?;
    // values: ArrayEncoding.flat(1) = { bits_per_value(1) = 32, buffer(2) = { index, type } }
    let flat = bytes(&values, 1).and_then(fields).ok_or_else(wrong)?;
    let buffer = bytes(&flat, 2).and_then(fields).ok_or_else(wrong)?;
    if number(&flat, 1) != 32 || number(&buffer, 1) != 0 || number(&buffer, 2) != 0 {
        return Err(wrong());
    }
    Ok(())
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]
mod tests {
    use super::*;

    fn extent(first_row: u64, rows: u64, offset: u64) -> Extent {
        Extent {
            first_row,
            rows,
            offset,
            compressed: 0,
            codec: 0,
            plain_len: 0,
            dict: None,
            object: 3,
        }
    }

    /// `(first_row, rows, offset)` of each piece.
    fn shape(pieces: &[Extent]) -> Vec<(u64, u64, u64)> {
        pieces
            .iter()
            .map(|e| (e.first_row, e.rows, e.offset))
            .collect()
    }

    #[test]
    fn deleted_rows_are_cut_out_of_a_run() {
        // Rows 10..20 at byte 1000, 16 bytes a row.
        let e = extent(10, 10, 1000);
        let cut = |deleted: &[u64]| shape(&without(&e, deleted, 16));
        assert_eq!(cut(&[]), [(10, 10, 1000)], "nothing deleted");
        assert_eq!(cut(&[3, 25]), [(10, 10, 1000)], "deletions outside the run");
        assert_eq!(cut(&[10]), [(11, 9, 1016)], "the first row");
        assert_eq!(cut(&[19]), [(10, 9, 1000)], "the last row");
        assert_eq!(
            cut(&[14]),
            [(10, 4, 1000), (15, 5, 1080)],
            "one in the middle"
        );
        assert_eq!(
            cut(&[12, 13, 17]),
            [(10, 2, 1000), (14, 3, 1064), (18, 2, 1128)],
            "a pair and a single"
        );
        assert_eq!(cut(&[10, 11, 12, 13, 14, 15, 16, 17, 18, 19]), []);
        assert_eq!(cut(&[1, 2, 10, 12, 30]), [(11, 1, 1016), (13, 7, 1048)]);
        // The object and the rest of the extent survive every piece.
        assert!(without(&e, &[14], 16).iter().all(|p| p.object == 3));
    }

    /// The real deletion file the fixture generator wrote: ten rows of 300, zstd-compressed.
    fn good() -> Vec<u8> {
        let dir = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/data/lance/deleted/_deletions"
        );
        let name = std::fs::read_dir(dir)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        std::fs::read(name).unwrap()
    }

    #[test]
    fn a_deletion_file_gives_its_sorted_rows() {
        // Written as [8, 5, 3, 1, 2, 4, 6, 7, 0, 9]: out of order, returned sorted.
        assert_eq!(deleted_rows(&good(), 300), Some((0..10).collect()));
        // Rows at or past the fragment's rows are impossible; so is a count past them.
        assert_eq!(deleted_rows(&good(), 10), Some((0..10).collect()));
        assert_eq!(deleted_rows(&good(), 9), None, "row 9 is not in 9 rows");
        assert_eq!(deleted_rows(&good(), 5), None);
    }

    fn arrow(name: &str) -> Vec<u8> {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/lance/arrow/");
        std::fs::read(format!("{dir}{name}")).unwrap()
    }

    #[test]
    fn only_a_plain_or_zstd_list_of_one_uint32_column_is_a_deletion_file() {
        let sorted: Vec<u64> = (0..10).collect();
        // The accepted shapes, written as [8, 5, 3, 1, 2, 4, 6, 7, 0, 9].
        assert_eq!(
            deleted_rows(&arrow("plain.arrow"), 10),
            Some(sorted.clone())
        );
        assert_eq!(deleted_rows(&arrow("zstd.arrow"), 10), Some(sorted));
        // Everything else is refused, not guessed at.
        for (name, why) in [
            ("lz4.arrow", "lz4 compression"),
            ("u64.arrow", "a 64-bit column"),
            ("two_columns.arrow", "two columns"),
            ("nulls.arrow", "a null"),
            ("two_batches.arrow", "two record batches"),
            ("stream.arrow", "the stream format, not the file"),
            ("empty.arrow", "no record batch at all"),
            ("string.arrow", "one node but three buffers"),
            ("null_and_u32.arrow", "two nodes but two buffers"),
        ] {
            assert_eq!(deleted_rows(&arrow(name), 10), None, "{why}");
        }
        // And the same plain file against a fragment too small for its rows.
        assert_eq!(deleted_rows(&arrow("plain.arrow"), 9), None);
        // Fewer rows than the fragment, but one of them at or past its end.
        assert_eq!(deleted_rows(&arrow("sparse.arrow"), 5), None);
        assert_eq!(deleted_rows(&arrow("sparse.arrow"), 6), Some(vec![1, 2, 5]));
    }

    #[test]
    fn the_decoded_size_of_the_rows_is_bounded() {
        // Ten rows decode to 40 bytes: allowed at 40, not at 39, whatever the fragment holds.
        let plain = arrow("plain.arrow");
        assert!(deleted_rows_within(&plain, 10, 40).is_some());
        assert_eq!(deleted_rows_within(&plain, 10, 39), None);
        assert_eq!(deleted_rows_within(&plain, 1 << 40, 39), None);
        assert!(deleted_rows_within(&plain, 1 << 40, 40).is_some());
        assert_eq!(deleted_rows_within(&plain, 9, 1 << 40), None);
    }

    #[test]
    fn a_damaged_deletion_file_is_refused_at_every_cut() {
        let ok = good();
        assert!(deleted_rows(&ok, 300).is_some());
        // Every prefix, and every single flipped byte, is either refused or reads some other
        // sorted set of rows within the fragment, never panics or reads out of bounds.
        for n in 0..ok.len() {
            assert!(deleted_rows(&ok[..n], 300).is_none(), "prefix {n}");
        }
        for i in 0..ok.len() {
            let mut b = ok.clone();
            b[i] ^= 0xFF;
            if let Some(rows) = deleted_rows(&b, 300) {
                assert!(rows.windows(2).all(|w| w[0] < w[1]));
                assert!(rows.iter().all(|r| *r < 300));
            }
        }
    }
}
