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

/// One index's documents, decoded from its slice of a bundle.
pub(crate) fn read_entry(buf: &[u8], e: &Entry) -> Result<Vec<Document>, EngineError> {
    let start = e.offset as usize;
    let end = start
        .checked_add(e.len as usize)
        .ok_or(EngineError::CorruptBundle)?;
    let run = buf.get(start..end).ok_or(EngineError::CorruptBundle)?;
    Ok(decode_docs(run)?)
}
