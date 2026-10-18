//! Boolean and string attribute columns, read chunk by chunk (M100, backlog row 75).
//!
//! Neither a bit-packed `BOOLEAN` nor a length-prefixed `BYTE_ARRAY` string has a fixed width a
//! row, so no range is computed for a row. The pin records each row group's column chunk from
//! the footer alone; a read fetches every chunk whole, in one round, and decodes its pages. A page
//! that does not hold exactly its values is [`ExternalError::Damaged`], never a wrong value.

use super::{
    ENC_PLAIN, ENC_PLAIN_DICTIONARY, ENC_RLE, ENC_RLE_DICTIONARY, Footer, MAX_PAGE_BYTES,
    PAGE_DICT, ParquetError, hybrid, page_header, unsupported,
};
use crate::external::{AttrChunk, AttrKind, AttrValue, ExternalError, decompress};

fn bad(why: &'static str) -> ParquetError {
    ParquetError::Malformed(why)
}

/// Each row group's chunk of `column`, whose physical type is `ty`, in a file of `len` bytes, and
/// the rows they hold. The checks are the ones a fixed-width chunk gets (M70 to M88): a codec this
/// crate reads, an encoding it decodes, the schema's type, inside the file, one value a row.
pub(super) fn locate_chunks(
    len: u64,
    f: &Footer,
    column: &str,
    ty: i64,
    optional: bool,
) -> Result<(u64, Vec<AttrChunk>), ParquetError> {
    let mut chunks = Vec::with_capacity(f.groups.len());
    let mut first_row = 0u64;
    for group in &f.groups {
        let (m, rows) = group
            .as_ref()
            .ok_or_else(|| ParquetError::Unsupported(format!("a row group lacks {column:?}")))?;
        let codec: u8 = match m.codec {
            Some(c @ (0 | 1 | 2 | 4 | 6 | 7)) => u8::try_from(c).map_err(|_| bad("codec"))?,
            other => {
                return unsupported(format!(
                    "the column is compressed with codec {}, and only snappy, gzip, brotli, lz4_raw and zstd are read",
                    other.map_or_else(|| "?".to_owned(), |c| c.to_string())
                ));
            }
        };
        if m.encodings
            .iter()
            .any(|e| ![ENC_PLAIN, ENC_RLE, ENC_PLAIN_DICTIONARY, ENC_RLE_DICTIONARY].contains(e))
        {
            return unsupported(
                "the column uses an encoding other than PLAIN, RLE or a dictionary",
            );
        }
        if m.ty != Some(ty) {
            return unsupported("a chunk's type disagrees with the schema");
        }
        let data = m.data_page_offset.ok_or(bad("no data page offset"))?;
        // M81: a chunk with a dictionary begins at its dictionary page; some writers record 0.
        let dict = m.dictionary_page_offset.filter(|d| *d > 0);
        let offset = u64::try_from(dict.unwrap_or(data)).map_err(|_| bad("negative offset"))?;
        let size = u64::try_from(m.total_compressed).map_err(|_| bad("negative size"))?;
        if offset.checked_add(size).is_none_or(|e| e > len) {
            return Err(bad("a chunk ends past the file"));
        }
        let rows = u64::try_from(*rows).map_err(|_| bad("negative count"))?;
        if u64::try_from(m.num_values).ok() != Some(rows) {
            return unsupported("a row group's values are not one a row");
        }
        chunks.push(AttrChunk {
            first_row,
            rows,
            offset,
            len: size,
            codec,
            optional,
            dictionary: dict.is_some(),
        });
        first_row = first_row.checked_add(rows).ok_or(bad("too many rows"))?;
    }
    Ok((first_row, chunks))
}

/// Every row's value of a column of `rows` rows whose chunks are `chunks`, `raw` holding each
/// chunk's bytes in the same order. The strings decoded hold at most `cap` bytes, and more is
/// [`ExternalError::Shape`]: [`crate::external::MAX_ATTRIBUTE_BYTES`] when the column is read.
///
/// # Errors
/// [`ExternalError::Damaged`] for a chunk that does not hold its rows; `Shape` past `cap`.
pub(crate) fn decode_chunks(
    chunks: &[AttrChunk],
    raw: &[bytes::Bytes],
    kind: AttrKind,
    rows: u64,
    cap: usize,
) -> Result<Vec<Option<AttrValue>>, ExternalError> {
    let rows = usize::try_from(rows).map_err(|_| ExternalError::Damaged)?;
    if raw.len() != chunks.len() {
        return Err(ExternalError::Damaged);
    }
    let mut out = Vec::with_capacity(rows);
    let mut budget = cap;
    for (chunk, bytes) in chunks.iter().zip(raw) {
        if u64::try_from(out.len()).ok() != Some(chunk.first_row)
            || u64::try_from(bytes.len()).ok() != Some(chunk.len)
        {
            return Err(ExternalError::Damaged);
        }
        decode_chunk(bytes, chunk, kind, &mut budget, &mut out)?;
    }
    if out.len() != rows {
        return Err(ExternalError::Damaged);
    }
    Ok(out)
}

/// A page's body as it is before any decompression, exactly `un` bytes once decoded.
fn plain(codec: u8, body: &[u8], un: usize) -> Result<Vec<u8>, ExternalError> {
    if codec == 0 {
        return if body.len() == un {
            Ok(body.to_vec())
        } else {
            Err(ExternalError::Damaged)
        };
    }
    decompress(codec, body, un)
}

/// Appends `chunk`'s rows, decoded from its bytes `b`, to `out`.
fn decode_chunk(
    b: &[u8],
    chunk: &AttrChunk,
    kind: AttrKind,
    budget: &mut usize,
    out: &mut Vec<Option<AttrValue>>,
) -> Result<(), ExternalError> {
    let damaged = || ExternalError::Damaged;
    let want = usize::try_from(chunk.rows).map_err(|_| damaged())?;
    let (start, mut at, mut first) = (out.len(), 0usize, true);
    let mut dict: Option<Vec<String>> = None;
    while out.len() - start < want {
        let h = page_header(b.get(at..).ok_or_else(damaged)?).ok_or_else(damaged)?;
        let body_at = at.checked_add(h.header_len).ok_or_else(damaged)?;
        let co = usize::try_from(h.compressed).map_err(|_| damaged())?;
        let end = body_at.checked_add(co).ok_or_else(damaged)?;
        let body = b.get(body_at..end).ok_or_else(damaged)?;
        at = end;
        let un = usize::try_from(h.uncompressed)
            .ok()
            .filter(|u| i64::try_from(*u).is_ok_and(|u| u <= MAX_PAGE_BYTES))
            .ok_or_else(damaged)?;
        // A chunk that declares a dictionary page begins with it, and has no other.
        let is_dict = h.kind == PAGE_DICT;
        if is_dict != (first && chunk.dictionary) {
            return Err(damaged());
        }
        first = false;
        let n = usize::try_from(h.values).map_err(|_| damaged())?;
        if is_dict {
            if kind != AttrKind::Str
                || (h.encoding != ENC_PLAIN && h.encoding != ENC_PLAIN_DICTIONARY)
            {
                return Err(damaged());
            }
            let page = plain(chunk.codec, body, un)?;
            // The dictionary is bounded by its page; only the strings handed out are counted.
            let mut unbounded = usize::MAX;
            dict = Some(plain_strings(&page, n, &mut unbounded)?);
            continue;
        }
        if n == 0 || n > want - (out.len() - start) {
            return Err(damaged());
        }
        // The definition levels and the values. A v1 page holds both inside its body, the levels
        // behind a 4-byte length; a v2 page's levels sit uncompressed before its values.
        let (levels, values) = if let Some(v) = &h.v2 {
            let dl = usize::try_from(v.definition_len).map_err(|_| damaged())?;
            if v.repetition_len != 0 || (!chunk.optional && dl != 0) {
                return Err(damaged());
            }
            let levels = body.get(..dl).ok_or_else(damaged)?.to_vec();
            let rest = body.get(dl..).ok_or_else(damaged)?;
            let un = un.checked_sub(dl).ok_or_else(damaged)?;
            let codec = if v.is_compressed { chunk.codec } else { 0 };
            (levels, plain(codec, rest, un)?)
        } else {
            let page = plain(chunk.codec, body, un)?;
            if chunk.optional {
                let (len, rest) = page.split_at_checked(4).ok_or_else(damaged)?;
                let len = u32::from_le_bytes(len.try_into().map_err(|_| damaged())?);
                let len = usize::try_from(len).map_err(|_| damaged())?;
                let levels = rest.get(..len).ok_or_else(damaged)?;
                (
                    levels.to_vec(),
                    rest.get(len..).ok_or_else(damaged)?.to_vec(),
                )
            } else {
                (Vec::new(), page)
            }
        };
        let present: Vec<bool> = if chunk.optional {
            hybrid(&levels, n, 1)
                .ok_or_else(damaged)?
                .into_iter()
                .map(|l| l == 1)
                .collect()
        } else {
            vec![true; n]
        };
        let held = present.iter().filter(|p| **p).count();
        if let Some(v) = &h.v2
            && usize::try_from(v.nulls).ok() != Some(n - held)
        {
            return Err(damaged());
        }
        let mut vals =
            decode_values(kind, h.encoding, &values, held, dict.as_deref(), budget)?.into_iter();
        for p in present {
            out.push(if p {
                Some(vals.next().ok_or_else(damaged)?)
            } else {
                None
            });
        }
    }
    if at == b.len() {
        Ok(())
    } else {
        Err(damaged())
    }
}

/// `n` PLAIN strings: each a 4-byte little-endian length, then that many bytes of UTF-8, with
/// nothing after. Each takes its length from `budget`, and one past it is `Shape`.
fn plain_strings(b: &[u8], n: usize, budget: &mut usize) -> Result<Vec<String>, ExternalError> {
    let damaged = || ExternalError::Damaged;
    let mut at = 0usize;
    let mut out = Vec::with_capacity(n.min(b.len() / 4));
    for _ in 0..n {
        let len = b
            .get(at..at.checked_add(4).ok_or_else(damaged)?)
            .ok_or_else(damaged)?;
        let len = u32::from_le_bytes(len.try_into().map_err(|_| damaged())?);
        let len = usize::try_from(len).map_err(|_| damaged())?;
        at += 4;
        let s = b
            .get(at..at.checked_add(len).ok_or_else(damaged)?)
            .ok_or_else(damaged)?;
        at += len;
        take(budget, len)?;
        out.push(String::from_utf8(s.to_vec()).map_err(|_| damaged())?);
    }
    if at == b.len() {
        Ok(out)
    } else {
        Err(damaged())
    }
}

/// Takes `len` bytes from `budget`, or refuses.
fn take(budget: &mut usize, len: usize) -> Result<(), ExternalError> {
    *budget = budget
        .checked_sub(len)
        .ok_or(ExternalError::Shape("the strings are too large to hold"))?;
    Ok(())
}

/// A page's `n` present values, encoded as `encoding`.
fn decode_values(
    kind: AttrKind,
    encoding: i64,
    b: &[u8],
    n: usize,
    dict: Option<&[String]>,
    budget: &mut usize,
) -> Result<Vec<AttrValue>, ExternalError> {
    let damaged = || ExternalError::Damaged;
    match (kind, encoding) {
        // Bit-packed, least significant bit first, padded to a byte.
        (AttrKind::Bool, ENC_PLAIN) => {
            if b.len() != n.div_ceil(8) {
                return Err(damaged());
            }
            (0..n)
                .map(|k| {
                    let byte = b.get(k / 8).ok_or_else(damaged)?;
                    Ok(AttrValue::Bool(byte >> (k % 8) & 1 == 1))
                })
                .collect()
        }
        // A 4-byte length, then the hybrid at width 1.
        (AttrKind::Bool, ENC_RLE) => {
            let (len, rest) = b.split_at_checked(4).ok_or_else(damaged)?;
            let len = u32::from_le_bytes(len.try_into().map_err(|_| damaged())?);
            if usize::try_from(len).ok() != Some(rest.len()) {
                return Err(damaged());
            }
            Ok(hybrid(rest, n, 1)
                .ok_or_else(damaged)?
                .into_iter()
                .map(|v| AttrValue::Bool(v == 1))
                .collect())
        }
        (AttrKind::Str, ENC_PLAIN) => Ok(plain_strings(b, n, budget)?
            .into_iter()
            .map(AttrValue::Str)
            .collect()),
        // One byte of bit width, then the hybrid: indices into the chunk's dictionary.
        (AttrKind::Str, ENC_PLAIN_DICTIONARY | ENC_RLE_DICTIONARY) => {
            let dict = dict.ok_or_else(damaged)?;
            let (width, rest) = b.split_first().ok_or_else(damaged)?;
            hybrid(rest, n, u32::from(*width))
                .ok_or_else(damaged)?
                .into_iter()
                .map(|i| {
                    let s = dict
                        .get(usize::try_from(i).map_err(|_| damaged())?)
                        .ok_or_else(damaged)?;
                    take(budget, s.len())?;
                    Ok(AttrValue::Str(s.clone()))
                })
                .collect()
        }
        _ => Err(damaged()),
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]
mod tests {
    use super::*;

    /// A v1 data page header of `n` values, PLAIN, `len` bytes of body, uncompressed: Thrift's
    /// compact protocol by hand.
    fn page_v1(n: u8, len: u8) -> Vec<u8> {
        assert!(n < 64 && len < 64, "one-byte zigzag varints");
        vec![
            0x15,
            0x00, // type: DATA_PAGE
            0x15,
            len << 1, // uncompressed size
            0x15,
            len << 1, // compressed size
            0x2C,     // field 5, a struct: DataPageHeader
            0x15,
            n << 1, // num_values
            0x15,
            0x00, // encoding: PLAIN
            0x15,
            0x06, // definition level encoding: RLE
            0x15,
            0x06, // repetition level encoding: RLE
            0x00,
            0x00,
        ]
    }

    /// One REQUIRED chunk of PLAIN strings, and its bytes.
    fn strings_chunk(strings: &[&str]) -> (AttrChunk, bytes::Bytes) {
        let mut body = Vec::new();
        for s in strings {
            body.extend(u32::try_from(s.len()).unwrap().to_le_bytes());
            body.extend(s.as_bytes());
        }
        let mut b = page_v1(
            u8::try_from(strings.len()).unwrap(),
            u8::try_from(body.len()).unwrap(),
        );
        b.extend(body);
        let chunk = AttrChunk {
            first_row: 0,
            rows: strings.len() as u64,
            offset: 0,
            len: b.len() as u64,
            codec: 0,
            optional: false,
            dictionary: false,
        };
        (chunk, b.into())
    }

    #[test]
    fn a_column_too_large_to_hold_is_not_read() {
        let (chunk, b) = strings_chunk(&["ab", "", "cde"]);
        let read = |cap| decode_chunks(&[chunk], std::slice::from_ref(&b), AttrKind::Str, 3, cap);
        let want: Vec<_> = ["ab", "", "cde"]
            .iter()
            .map(|s| Some(AttrValue::Str((*s).to_owned())))
            .collect();
        // Exactly the cap is read; a byte less is too little to hold them.
        assert_eq!(read(5).unwrap(), want);
        assert_eq!(read(6).unwrap(), want);
        assert!(matches!(read(4), Err(ExternalError::Shape(_))));
        assert!(matches!(read(0), Err(ExternalError::Shape(_))));
        // The column's rows must be the chunks', and each chunk its own length.
        assert!(matches!(
            decode_chunks(&[chunk], std::slice::from_ref(&b), AttrKind::Str, 4, 5),
            Err(ExternalError::Damaged)
        ));
        assert!(matches!(
            decode_chunks(&[chunk], &[b.slice(..b.len() - 1)], AttrKind::Str, 3, 5),
            Err(ExternalError::Damaged)
        ));
        assert!(matches!(
            decode_chunks(&[chunk], &[], AttrKind::Str, 3, 5),
            Err(ExternalError::Damaged)
        ));
    }

    /// Each fixture's first chunk of each M100 column, its bytes, and its kind.
    async fn fixture_chunks() -> Vec<(AttrChunk, Vec<u8>, AttrKind)> {
        use pstore_blob::{BlobStore, Key, MemoryStore};
        let mut out = Vec::new();
        for file in [
            "strings-plain",
            "strings-dict-snappy",
            "strings-v2-zstd",
            "vectors-labels",
        ] {
            let path = format!("{}/tests/data/{file}.parquet", env!("CARGO_MANIFEST_DIR"));
            let raw = std::fs::read(path).unwrap();
            let (store, key) = (MemoryStore::new(), Key::new("lake/f.parquet"));
            store.put(&key, raw.clone().into()).await.unwrap();
            for column in ["name", "namen", "flag", "flagn", "long", "label"] {
                let Ok((super::super::Scalar::Chunks { chunks, .. }, kind)) =
                    super::super::locate_attribute(&store, &key, column).await
                else {
                    continue;
                };
                let c = chunks[0];
                let at = usize::try_from(c.offset).unwrap();
                let bytes = raw[at..at + usize::try_from(c.len).unwrap()].to_vec();
                out.push((AttrChunk { first_row: 0, ..c }, bytes, kind));
            }
        }
        out
    }

    fn decode(b: &[u8], chunk: &AttrChunk, kind: AttrKind) -> Result<usize, ExternalError> {
        let (mut budget, mut out) = (MAX_PAGE_BYTES as usize, Vec::new());
        decode_chunk(b, chunk, kind, &mut budget, &mut out).map(|()| out.len())
    }

    #[tokio::test]
    async fn every_cut_or_flipped_byte_of_a_real_chunk_is_refused_or_holds_its_rows() {
        let chunks = fixture_chunks().await;
        assert!(chunks.len() >= 14, "{} chunks", chunks.len());
        for (chunk, b, kind) in &chunks {
            let rows = usize::try_from(chunk.rows).unwrap();
            assert_eq!(decode(b, chunk, *kind).unwrap(), rows);
            // A chunk cut short cannot hold its last page, wherever it is cut.
            for cut in 0..b.len() {
                assert!(
                    matches!(decode(&b[..cut], chunk, *kind), Err(ExternalError::Damaged)),
                    "{kind:?} cut at {cut} of {}",
                    b.len()
                );
            }
            // A flipped byte is refused as damage or too large, or still gives every row:
            // never a panic, a short column or another error.
            for at in 0..b.len() {
                let mut e = b.clone();
                e[at] ^= 0xA5;
                let got = decode(&e, chunk, *kind);
                assert!(
                    matches!(got, Err(ExternalError::Damaged | ExternalError::Shape(_)))
                        || got.as_ref().is_ok_and(|n| *n == rows),
                    "{kind:?} flipped at {at}: {got:?}"
                );
            }
        }
    }
}
