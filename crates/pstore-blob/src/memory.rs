//! An in-process backend with exact, assertable semantics.
//!
//! This is the **primary correctness vehicle** (D-3, promoted by D-99): no emulator
//! implements our CAS primitive faithfully, so the only backend whose behaviour we can
//! both control and assert is one we wrote.

use crate::{BlobError, Capabilities, CasError, Key, Precondition, PutOutcome, Support};
use bytes::Bytes;
use pstore_types::CasTag;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

/// How the backend derives the tag it hands back — the axis real backends differ on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TagStyle {
    /// Monotonic generation, as GCS does. Immune to ABA: no two states share a tag.
    #[default]
    Monotonic,
    /// Content-derived, as an S3 ETag is for a single-part unencrypted PUT.
    ///
    /// ⚠️ **ABA-prone**: writing `v1`, then `v2`, then `v1` again returns a tag equal to
    /// the first, so a writer holding the original tag can land against a world that
    /// changed and changed back. This is why the manifest carries a monotonic epoch and a
    /// nonce, and it is modelled here so that hazard is testable rather than argued about.
    ContentHash,
}

#[derive(Debug, Default)]
struct State {
    objects: BTreeMap<String, (Bytes, CasTag)>,
    generation: u64,
}

/// In-memory [`BlobStore`](crate::BlobStore).
#[derive(Debug, Clone)]
pub struct MemoryStore {
    state: Arc<Mutex<State>>,
    caps: Capabilities,
    tag_style: TagStyle,
}

impl Default for MemoryStore {
    fn default() -> Self {
        Self::new()
    }
}

impl MemoryStore {
    /// A backend with the strong, GCS-like tag semantics.
    #[must_use]
    pub fn new() -> Self {
        Self::with_tag_style(TagStyle::Monotonic)
    }

    /// A backend whose coalescing gap is `gap` bytes.
    ///
    /// ⚠️ Exists because the default 64 KiB gap is larger than a small fixture's whole
    /// object, so every ranged read merges into one and a test measuring "does asking for
    /// more ranges move more bytes" measures the coalescer instead. At production sizing a
    /// posting list is tens of kilobytes and the gap does not merge them; at test sizing it
    /// does, and shrinking the gap is the honest way to keep the fixture small without
    /// removing the effect under test.
    #[must_use]
    pub fn with_coalesce_gap(gap: u64) -> Self {
        let mut s = Self::new();
        s.caps.coalesce_gap = gap;
        s
    }

    /// A backend that refuses delete batches larger than `n`.
    ///
    /// ⚠️ Exists because the cap is `Capabilities`' to state and a caller's to read: S3 takes
    /// 1000 per request and Azure 256, and code that knows a number instead of asking for one
    /// is code that is wrong on one of them. A small `n` is what makes the boundary reachable
    /// in a test without building a thousand objects.
    #[must_use]
    pub fn with_max_batch_delete(n: usize) -> Self {
        let mut s = Self::new();
        s.caps.max_batch_delete = n;
        s
    }

    /// A backend with the chosen tag semantics, for exercising the ABA hazard.
    #[must_use]
    pub fn with_tag_style(tag_style: TagStyle) -> Self {
        Self {
            state: Arc::new(Mutex::new(State::default())),
            caps: Capabilities {
                backend: format!("memory({tag_style:?})"),
                cas: Support::Supported,
                create_if_absent: Support::Supported,
                delete_is_free: true,
                max_batch_delete: 1000,
                coalesce_gap: 64 * 1024,
            },
            tag_style,
        }
    }

    /// Poison recovery rather than `unwrap`: a panic in another thread must not make the
    /// store permanently unusable, and this crate denies `unwrap_used`.
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn next_tag(&self, st: &mut State, body: &Bytes) -> CasTag {
        match self.tag_style {
            TagStyle::Monotonic => {
                st.generation = st.generation.saturating_add(1);
                CasTag::new(st.generation.to_string())
            }
            // FNV-1a. Not a real MD5, but it has the property under test: equal content
            // yields an equal tag.
            TagStyle::ContentHash => {
                let mut h: u64 = 0xcbf2_9ce4_8422_2325;
                for b in body.iter() {
                    h ^= u64::from(*b);
                    h = h.wrapping_mul(0x1000_0000_01b3);
                }
                CasTag::new(format!("\"{h:016x}\""))
            }
        }
    }
}

#[async_trait::async_trait]
impl crate::BlobStore for MemoryStore {
    fn capabilities(&self) -> &Capabilities {
        &self.caps
    }

    async fn get(&self, key: &Key) -> Result<Bytes, BlobError> {
        self.lock()
            .objects
            .get(key.as_str())
            .map(|(b, _)| b.clone())
            .ok_or_else(|| BlobError::NotFound(key.to_string()))
    }

    async fn get_range(&self, key: &Key, range: std::ops::Range<u64>) -> Result<Bytes, BlobError> {
        let body = self.get(key).await?;
        let len = body.len() as u64;
        if range.end > len || range.start > range.end {
            return Err(BlobError::RangeOutOfBounds(range, len));
        }
        // `slice` rather than indexing: this crate denies `indexing_slicing`, and the
        // bounds are checked above where the error is meaningful.
        Ok(body.slice(range.start as usize..range.end as usize))
    }

    async fn get_with_tag(&self, key: &Key) -> Result<(Bytes, CasTag), BlobError> {
        // Under one lock, so the pair cannot straddle another writer's commit.
        self.lock()
            .objects
            .get(key.as_str())
            .map(|(b, t)| (b.clone(), t.clone()))
            .ok_or_else(|| BlobError::NotFound(key.to_string()))
    }

    async fn get_suffix(&self, key: &Key, n: u64) -> Result<Bytes, BlobError> {
        let body = self.get(key).await?;
        let start = (body.len() as u64).saturating_sub(n) as usize;
        Ok(body.slice(start..))
    }

    async fn get_tag(&self, key: &Key) -> Option<CasTag> {
        self.lock()
            .objects
            .get(key.as_str())
            .map(|(_, t)| t.clone())
    }

    async fn head(&self, key: &Key) -> Result<u64, BlobError> {
        self.get(key).await.map(|b| b.len() as u64)
    }

    async fn put(&self, key: &Key, body: Bytes) -> Result<PutOutcome, BlobError> {
        let mut st = self.lock();
        let tag = self.next_tag(&mut st, &body);
        st.objects
            .insert(key.as_str().to_owned(), (body, tag.clone()));
        Ok(PutOutcome { tag })
    }

    async fn put_conditional(
        &self,
        key: &Key,
        body: Bytes,
        pre: Precondition,
    ) -> Result<PutOutcome, CasError> {
        let mut st = self.lock();
        let current = st.objects.get(key.as_str()).map(|(_, t)| t.clone());
        let ok = match (&pre, &current) {
            (Precondition::NotExists, None) => true,
            (Precondition::NotExists, Some(_)) => false,
            (Precondition::Match(want), Some(have)) => want == have,
            (Precondition::Match(_), None) => false,
        };
        if !ok {
            return Err(CasError::Lost);
        }
        let tag = self.next_tag(&mut st, &body);
        st.objects
            .insert(key.as_str().to_owned(), (body, tag.clone()));
        Ok(PutOutcome { tag })
    }

    async fn delete_batch(&self, keys: &[Key]) -> Result<(), BlobError> {
        if keys.len() > self.caps.max_batch_delete {
            return Err(BlobError::Other(format!(
                "batch of {} exceeds the backend's limit of {}",
                keys.len(),
                self.caps.max_batch_delete
            )));
        }
        let mut st = self.lock();
        for k in keys {
            st.objects.remove(k.as_str());
        }
        Ok(())
    }

    async fn list_unrestricted(&self, prefix: &Key) -> Result<Vec<Key>, BlobError> {
        Ok(self
            .lock()
            .objects
            .keys()
            .filter(|k| k.starts_with(prefix.as_str()))
            .map(|k| Key::new(k.clone()))
            .collect())
    }
}
