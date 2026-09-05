//! Sensitivity sweeps: where we cannot measure, parameterize (D-101).
//!
//! OQ-5 asks what CAS throughput a real backend sustains under contention. We have no
//! cloud accounts, so we cannot answer it. The useful substitute is not a guess but the
//! **shape of the curve**: at what contention does the commit protocol stop making
//! progress, and how does cost per commit grow on the way there? One measurement in M0b
//! then says which point on this curve reality sits at, rather than starting the analysis
//! from scratch.
//!
//! ⚠️ **This measures our protocol against our own store**, not S3. The curve's shape is
//! the deliverable; its position is not.

use bytes::Bytes;
use pstore_blob::{BlobStore, CasError, Key, Precondition};
use std::sync::Arc;

/// One row of a sweep: what happened at one contention level.
#[derive(Debug, Clone, PartialEq)]
pub struct Point {
    /// Writers racing for the same key.
    pub writers: usize,
    /// Commits each writer was asked to land.
    pub commits_each: usize,
    /// CAS calls issued in total, successful or not.
    pub attempts: u64,
    /// Commits that landed.
    pub commits: u64,
    /// 412s: another writer won, so the loser rebased.
    pub lost: u64,
}

impl Point {
    /// CAS calls spent per commit landed. **The cost curve.**
    ///
    /// At 1.0 every attempt lands. Growth is superlinear once writers exceed the point
    /// where a read-modify-write window reliably overlaps another's, which is why the
    /// design keeps bulk writes off the CAS path entirely.
    #[must_use]
    pub fn attempts_per_commit(&self) -> f64 {
        if self.commits == 0 {
            return f64::INFINITY;
        }
        self.attempts as f64 / self.commits as f64
    }

    /// Fraction of CAS calls that landed.
    #[must_use]
    pub fn success_rate(&self) -> f64 {
        if self.attempts == 0 {
            return 0.0;
        }
        self.commits as f64 / self.attempts as f64
    }
}

/// Runs the real commit loop — read, rebase, CAS, retry — with `writers` racing for one
/// key until each has landed `commits_each`.
///
/// The loop is the one from the manifest commit protocol: on `Lost` re-read and rebuild,
/// on `Contended` retry the same attempt without rebasing.
pub async fn contention_point<S: BlobStore>(
    store: Arc<S>,
    key: Key,
    writers: usize,
    commits_each: usize,
) -> Point {
    store
        .put_conditional(&key, Bytes::from_static(b"0"), Precondition::NotExists)
        .await
        .ok();

    let mut tasks = Vec::with_capacity(writers);
    for w in 0..writers {
        let (s, k) = (Arc::clone(&store), key.clone());
        tasks.push(tokio::spawn(async move {
            let (mut attempts, mut lost) = (0u64, 0u64);
            for c in 0..commits_each {
                loop {
                    // Rebase: read the state this attempt will be conditioned on.
                    let tag = match s.get_tag(&k).await {
                        Some(t) => t,
                        None => break,
                    };
                    // The window every optimistic protocol has: another writer can land
                    // between the read and the CAS. Yielding widens it deterministically
                    // enough to be measurable on a store with no network.
                    tokio::task::yield_now().await;
                    attempts += 1;
                    let body = Bytes::from(format!("w{w}c{c}"));
                    match s.put_conditional(&k, body, Precondition::Match(tag)).await {
                        Ok(_) => break,
                        Err(CasError::Lost) => lost += 1,
                        Err(CasError::Contended) => {}
                        Err(CasError::Io(_)) => break,
                    }
                }
            }
            (attempts, lost)
        }));
    }

    let (mut attempts, mut lost) = (0u64, 0u64);
    for t in tasks {
        if let Ok((a, l)) = t.await {
            attempts += a;
            lost += l;
        }
    }
    Point {
        writers,
        commits_each,
        attempts,
        commits: (writers * commits_each) as u64,
        lost,
    }
}

/// Sweeps contention across `levels`, returning one [`Point`] each.
pub async fn contention_sweep<S: BlobStore>(
    store: Arc<S>,
    levels: &[usize],
    commits_each: usize,
) -> Vec<Point> {
    let mut out = Vec::with_capacity(levels.len());
    for (i, w) in levels.iter().enumerate() {
        // A fresh key per level, so one level's final state cannot bias the next.
        let key = Key::new(format!("sweep/contention/{i}"));
        out.push(contention_point(Arc::clone(&store), key, *w, commits_each).await);
    }
    out
}

/// Renders a sweep as a table, with the caveat attached rather than left to a reader.
#[must_use]
pub fn render(points: &[Point]) -> String {
    let mut s = String::from(
        "writers  commits  attempts  lost  attempts/commit  success\n\
         -------  -------  --------  ----  ---------------  -------\n",
    );
    for p in points {
        s.push_str(&format!(
            "{:>7}  {:>7}  {:>8}  {:>4}  {:>15.2}  {:>6.1}%\n",
            p.writers,
            p.commits,
            p.attempts,
            p.lost,
            p.attempts_per_commit(),
            p.success_rate() * 100.0
        ));
    }
    s.push_str(
        "\nPROVISIONAL: this is our commit protocol against an in-process store, not S3.\n\
         The SHAPE is the deliverable; the position of the real operating point is M0b.\n",
    );
    s
}
