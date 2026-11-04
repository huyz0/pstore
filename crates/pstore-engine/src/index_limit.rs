//! A tenant's index limit (M169): the most names its HEAD may count.
//!
//! ⚠️ **Checked where a name enters, never at the fold.** A create, branch, replication or
//! external index checks the HEAD its CAS is built on, so it is exact. A write checks the
//! door's cached HEAD and this process's unfolded names, so two processes with stale HEADs may
//! each admit one: a fold that refused would drop rows already acknowledged durable.

use crate::{Engine, EngineError, head, is_rowless};
use pstore_blob::BlobStore;
use std::collections::BTreeSet;

/// The names `head` counts against a limit: every name an index's existence rests on.
pub(crate) fn counted(head: &head::Head) -> BTreeSet<String> {
    head.indexes
        .keys()
        .chain(head.schema_rejects.keys())
        .chain(head.quarantine.keys())
        .chain(head.externals.keys())
        .chain(head.replications.keys())
        .cloned()
        .collect()
}

impl<S: BlobStore> Engine<S> {
    /// Refuses, past `limit` indexes, any request that would add one (M169). `0`, the default,
    /// is unbounded.
    #[must_use]
    pub fn with_index_limit(mut self, limit: usize) -> Self {
        self.index_limit = limit;
        self
    }

    /// The limit [`Self::with_index_limit`] set.
    #[must_use]
    pub fn index_limit(&self) -> usize {
        self.index_limit
    }

    /// Keeps `head`'s counted names for the door, when a limit is set and `head` is no older
    /// than what is held.
    pub(crate) fn remember_counted(&self, head: &head::Head) {
        if self.index_limit == 0 {
            return;
        }
        let mut held = self
            .counted
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if held.as_ref().is_none_or(|(at, _)| head.epoch > *at) {
            *held = Some((head.epoch, counted(head)));
        }
    }

    /// The names this process holds unfolded rows for, which no HEAD counts yet.
    fn unfolded_names(&self) -> BTreeSet<String> {
        let m = self.mem();
        m.pending
            .iter()
            .chain(m.durable.iter().flat_map(|(_, batch)| batch.iter()))
            .filter(|(_, docs)| docs.iter().any(|d| !is_rowless(d)))
            .map(|(name, _)| name.clone())
            .collect()
    }

    /// Whether adding `index` passes the limit, given the names a HEAD counts.
    fn past_limit(&self, names: &BTreeSet<String>, index: &str) -> bool {
        if self.index_limit == 0 || names.contains(index) {
            return false;
        }
        let unfolded = self.unfolded_names();
        if unfolded.contains(index) {
            return false;
        }
        let new = unfolded.iter().filter(|n| !names.contains(*n)).count();
        names.len() + new >= self.index_limit
    }

    /// [`EngineError::IndexLimit`] when adding `index` to `head` passes the limit: what every
    /// path that commits a new name asks of the HEAD its CAS is built on.
    pub(crate) fn refuse_past_limit(
        &self,
        head: &head::Head,
        index: &str,
    ) -> Result<(), EngineError> {
        if self.index_limit != 0 && self.past_limit(&counted(head), index) {
            return Err(self.limit_error());
        }
        Ok(())
    }

    fn limit_error(&self) -> EngineError {
        EngineError::IndexLimit {
            limit: self.index_limit,
        }
    }

    /// The write door's check: by the cached HEAD, at zero requests; a refusal re-reads HEAD
    /// once before it stands, so a drop another process made is seen.
    ///
    /// ⚠️ **Best effort with no cached HEAD**, by M35's rule: the door's read already failed
    /// once, and a buffered write is not refused for a read fault.
    pub(crate) async fn refuse_past_limit_at_door(&self, index: &str) -> Result<(), EngineError> {
        if self.index_limit == 0 {
            return Ok(());
        }
        let past = {
            let held = self
                .counted
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match held.as_ref() {
                Some((_, names)) => self.past_limit(names, index),
                None => false,
            }
        };
        if !past {
            return Ok(());
        }
        let at = head::read(&*self.store, self.tenant).await?;
        self.remember_schemas(&at.head);
        self.refuse_past_limit(&at.head, index)
    }
}
