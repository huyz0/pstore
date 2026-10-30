//! What a query's plan was, as it ran (M165): recorded where each choice is made, never
//! beside it.
//!
//! ⚠️ **A task-local, read on the query's own task.** `run` and `candidates` record; the leg
//! and open tasks they spawn (M59) only return values to them, so nothing records from a
//! task the scope does not reach. Outside [`collect`], recording is a no-op, and a query that
//! did not ask pays one failed task-local lookup per segment and leg.

use std::cell::RefCell;

/// How a dense leg searched one segment, as the search reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Strategy {
    /// Every row scored at full precision.
    Exact,
    /// From clusters (a centroid table, or a named field's sidecar, M83), probing this many
    /// lists.
    Approximate {
        /// The lists probed: the smaller of the `p` asked and the segment's lists.
        probes: usize,
    },
}

/// Why a leg scored every row of every segment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Because {
    /// A filter applied (M9b).
    Filter,
    /// The legs fuse by `sum` (M9g.2).
    Sum,
}

/// One leg's figures over the local folded segments.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Leg {
    /// `dense`, `sparse`, `multi`, `multi_fde` or `text`.
    pub kind: &'static str,
    /// Its field.
    pub field: String,
    /// The rows it contributes.
    pub limit: usize,
    /// Some segment was asked for more than `limit` (M9c).
    pub widened: bool,
    /// Every row of every segment, and why.
    pub exhaustive: Option<Because>,
    /// The rows asked of each segment, summed.
    pub requested: usize,
    /// The hits the segments returned.
    pub candidates: usize,
    /// The hits left after the mask, the delete vectors and the cut to the segment's room.
    pub kept: usize,
    /// Segments searched from clusters (a dense leg only).
    pub approximate_segments: usize,
    /// Segments scanned at full precision (a dense leg only).
    pub exact_segments: usize,
    /// Lists those approximate searches probed, summed (a dense leg only).
    pub probes: usize,
}

impl Leg {
    /// A leg's entry before any segment ran it: its kind, field and limit.
    pub(crate) fn of(p: &crate::Prefetch) -> Self {
        use crate::Prefetch;
        let (kind, field, limit) = match p {
            Prefetch::Dense { field, limit, .. } => ("dense", field, limit),
            Prefetch::Sparse { field, limit, .. } => ("sparse", field, limit),
            Prefetch::Multi { field, limit, .. } => ("multi", field, limit),
            Prefetch::MultiFde { field, limit, .. } => ("multi_fde", field, limit),
            Prefetch::Text { field, limit, .. } => ("text", field, limit),
            Prefetch::Trigram { field, limit, .. } => ("trigram", field, limit),
        };
        Self {
            kind,
            field: field.clone(),
            limit: *limit,
            ..Self::default()
        }
    }
}

/// A query's plan, as it ran.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Explain {
    /// Folded segments targeted.
    pub segments: usize,
    /// Unfolded ids each folded segment was widened by (M9c).
    pub shadow: usize,
    /// Segments whose share a peer answered (M54, M55).
    pub remote_segments: usize,
    /// Whether a filter applied.
    pub filter: bool,
    /// `rrf`, `weighted_rrf`, `sum` or `max`.
    pub fusion: &'static str,
    /// One entry a leg, in request order.
    pub legs: Vec<Leg>,
}

#[derive(Debug, Default)]
struct Collector {
    /// The ordinal of the target holding this process's unfolded rows, which the engine
    /// names: not a folded segment, so in no figure.
    fresh: Option<usize>,
    explain: Explain,
}

tokio::task_local! {
    static EXPLAIN: RefCell<Collector>;
}

/// Runs `fut`, collecting the plan of the queries it runs. The last run's plan is the one
/// returned: each run starts afresh, so a retried attempt leaves nothing behind.
pub async fn collect<F: std::future::Future>(fut: F) -> (F::Output, Explain) {
    EXPLAIN
        .scope(RefCell::new(Collector::default()), async move {
            let out = fut.await;
            let explain = EXPLAIN.with(|c| std::mem::take(&mut c.borrow_mut().explain));
            (out, explain)
        })
        .await
}

/// Names the target that holds this process's unfolded rows (the engine's fresh segment),
/// or none. Kept across the runs it precedes.
pub fn fresh(ordinal: Option<usize>) {
    let _ = EXPLAIN.try_with(|c| c.borrow_mut().fresh = ordinal);
}

/// Records into the run's plan, when a caller collects one.
pub(crate) fn record(f: impl FnOnce(&mut Explain, Option<usize>)) {
    let _ = EXPLAIN.try_with(|c| {
        let mut c = c.borrow_mut();
        let fresh = c.fresh;
        f(&mut c.explain, fresh);
    });
}
