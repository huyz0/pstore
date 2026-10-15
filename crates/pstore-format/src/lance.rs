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
) -> Result<(Key, Located), LanceError> {
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

    // Exactly one fragment, with no deletions, and one data file holding the field.
    let fragments: Vec<_> = m
        .iter()
        .filter_map(|(f, w)| match w {
            Wire::Bytes(b) if *f == 2 => Some(*b),
            _ => None,
        })
        .collect();
    let [fragment] = fragments.as_slice() else {
        return unsupported(format!(
            "the dataset has {} fragments: a column in place is one data file",
            fragments.len()
        ));
    };
    let frag = fields(fragment).ok_or(bad("fragment protobuf"))?;
    if bytes(&frag, 3).is_some() {
        return unsupported("the fragment has deleted rows: a row number is no longer a position");
    }
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
            return unsupported("the column is in more than one data file");
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
        return unsupported("no data file holds the column");
    };
    if (major, minor) != (2, 0) {
        return unsupported(format!(
            "the data file is format {major}.{minor}, and only 2.0 is read in place"
        ));
    }

    // <dataset>/_versions/<n>.manifest  ->  <dataset>/data/<path>
    let key = manifest.as_str();
    let dataset = key
        .rsplit_once('/')
        .and_then(|(dir, _)| {
            dir.strip_suffix("/_versions")
                .or_else(|| (dir == "_versions").then_some(""))
        })
        .ok_or(bad("the manifest is not under _versions/"))?;
    let path = std::str::from_utf8(&path).map_err(|_| bad("data file path"))?;
    if path.is_empty() || path.contains("..") || path.starts_with('/') {
        return Err(bad("data file path"));
    }
    let data = if dataset.is_empty() {
        Key::new(format!("data/{path}"))
    } else {
        Key::new(format!("{dataset}/data/{path}"))
    };
    let located = locate_file(
        store,
        &data,
        usize::try_from(col).map_err(|_| bad("column"))?,
        dims,
        rows,
        size,
    )
    .await?;
    Ok((data, located))
}

/// The pages of column `col` of one data file.
async fn locate_file<S: BlobStore>(
    store: &S,
    key: &Key,
    col: usize,
    dims: u32,
    rows: u64,
    declared_size: u64,
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
    if (u16_at(32), u16_at(34)) != (Some(0), Some(3)) {
        return unsupported("the data file's own version is not 0.3 (Lance 2.0)");
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
        extents.push(Extent {
            first_row,
            rows: n,
            offset: *offset,
            compressed: 0,
            codec: 0,
            plain_len: 0,
            dict: None,
        });
        first_row += n;
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
