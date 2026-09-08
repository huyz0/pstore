//! Exact retrieval over a sparse field.
//!
//! ⚠️ **No probe set, no recall knob, no ladder.** Every row carrying any query dimension is
//! scored — `modalities-and-sequencing.md` §5: *"sparse search is exact, not approximate"* —
//! which is why this is a fraction of the dense path and why it lands first. What it is
//! *not* is a promise about score precision: the impacts are quantized (D-72, OQ-126), so
//! ranking is exact only against an f32 oracle. The candidate set is the exact part.
//!
//! The layout, the encodings and the dictionary live in
//! [`pstore_format::sparse`](pstore_format::sparse); this module only decides which byte
//! ranges to ask for and adds up what comes back.

use pstore_blob::{BlobStore, Key};
use pstore_format::sparse::{Dictionary, Entry, dict_key};
use pstore_format::{FormatError, Section, Segment};

/// An opened sparse field: the segment's directory and the term dictionary, both in memory.
#[derive(Debug)]
pub struct SparseIndex {
    dict: Dictionary,
    span: std::ops::Range<u64>,
}

impl SparseIndex {
    /// Opens a sparse field. **One round trip**: the dictionary is a different object from
    /// the segment, so it is fetched *beside* the footer rather than after it.
    pub async fn open<S: BlobStore>(
        store: &S,
        key: &Key,
        field: &str,
    ) -> Result<Self, FormatError> {
        // ⚠️ `join`, not two awaits. Both keys are derived and neither depends on the
        // other's contents, so awaiting the footer first would make every cold open two hops
        // for no reason a caller could see. Same shape as `VecIndex::open`, same reason.
        // ⚠️ `Pinned`: every query on this field needs the dictionary, it unblocks
        // everything downstream, and it is a fraction of a percent of the field's bytes
        // (D-21).
        let (seg, raw) = futures_util::future::join(
            Segment::open(store, key),
            store.get_immutable(&dict_key(key), pstore_blob::Class::Pinned),
        )
        .await;
        // ⚠️ A missing dictionary is an ERROR here, unlike the centroid table, which is
        // absent by design below the exact-scan threshold. There is no "scan the postings
        // exactly" fallback: without the dictionary the section is an undelimited byte
        // string, and answering from it would be guessing.
        let raw = raw.map_err(|e| FormatError::Blob(e.to_string()))?;
        Self::from_segment(&seg?, field, raw.as_ref())
    }

    /// Opens a sparse field over an **already-open** segment.
    ///
    /// ⚠️ Exists so a hybrid query opens the segment once. Two retrievers each calling
    /// `open` is one extra suffix read, one extra `Meta` admission, and — with a cache in
    /// the stack — completely invisible, because singleflight collapses the two identical
    /// concurrent reads into one and the request counter reports the right answer for the
    /// wrong code.
    pub fn from_segment(
        segment: &Segment,
        field: &str,
        dictionary: &[u8],
    ) -> Result<Self, FormatError> {
        let layout = segment
            .field_layout(field)
            .ok_or(FormatError::UnknownField)?;
        // ⚠️ An error, not an empty answer. A dense field asked for sparsely would otherwise
        // return zero hits, which reads exactly like a field with no matches — so a caller
        // could not tell a typo from data.
        if layout.kind != 1 {
            return Err(FormatError::UnknownField);
        }
        let span = segment
            .field_section(field, Section::SparsePostings)
            .ok_or(FormatError::Corrupt(
                "a sparse field with no postings section",
            ))?;
        let dict = Dictionary::decode(dictionary)
            .ok_or(FormatError::Corrupt("the sparse dictionary did not decode"))?;
        Ok(Self { dict, span })
    }

    /// Where the postings section lives, for a byte assertion that can name a section.
    #[must_use]
    pub fn span(&self) -> std::ops::Range<u64> {
        self.span.clone()
    }

    /// The dictionary this field was opened with.
    #[must_use]
    pub fn dictionary(&self) -> &Dictionary {
        &self.dict
    }

    /// Bytes of postings the query's own lists occupy — what a fetch may legitimately move.
    #[must_use]
    pub fn list_bytes(&self, query: &[(u32, f32)]) -> u64 {
        self.entries(query)
            .iter()
            .map(|(e, _)| u64::from(e.bytes))
            .sum()
    }

    /// The dictionary entries a query touches, in ascending byte order.
    ///
    /// ⚠️ A dimension the vocabulary does not contain is dropped **here**, before any range
    /// exists. It is the common case — a query term the corpus never used — and it must cost
    /// no request, no byte and no error.
    fn entries(&self, query: &[(u32, f32)]) -> Vec<(Entry, f32)> {
        let mut out: Vec<(Entry, f32)> = query
            .iter()
            .filter_map(|(dim, w)| self.dict.lookup(*dim).map(|e| (e, *w)))
            .collect();
        // Ascending, so the coalescer sees the runs it can merge. Unsorted ranges cost the
        // same requests and make the plan unreadable.
        out.sort_by_key(|(e, _)| e.offset);
        out.dedup_by_key(|(e, _)| e.dim);
        out
    }

    /// The `k` best rows for a sparse query. **One further round trip**, whatever the term
    /// count.
    ///
    /// Scores are `Σ qᵢ · dᵢ` over shared dimensions — a dot product, which is what a learned
    /// sparse model's impacts mean.
    pub async fn search<S: BlobStore>(
        &self,
        store: &S,
        key: &Key,
        query: &[(u32, f32)],
        k: usize,
    ) -> Result<Vec<(usize, f32)>, FormatError> {
        let entries = self.entries(query);
        if entries.is_empty() {
            return Ok(Vec::new());
        }
        let ranges: Vec<std::ops::Range<u64>> = entries
            .iter()
            .map(|(e, _)| {
                let lo = self.span.start + e.offset;
                lo..lo + u64::from(e.bytes)
            })
            .collect();
        // ⚠️ One call, so every list goes out together. Awaiting each before issuing the next
        // would give the same answer at one round trip per query term — 30 ms each, and
        // invisible to every functional test. Width is free; depth is not.
        // ⚠️ `Bulk`: postings are the query's payload, not its metadata, and admitting them
        // to the pinned arena would evict the dictionaries that unblock every other query
        // (D-21 is quotas, not priorities).
        let bufs = store
            .get_ranges_as(key, &ranges, pstore_blob::Class::Bulk)
            .await?;

        let mut scores: std::collections::HashMap<u32, f32> = std::collections::HashMap::new();
        for (n, (e, qw)) in entries.iter().enumerate() {
            let Some(buf) = bufs.get(n) else { continue };
            for (row, impact) in self.dict.decode_list(e, buf) {
                *scores.entry(row).or_insert(0.0) += qw * impact;
            }
        }

        let mut scored: Vec<(usize, f32)> =
            scores.into_iter().map(|(r, s)| (r as usize, s)).collect();
        // Ties by row so a ranking is reproducible across runs — a `HashMap` iteration order
        // is not, and a test comparing against brute force would fail intermittently.
        scored.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        scored.truncate(k);
        Ok(scored)
    }
}
