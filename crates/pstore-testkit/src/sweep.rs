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
use std::time::Duration;

pub use pstore_types::MAX_CAS_ATTEMPTS;

/// Why a point could not be taken. ⚠️ A sweep that cannot measure must say so rather than
/// return a row: a point reporting commits it never made is worse than no point, because it
/// travels.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SweepError {
    /// The key was not present when the point began, so every writer would have taken the
    /// missing-tag exit and the row would have claimed its commits landed.
    #[error("the key {0} was not seeded, so no attempt could have been conditioned on it")]
    Unseeded(Key),
}

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
    /// Commits that spent [`MAX_CAS_ATTEMPTS`] without landing and were given up on.
    ///
    /// ⚠️ **The field that makes `commits` honest.** Before M0c this struct reported
    /// `writers * commits_each` — a product of its own inputs — and no caller could tell a
    /// commit that landed from one that was never attempted.
    pub abandoned: u64,
    /// Shortest injected delay this point ran under.
    pub latency_min: Duration,
    /// Longest injected delay. The **spread** against `latency_min` is the jitter, which is
    /// the part with a reason to move the attempt ratio; the ceiling alone is not.
    pub latency_max: Duration,
    /// Injected 412/409 rate this point ran under.
    ///
    /// ⚠️ Named for the two classes CAS can express, and not "error rate": measured, a
    /// `read_error` or a `slow_down` never reaches this loop at all.
    pub cas_error_rate: f64,
    /// Wall clock for the point. ⚠️ Relative only, and the only thing injected latency is
    /// guaranteed to move.
    pub elapsed: Duration,
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
/// key until each has landed `commits_each` or spent [`MAX_CAS_ATTEMPTS`] on one.
///
/// The loop is the one from the manifest commit protocol: on `Lost` re-read and rebuild,
/// on `Contended` retry the same attempt without rebasing.
///
/// ⚠️ **Bounded, because the caller it models is.** `Appender::record` gives up after
/// `MAX_CAS_ATTEMPTS` and returns `Contended`; a sweep that retried forever would report a
/// cost no caller would ever pay, and at a 100% injected 412 rate it would not return at all.
///
/// # Errors
///
/// [`SweepError::Unseeded`] when the key is absent after the seeding attempt — which is what
/// a high injected rate does to the seed itself. Without this the point comes back claiming
/// `writers * commits_each` commits on **zero** attempts.
pub async fn contention_point<S: BlobStore>(
    store: Arc<S>,
    key: Key,
    writers: usize,
    commits_each: usize,
) -> Result<Point, SweepError> {
    store
        .put_conditional(&key, Bytes::from_static(b"0"), Precondition::NotExists)
        .await
        .ok();
    if store.get_tag(&key).await.is_none() {
        return Err(SweepError::Unseeded(key));
    }

    // ⚠️ `tokio::time::Instant`, not `std::time::Instant`. The std clock does not see a
    // paused runtime, so under `start_paused` a 30 ms round trip would be recorded as zero
    // and a test asserting that latency is actually awaited would fail against correct code.
    let started = tokio::time::Instant::now();
    let mut tasks = Vec::with_capacity(writers);
    for w in 0..writers {
        let (s, k) = (Arc::clone(&store), key.clone());
        tasks.push(tokio::spawn(async move {
            let (mut attempts, mut lost, mut commits, mut abandoned) = (0u64, 0u64, 0u64, 0u64);
            for c in 0..commits_each {
                let mut spent = 0u32;
                loop {
                    if spent >= MAX_CAS_ATTEMPTS {
                        abandoned += 1;
                        break;
                    }
                    // Rebase: read the state this attempt will be conditioned on.
                    let tag = match s.get_tag(&k).await {
                        Some(t) => t,
                        None => {
                            abandoned += 1;
                            break;
                        }
                    };
                    // The window every optimistic protocol has: another writer can land
                    // between the read and the CAS. Yielding widens it deterministically
                    // enough to be measurable on a store with no network.
                    tokio::task::yield_now().await;
                    attempts += 1;
                    spent += 1;
                    let body = Bytes::from(format!("w{w}c{c}"));
                    match s.put_conditional(&k, body, Precondition::Match(tag)).await {
                        Ok(_) => {
                            commits += 1;
                            break;
                        }
                        Err(CasError::Lost) => lost += 1,
                        Err(CasError::Contended) => {}
                        Err(CasError::Io(_)) => {
                            abandoned += 1;
                            break;
                        }
                    }
                }
            }
            (attempts, lost, commits, abandoned)
        }));
    }

    let (mut attempts, mut lost, mut commits, mut abandoned) = (0u64, 0u64, 0u64, 0u64);
    for t in tasks {
        if let Ok((a, l, c, ab)) = t.await {
            attempts += a;
            lost += l;
            commits += c;
            abandoned += ab;
        }
    }
    Ok(Point {
        writers,
        commits_each,
        attempts,
        // ⚠️ Counted. This was `(writers * commits_each) as u64` — a product of the inputs,
        // true only while nothing can abandon a commit, and silently flattering once
        // something can.
        commits,
        lost,
        abandoned,
        latency_min: Duration::ZERO,
        latency_max: Duration::ZERO,
        cas_error_rate: 0.0,
        elapsed: started.elapsed(),
    })
}

/// Sweeps contention across `levels`, returning one [`Point`] each.
///
/// # Errors
///
/// Propagates [`SweepError::Unseeded`] from any level.
pub async fn contention_sweep<S: BlobStore>(
    store: Arc<S>,
    levels: &[usize],
    commits_each: usize,
) -> Result<Vec<Point>, SweepError> {
    let mut out = Vec::with_capacity(levels.len());
    for (i, w) in levels.iter().enumerate() {
        // A fresh key per level, so one level's final state cannot bias the next.
        let key = Key::new(format!("sweep/contention/{i}"));
        out.push(contention_point(Arc::clone(&store), key, *w, commits_each).await?);
    }
    Ok(out)
}

/// Seeds a key on a raw store, then wraps it in the injecting one.
///
/// ⚠️ **The seed must land outside the injection**, and measuring showed why: seeded through
/// a store at `cas_lost: 1.0` the seed never lands, every writer takes the missing-tag exit,
/// and the row comes back reporting commits on zero attempts. Seeded outside, the same
/// configuration livelocks instead — which is the behaviour the budget exists to bound, and
/// which cannot be observed at all through a contaminated seed.
async fn seeded(key: &Key) -> pstore_blob::MemoryStore {
    let raw = pstore_blob::MemoryStore::new();
    raw.put_conditional(key, Bytes::from_static(b"0"), Precondition::NotExists)
        .await
        .ok();
    raw
}

/// How does the commit protocol behave as round-trip latency rises?
///
/// Each entry of `spreads` is a `(min, max)` delay applied to **every** operation. ⚠️ No
/// direction is claimed for the attempt ratio: the delay lands on the rebase read and on the
/// CAS alike, so it scales the vulnerable window and the whole cycle together. What rises is
/// [`Point::elapsed`]; whether jitter moves the ratio is what this axis is for.
///
/// # Errors
///
/// Propagates [`SweepError::Unseeded`], which a correctly seeded store cannot produce.
pub async fn latency_sweep(
    spreads: &[(Duration, Duration)],
    writers: usize,
    commits_each: usize,
    seed: u64,
) -> Result<Vec<Point>, SweepError> {
    let mut out = Vec::with_capacity(spreads.len());
    for (i, &(lo, hi)) in spreads.iter().enumerate() {
        let key = Key::new(format!("sweep/latency/{i}"));
        let store = Arc::new(pstore_blob::Faulty::new(
            seeded(&key).await,
            seed,
            pstore_blob::Faults {
                latency_min: lo,
                latency_max: hi,
                ..pstore_blob::Faults::none()
            },
        ));
        let mut p = contention_point(store, key, writers, commits_each).await?;
        p.latency_min = lo;
        p.latency_max = hi;
        out.push(p);
    }
    Ok(out)
}

/// How does it behave as the backend refuses conditional writes?
///
/// ⚠️ **412 and 409 only, and the name says so.** Measured: `read_error`, `write_error` and
/// `slow_down` never reach this loop — its only read is `get_tag`, which returns `Option` and
/// so has no error channel, and `put_conditional` consults only the CAS classes. An axis
/// called "error rate" would imply a coverage that does not exist.
///
/// # Errors
///
/// Propagates [`SweepError::Unseeded`], which a correctly seeded store cannot produce.
pub async fn cas_error_sweep(
    rates: &[f64],
    writers: usize,
    commits_each: usize,
    seed: u64,
) -> Result<Vec<Point>, SweepError> {
    let mut out = Vec::with_capacity(rates.len());
    for (i, &rate) in rates.iter().enumerate() {
        let key = Key::new(format!("sweep/cas-error/{i}"));
        let store = Arc::new(pstore_blob::Faulty::new(
            seeded(&key).await,
            seed,
            pstore_blob::Faults {
                cas_lost: rate,
                ..pstore_blob::Faults::none()
            },
        ));
        let mut p = contention_point(store, key, writers, commits_each).await?;
        p.cas_error_rate = rate;
        out.push(p);
    }
    Ok(out)
}

/// Renders a sweep as a table, with the caveat attached rather than left to a reader.
#[must_use]
pub fn render(points: &[Point]) -> String {
    let mut s = String::from(
        "writers  latency  412/409  commits  aband  attempts  lost  attempts/commit  \
         success  wall\n\
         -------  -------  -------  -------  -----  --------  ----  ---------------  \
         -------  ----\n",
    );
    for p in points {
        // ⚠️ A row at or above the budget is flagged in the row itself. A threshold printed
        // only in prose beneath a table is a threshold that gets pasted away from it.
        let over = if p.abandoned > 0 || p.attempts_per_commit() >= f64::from(MAX_CAS_ATTEMPTS) {
            " <- at the give-up budget"
        } else {
            ""
        };
        s.push_str(&format!(
            "{:>7}  {:>6}ms  {:>7.2}  {:>7}  {:>5}  {:>8}  {:>4}  {:>15.2}  {:>6.1}%  \
             {:>4}ms{over}\n",
            p.writers,
            p.latency_max.as_millis(),
            p.cas_error_rate,
            p.commits,
            p.abandoned,
            p.attempts,
            p.lost,
            p.attempts_per_commit(),
            p.success_rate() * 100.0,
            p.elapsed.as_millis(),
        ));
    }
    s.push_str(&format!(
        "\nGive-up budget: {MAX_CAS_ATTEMPTS} attempts per commit, the same constant the \
         catalog's\nappender stops at -- so a flagged row is one a real caller would have \
         failed.\n\
         \nPROVISIONAL: this is our commit protocol against an in-process store, not S3.\n\
         The SHAPE is the deliverable; the position of the real operating point is M0b.\n"
    ));
    s
}
