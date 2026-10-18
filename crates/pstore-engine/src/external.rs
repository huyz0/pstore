//! M86: an external index registered by name.
//!
//! A tenant names a column of its own Parquet file or Lance dataset; the engine pins it (M69),
//! builds the codes (`ExternalIndex::build`) under a key of pstore's own, and commits HEAD naming
//! them. A query by name opens the codes and searches, reading the customer's object only for
//! the rows it ranks. **Off by default**: nothing is read from a customer's bucket unless the
//! deployment calls [`Engine::with_external_sources`] with the prefixes it allows.

use crate::head::{self, ExternalKind, ExternalRef, ReplicaSource};
use crate::{Engine, EngineError, MAX_COMMIT_ATTEMPTS, backoff, nonce_for, valid_name};
use pstore_blob::{BlobError, BlobStore, Key};
use pstore_format::external::{ExternalColumn, ExternalError};
use pstore_index::external::{ExternalIndex, ExternalIndexError};
use pstore_types::Epoch;

/// Rows read from the customer's object per request while building the codes.
const BUILD_CHUNK: usize = 4096;

/// Why an external index request was refused (M86), by name, so a server maps each to a status.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ExternalRefusal {
    /// The deployment has not allowed reading customers' objects: the default.
    #[error("external sources are not enabled on this deployment")]
    Off,
    /// The source is outside every prefix this deployment allows.
    #[error("the source is outside the prefixes this deployment allows: {0}")]
    Prefix(String),
    /// The name is taken, by an external index or by an index with rows.
    #[error("the name is already in use: {0}")]
    Exists(String),
    /// No external index has the name.
    #[error("no external index named {0}")]
    NotFound(String),
    /// The customer's object is not the one the index was built over.
    #[error("the source object changed since the index was built")]
    Changed,
    /// The column or the query cannot be served, and why.
    #[error("{0}")]
    Source(String),
}

impl From<ExternalRefusal> for EngineError {
    fn from(r: ExternalRefusal) -> Self {
        Self::External(r)
    }
}

/// An error of the library, as the engine's.
fn from_source(e: &ExternalError) -> EngineError {
    match e {
        ExternalError::Changed => ExternalRefusal::Changed.into(),
        ExternalError::Blob(BlobError::NotFound(k)) => {
            ExternalRefusal::Source(format!("the source has no object {k}")).into()
        }
        ExternalError::Blob(b) => EngineError::Blob(b.to_string()),
        other => ExternalRefusal::Source(other.to_string()).into(),
    }
}

fn from_index(e: &ExternalIndexError) -> EngineError {
    match e {
        ExternalIndexError::Source(s) => from_source(s),
        ExternalIndexError::Blob(b) => EngineError::Blob(b.to_string()),
        other => ExternalRefusal::Source(other.to_string()).into(),
    }
}

impl<S: BlobStore> Engine<S> {
    /// Allows this engine to read customers' objects under `prefixes` (M86). Without it, every
    /// external registration and query is refused, and no request is made.
    ///
    /// ⚠️ An empty list allows nothing: a deployment that turns the feature on and names no
    /// prefix has not said which of its buckets a tenant may point at.
    #[must_use]
    pub fn with_external_sources(mut self, prefixes: Vec<String>) -> Self {
        self.external_prefixes = Some(prefixes);
        self
    }

    /// The prefixes this engine may read under, or [`ExternalRefusal::Off`].
    fn external_enabled(&self) -> Result<&[String], ExternalRefusal> {
        match &self.external_prefixes {
            Some(p) if !p.is_empty() => Ok(p),
            _ => Err(ExternalRefusal::Off),
        }
    }

    /// Whether `source` may be read, before any request is made.
    fn external_allowed(&self, source: &str) -> Result<(), ExternalRefusal> {
        let prefixes = self.external_enabled()?;
        // A key is bytes to a store, but `..` is how a prefix check is walked around by anything
        // that resolves one.
        if source.split('/').any(|part| part == "..")
            || !prefixes.iter().any(|p| source.starts_with(p.as_str()))
        {
            return Err(ExternalRefusal::Prefix(source.to_owned()));
        }
        Ok(())
    }

    /// Where the codes of `index` registered at `epoch` live: pstore's own, derived, never
    /// discovered.
    fn codes_key(&self, epoch: Epoch, index: &str) -> Key {
        Key::new(format!(
            "{:04x}/tnt/{}/ext/{index}/{:020}-{:016x}.codes",
            self.tenant.0 as u16, self.tenant.0, epoch.0, self.lane.0
        ))
    }

    /// Registers `column` of the customer's `source` as the external index `index` (M86): pins
    /// it, builds the codes, and commits HEAD naming them. Answers the epoch that did.
    ///
    /// # Errors
    /// [`ExternalRefusal::Off`] and [`ExternalRefusal::Prefix`] before any request;
    /// [`ExternalRefusal::Exists`]; [`ExternalRefusal::Source`] with the reader's reason for a
    /// column that cannot be read in place; a store failure; a commit that never lands.
    pub async fn register_external(
        &self,
        index: &str,
        kind: ExternalKind,
        source: &str,
        column: &str,
    ) -> Result<Epoch, EngineError> {
        self.external_allowed(source)?;
        if !valid_name(index) {
            return Err(EngineError::Refused(format!(
                "{index:?} is not a valid index name"
            )));
        }
        let at = head::read(&*self.store, self.tenant).await?;
        name_free(&at.head, index)?;
        let src = Key::new(source);
        let col = match kind {
            ExternalKind::Parquet => ExternalColumn::pin_parquet(&*self.store, &src, column).await,
            ExternalKind::Lance => {
                ExternalColumn::pin_lance_latest(&*self.store, source, column).await
            }
        }
        .map_err(|e| from_source(&e))?;
        let (rows, source_len) = (col.rows, col.len);
        // Named for the epoch it expects to commit at; a retry that lands later keeps the name,
        // which is only a name.
        let codes = self.codes_key(at.head.epoch.next(), index);
        ExternalIndex::build(&*self.store, &codes, col, BUILD_CHUNK)
            .await
            .map_err(|e| from_index(&e))?;
        let entry = ExternalRef {
            kind,
            source: source.to_owned(),
            column: column.to_owned(),
            codes: codes.as_str().to_owned(),
            rows,
            source_len,
        };
        self.commit_external(at, index, entry, &codes).await
    }

    /// Commits `entry` as external index `index` on top of `at`, retrying a lost or contended
    /// commit and burying `codes` -- already written, and named by nothing -- if it gives up
    /// (M86; M92 shares it).
    async fn commit_external(
        &self,
        mut at: head::HeadAt,
        index: &str,
        entry: ExternalRef,
        codes: &Key,
    ) -> Result<Epoch, EngineError> {
        for attempt in 0..MAX_COMMIT_ATTEMPTS {
            let mut next = at.head.clone();
            next.epoch = next.epoch.next();
            next.nonce = nonce_for(next.epoch, self.lane);
            next.externals.insert(index.to_owned(), entry.clone());
            match head::commit(&*self.store, self.tenant, &at, &next).await {
                Ok(epoch) => {
                    self.record_commit(epoch);
                    self.record_reapable(epoch);
                    return Ok(epoch);
                }
                Err(e @ (EngineError::Lost | EngineError::Contended))
                    if attempt < MAX_COMMIT_ATTEMPTS - 1 =>
                {
                    if matches!(e, EngineError::Lost) {
                        at = head::read(&*self.store, self.tenant).await?;
                        if let Err(taken) = name_free(&at.head, index) {
                            self.bury_abandoned(&[codes.as_str().to_owned()]).await;
                            return Err(taken);
                        }
                    }
                    backoff(self.lane, attempt).await;
                }
                Err(e) => {
                    self.bury_abandoned(&[codes.as_str().to_owned()]).await;
                    return Err(e);
                }
            }
        }
        self.bury_abandoned(&[codes.as_str().to_owned()]).await;
        Err(EngineError::Lost)
    }

    /// Copies the external index `source` names, read from `from`, to the new name `dest` of
    /// this tenant (M92): its codes blob is moved to a key of this tenant's own and HEAD names
    /// it. Nothing is rebuilt, and the customer's object is neither read nor written.
    ///
    /// ⚠️ The registration's `source` must be under **this** engine's allowed prefixes: a
    /// deployment that has not allowed the customer's bucket does not read it because another
    /// did. And a copy from another store reads the customer's object of the same key in *its
    /// own* store, so what it finds there is the codes' pin to answer (`Source` for none,
    /// `Changed` for another), never a stale answer.
    ///
    /// # Errors
    /// [`ExternalRefusal::Off`] / [`ExternalRefusal::Prefix`] before the codes are read;
    /// [`ExternalRefusal::NotFound`] for a source with no such external index;
    /// [`ExternalRefusal::Exists`]; [`ExternalRefusal::Source`] for a codes blob that is not one;
    /// a store failure.
    pub async fn copy_external<R: BlobStore + ?Sized>(
        &self,
        dest: &str,
        source: ReplicaSource,
        from: &R,
    ) -> Result<Epoch, EngineError> {
        self.external_enabled()?;
        for n in [dest, source.index.as_str()] {
            if !valid_name(n) {
                return Err(EngineError::Refused(format!(
                    "{n:?} is not a valid index name"
                )));
            }
        }
        let there = head::read(from, source.tenant).await?;
        let entry = there
            .head
            .externals
            .get(&source.index)
            .ok_or_else(|| ExternalRefusal::NotFound(source.index.clone()))?
            .clone();
        self.external_allowed(&entry.source)?;
        let at = head::read(&*self.store, self.tenant).await?;
        name_free(&at.head, dest)?;
        let raw = from.get(&Key::new(entry.codes.clone())).await?;
        // Whatever is moved is a codes blob: a damaged one is refused here, not at a query.
        ExternalIndex::from_bytes(&raw).map_err(|e| from_index(&e))?;
        let codes = self.codes_key(at.head.epoch.next(), dest);
        self.store.put(&codes, raw).await?;
        let entry = ExternalRef {
            codes: codes.as_str().to_owned(),
            ..entry
        };
        self.commit_external(at, dest, entry, &codes).await
    }

    /// The `k` rows of external index `index` nearest `query`, as `(row, exact cosine)` (M86):
    /// the library's ranking, to the bit. The `window` best by code estimate are read from the
    /// customer's object.
    ///
    /// # Errors
    /// [`ExternalRefusal::Off`] / [`ExternalRefusal::Prefix`] before any request;
    /// [`ExternalRefusal::NotFound`]; [`ExternalRefusal::Changed`] when the customer's object is
    /// not the one built over -- never a stale answer; the library's refusal of a bad query.
    pub async fn query_external(
        &self,
        index: &str,
        query: &[f32],
        k: usize,
        window: usize,
    ) -> Result<Vec<(usize, f32)>, EngineError> {
        // The policy is checked before HEAD is read: a deployment that is off makes no request
        // for anyone, and one that narrowed its prefixes stops reading what is outside them.
        self.external_enabled()?;
        let at = head::read(&*self.store, self.tenant).await?;
        let e = at
            .head
            .externals
            .get(index)
            .ok_or_else(|| ExternalRefusal::NotFound(index.to_owned()))?;
        self.external_allowed(&e.source)?;
        let built = ExternalIndex::open(&*self.store, &Key::new(e.codes.clone()))
            .await
            .map_err(|err| from_index(&err))?;
        built
            .search(&*self.store, query, k, window)
            .await
            .map_err(|err| from_index(&err))
    }

    /// Drops external index `index` (M86): HEAD stops naming it, and its codes are buried at the
    /// epoch that did, so GC reaps them after its window. The customer's object is never touched.
    /// Answers that epoch, or `None` when no such index exists (and commits nothing).
    ///
    /// # Errors
    /// As a commit.
    pub async fn drop_external(&self, index: &str) -> Result<Option<Epoch>, EngineError> {
        let mut at = head::read(&*self.store, self.tenant).await?;
        for attempt in 0..MAX_COMMIT_ATTEMPTS {
            let Some(entry) = at.head.externals.get(index).cloned() else {
                return Ok(None);
            };
            let mut next = at.head.clone();
            next.epoch = next.epoch.next();
            next.nonce = nonce_for(next.epoch, self.lane);
            next.externals.remove(index);
            // Live until now, so buried at this epoch like a dropped index's segments.
            next.graveyard
                .entry(next.epoch.0)
                .or_default()
                .push(entry.codes);
            match head::commit(&*self.store, self.tenant, &at, &next).await {
                Ok(epoch) => {
                    self.record_commit(epoch);
                    self.record_reapable(epoch);
                    return Ok(Some(epoch));
                }
                Err(e @ (EngineError::Lost | EngineError::Contended))
                    if attempt < MAX_COMMIT_ATTEMPTS - 1 =>
                {
                    if matches!(e, EngineError::Lost) {
                        at = head::read(&*self.store, self.tenant).await?;
                    }
                    backoff(self.lane, attempt).await;
                }
                Err(e) => return Err(e),
            }
        }
        Err(EngineError::Lost)
    }
}

/// Whether `index` is a name no external index, index or replication holds in `h`.
fn name_free(h: &head::Head, index: &str) -> Result<(), EngineError> {
    if h.externals.contains_key(index)
        || h.indexes.contains_key(index)
        || h.schemas.contains_key(index)
        || h.replications.contains_key(index)
    {
        return Err(ExternalRefusal::Exists(index.to_owned()).into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::head::{Head, IndexSchema, ReplicaSource, Replication};
    use crate::{Metric, SegmentRef};
    use pstore_types::TenantId;

    fn taken(h: &Head) -> bool {
        matches!(
            name_free(h, "n"),
            Err(EngineError::External(ExternalRefusal::Exists(m))) if m == "n"
        )
    }

    #[test]
    fn a_name_is_taken_by_any_one_of_the_four_things_that_can_hold_it() {
        assert!(!taken(&Head::default()));
        let mut h = Head::default();
        h.externals.insert(
            "n".to_owned(),
            ExternalRef {
                kind: ExternalKind::Parquet,
                source: String::new(),
                column: String::new(),
                codes: String::new(),
                rows: 0,
                source_len: 0,
            },
        );
        assert!(taken(&h));
        let mut h = Head::default();
        h.indexes.insert(
            "n".to_owned(),
            vec![SegmentRef {
                key: "k".to_owned(),
                rows: 1,
                dicts: None,
                len: None,
            }],
        );
        assert!(taken(&h));
        // A schema alone: an index declared and not yet written.
        let mut h = Head::default();
        h.schemas.insert(
            "n".to_owned(),
            IndexSchema {
                dims: 2,
                text_field: String::new(),
                metric: Metric::DotProduct,
                fts: pstore_format::text::FullText::default(),
                trigram: Vec::new(),
                fields: Default::default(),
            },
        );
        assert!(taken(&h));
        let replication = Replication {
            source: ReplicaSource::local(TenantId(1), "src"),
            running: true,
            run: 1,
            applied: None,
            rejected: 0,
        };
        let mut h = Head::default();
        h.replications.insert("n".to_owned(), replication.clone());
        assert!(taken(&h));
        // And a different name in all four is not this one.
        let mut other = Head::default();
        other.replications.insert("m".to_owned(), replication);
        assert!(!taken(&other));
    }
}
