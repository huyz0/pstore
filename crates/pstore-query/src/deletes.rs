//! Delete vectors — a segment's rows that a newer operation superseded (M9c).
//!
//! Cumulative and immutable: the vector written at epoch `E` lists every row of its segment
//! deleted as of `E`, so a query reads exactly one per segment.

use std::collections::HashSet;

/// The rows, as sorted little-endian `u32`s.
#[must_use]
pub fn encode(rows: &HashSet<usize>) -> Vec<u8> {
    let mut sorted: Vec<u32> = rows.iter().filter_map(|r| u32::try_from(*r).ok()).collect();
    sorted.sort_unstable();
    sorted.iter().flat_map(|r| r.to_le_bytes()).collect()
}

/// The rows a vector deletes. A trailing fragment shorter than a row is ignored.
#[must_use]
pub fn decode(raw: &[u8]) -> HashSet<usize> {
    raw.chunks_exact(4)
        .filter_map(|c| c.try_into().ok())
        .map(|b| u32::from_le_bytes(b) as usize)
        .collect()
}
