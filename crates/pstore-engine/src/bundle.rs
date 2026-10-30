//! A WAL object carrying writes for **many indexes at once**.
//!
//! Per-index flushing costs `2,592,000/T` PUTs per index per month whether the index
//! writes one document or a billion, which at a million tenants is a floor of hundreds of
//! thousands of dollars for merely being alive. Making the flush unit the *node* rather
//! than the tenant moves the cost onto write volume, where it belongs.
//!
//! ```text
//! [index A's docs][index B's docs]…[bundle index][footer]
//! ```
//! Entries are sorted by index name and each is a contiguous run, so a reader fetches
//! **only its own slice** by byte range — which is what makes sharing an object between
//! tenants safe to read as well as cheap to write.

use crate::EngineError;
use pstore_format::{Document, decode_docs, encode_docs};
use std::collections::BTreeMap;

const MAGIC: &[u8; 8] = b"PSTOREWB";

/// Where one index's rows sit inside a bundle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Byte offset of the run.
    pub offset: u64,
    /// Byte length of the run.
    pub len: u32,
    /// Documents in it.
    pub docs: u32,
}

/// Encodes one bundle from per-index document runs.
#[must_use]
pub(crate) fn encode(by_index: &BTreeMap<String, Vec<Document>>) -> Vec<u8> {
    let mut body = Vec::new();
    let mut entries: Vec<(&str, Entry)> = Vec::new();
    for (name, docs) in by_index {
        let offset = body.len() as u64;
        let run = encode_docs(docs);
        let len = run.len() as u32;
        body.extend_from_slice(&run);
        entries.push((
            name,
            Entry {
                offset,
                len,
                docs: docs.len() as u32,
            },
        ));
    }
    let index_offset = body.len() as u64;
    let mut idx = Vec::new();
    idx.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    for (name, e) in &entries {
        idx.extend_from_slice(&(name.len() as u32).to_le_bytes());
        idx.extend_from_slice(name.as_bytes());
        idx.extend_from_slice(&e.offset.to_le_bytes());
        idx.extend_from_slice(&e.len.to_le_bytes());
        idx.extend_from_slice(&e.docs.to_le_bytes());
    }
    let index_len = idx.len() as u32;
    body.extend_from_slice(&idx);
    // Footer last and fixed-width, same contract as a segment: one suffix read finds it.
    body.extend_from_slice(MAGIC);
    body.extend_from_slice(&index_offset.to_le_bytes());
    body.extend_from_slice(&index_len.to_le_bytes());
    body.extend_from_slice(MAGIC);
    body
}

/// The bundle's directory: which indexes it carries and where.
pub(crate) fn read_index(buf: &[u8]) -> Result<BTreeMap<String, Entry>, EngineError> {
    let foot_len = 8 + 8 + 4 + 8;
    let at = buf
        .len()
        .checked_sub(foot_len)
        .ok_or(EngineError::CorruptBundle)?;
    let foot = buf.get(at..).ok_or(EngineError::CorruptBundle)?;
    if foot.get(..8) != Some(&MAGIC[..]) || foot.get(foot_len - 8..) != Some(&MAGIC[..]) {
        return Err(EngineError::CorruptBundle);
    }
    let index_offset = u64::from_le_bytes(
        foot.get(8..16)
            .and_then(|b| b.try_into().ok())
            .ok_or(EngineError::CorruptBundle)?,
    ) as usize;
    let index_len = u32::from_le_bytes(
        foot.get(16..20)
            .and_then(|b| b.try_into().ok())
            .ok_or(EngineError::CorruptBundle)?,
    ) as usize;
    // ⚠️ Checked: a tampered offset of u64::MAX overflowed the addition and PANICKED
    // rather than returning an error -- a crash vector reachable from a malformed object,
    // which is the one input a storage layer must assume is hostile.
    let end = index_offset
        .checked_add(index_len)
        .ok_or(EngineError::CorruptBundle)?;
    let idx = buf
        .get(index_offset..end)
        .ok_or(EngineError::CorruptBundle)?;

    directory(idx)
}

/// One index's documents, decoded from its slice of a bundle.
pub(crate) fn read_entry(buf: &[u8], e: &Entry) -> Result<Vec<Document>, EngineError> {
    let start = e.offset as usize;
    let end = start
        .checked_add(e.len as usize)
        .ok_or(EngineError::CorruptBundle)?;
    let run = buf.get(start..end).ok_or(EngineError::CorruptBundle)?;
    Ok(decode_docs(run)?)
}

/// What the tail of a bundle says of its directory (M160).
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Directory {
    /// The tail held all of it.
    Read(BTreeMap<String, Entry>),
    /// It is longer than the tail: these absolute bytes of the bundle hold it, to be
    /// decoded with [`directory`].
    Needs(std::ops::Range<u64>),
}

/// The directory of a bundle `len` bytes long, from its last `tail.len()` bytes (M160).
///
/// ⚠️ The directory's offset is absolute, so it is placed against `len`, the length the
/// store reported, never against the tail: the footer must agree that the directory ends
/// where it begins, or the object is not the bundle it claims to be.
pub(crate) fn directory_from_tail(tail: &[u8], len: u64) -> Result<Directory, EngineError> {
    let foot_len = 8 + 8 + 4 + 8;
    let at = tail
        .len()
        .checked_sub(foot_len)
        .ok_or(EngineError::CorruptBundle)?;
    let foot = tail.get(at..).ok_or(EngineError::CorruptBundle)?;
    if foot.get(..8) != Some(&MAGIC[..]) || foot.get(foot_len - 8..) != Some(&MAGIC[..]) {
        return Err(EngineError::CorruptBundle);
    }
    let index_offset = u64::from_le_bytes(
        foot.get(8..16)
            .and_then(|b| b.try_into().ok())
            .ok_or(EngineError::CorruptBundle)?,
    );
    let index_len = u64::from(u32::from_le_bytes(
        foot.get(16..20)
            .and_then(|b| b.try_into().ok())
            .ok_or(EngineError::CorruptBundle)?,
    ));
    let end = index_offset
        .checked_add(index_len)
        .ok_or(EngineError::CorruptBundle)?;
    if end.checked_add(foot_len as u64) != Some(len) {
        return Err(EngineError::CorruptBundle);
    }
    // Where the tail starts in the bundle.
    let from = len
        .checked_sub(tail.len() as u64)
        .ok_or(EngineError::CorruptBundle)?;
    if index_offset < from {
        return Ok(Directory::Needs(index_offset..end));
    }
    let start = (index_offset - from) as usize;
    let idx = tail
        .get(start..start + index_len as usize)
        .ok_or(EngineError::CorruptBundle)?;
    Ok(Directory::Read(directory(idx)?))
}

/// A directory's own bytes, decoded: [`read_index`]'s, and M160's from a tail.
pub(crate) fn directory(idx: &[u8]) -> Result<BTreeMap<String, Entry>, EngineError> {
    let mut out = BTreeMap::new();
    let mut i = 0usize;
    let take = |i: &mut usize, n: usize| -> Result<&[u8], EngineError> {
        let end = i.checked_add(n).ok_or(EngineError::CorruptBundle)?;
        let s = idx.get(*i..end).ok_or(EngineError::CorruptBundle)?;
        *i = end;
        Ok(s)
    };
    let count = u32::from_le_bytes(
        take(&mut i, 4)?
            .try_into()
            .map_err(|_| EngineError::CorruptBundle)?,
    );
    for _ in 0..count {
        let nlen = u32::from_le_bytes(
            take(&mut i, 4)?
                .try_into()
                .map_err(|_| EngineError::CorruptBundle)?,
        ) as usize;
        let name = String::from_utf8(take(&mut i, nlen)?.to_vec())
            .map_err(|_| EngineError::CorruptBundle)?;
        let offset = u64::from_le_bytes(
            take(&mut i, 8)?
                .try_into()
                .map_err(|_| EngineError::CorruptBundle)?,
        );
        let len = u32::from_le_bytes(
            take(&mut i, 4)?
                .try_into()
                .map_err(|_| EngineError::CorruptBundle)?,
        );
        let docs = u32::from_le_bytes(
            take(&mut i, 4)?
                .try_into()
                .map_err(|_| EngineError::CorruptBundle)?,
        );
        out.insert(name, Entry { offset, len, docs });
    }
    Ok(out)
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]
mod tests {
    use super::*;

    fn bundle(names: &[&str]) -> Vec<u8> {
        let by: BTreeMap<String, Vec<Document>> = names
            .iter()
            .map(|n| {
                (
                    (*n).to_owned(),
                    vec![Document::new("a", vec![1.0]), Document::new("b", vec![2.0])],
                )
            })
            .collect();
        encode(&by)
    }

    /// M160 criteria 3 and 5: the directory is found from the bundle's tail alone, read
    /// as the store would serve the last bytes of an object whose length it reported;
    /// a directory longer than the tail asks for exactly its bytes.
    #[test]
    fn a_directory_reads_from_the_tail() {
        let whole = bundle(&["a", "b"]);
        let want = read_index(&whole).unwrap();
        let len = whole.len() as u64;
        for n in [whole.len(), 28 + 64, 4096.min(whole.len())] {
            let tail = &whole[whole.len() - n..];
            assert_eq!(
                directory_from_tail(tail, len).unwrap(),
                Directory::Read(want.clone()),
                "a tail of {n}"
            );
        }
        // Many long names: a directory past a short tail.
        let names: Vec<String> = (0..40).map(|i| format!("{i:0>100}")).collect();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let whole = bundle(&refs);
        let want = read_index(&whole).unwrap();
        let len = whole.len() as u64;
        let Directory::Needs(range) =
            directory_from_tail(&whole[whole.len() - 256..], len).unwrap()
        else {
            panic!("a 256-byte tail held a directory of 40 long names")
        };
        assert_eq!(range.end, len - 28, "the directory ends at the footer");
        let bytes = &whole[range.start as usize..range.end as usize];
        assert_eq!(directory(bytes).unwrap(), want);
        // A tail that starts exactly where the directory does holds all of it; one byte
        // shorter does not.
        let start = range.start as usize;
        assert_eq!(
            directory_from_tail(&whole[start..], len).unwrap(),
            Directory::Read(want.clone())
        );
        assert_eq!(
            directory_from_tail(&whole[start + 1..], len).unwrap(),
            Directory::Needs(range.clone())
        );
        // Not a bundle, or a length that disagrees with its footer.
        assert!(directory_from_tail(&whole[whole.len() - 20..], len).is_err());
        assert!(directory_from_tail(&whole[whole.len() - 256..], len + 1).is_err());
        // Either magic corrupted, with every other byte intact.
        for at in [28, 1] {
            let mut bad = whole.clone();
            let i = bad.len() - at;
            bad[i] ^= 0xFF;
            assert!(
                directory_from_tail(&bad[bad.len() - 256..], len).is_err(),
                "a footer with a corrupt magic {at} bytes from the end"
            );
        }
    }
}
