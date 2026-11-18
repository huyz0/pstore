//! What a backend *actually does*, as opposed to what its documentation claims.
//!
//! `Capabilities` is **measured, not declared** (D-100). Self-hosted implementations
//! diverge on precisely the CAS semantics this architecture rests on — MinIO does not
//! accept the `If-None-Match: *` wildcard at all — so a hand-written capability table is
//! a statement of hope. The same suite runs against the in-process store, a fake, and
//! later a real cloud; **a divergence between them is the finding**.

use bytes::Bytes;
use pstore_blob::{BlobStore, Capabilities, CasError, Key, Precondition, Support};

/// One probe's outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Probe {
    /// What was probed.
    pub name: &'static str,
    /// What was observed.
    pub outcome: Support,
}

/// Everything observed about one backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    /// The backend's own name, for the recorded profile.
    pub backend: String,
    /// Every probe, in the order run.
    pub probes: Vec<Probe>,
    /// The capability profile derived from the probes — **not** from `capabilities()`.
    pub observed: Capabilities,
}

impl Report {
    /// Whether every probe behaved as the specification requires.
    #[must_use]
    pub fn conforms(&self) -> bool {
        self.probes.iter().all(|p| p.outcome == Support::Supported)
    }

    /// The probes that did not.
    #[must_use]
    pub fn divergences(&self) -> Vec<&Probe> {
        self.probes
            .iter()
            .filter(|p| p.outcome != Support::Supported)
            .collect()
    }
}

fn k(run: u64, s: &str) -> Key {
    // Namespaced per run so a suite can be pointed at a shared bucket without two runs
    // colliding -- which is how this will be used against a real cloud in M0b.
    Key::new(format!("conformance/{run}/{s}"))
}

async fn probe_create_if_absent<S: BlobStore>(s: &S, run: u64) -> Support {
    let key = k(run, "cia");
    if s.put_conditional(&key, Bytes::from_static(b"1"), Precondition::NotExists)
        .await
        .is_err()
    {
        return Support::Unsupported;
    }
    match s
        .put_conditional(&key, Bytes::from_static(b"2"), Precondition::NotExists)
        .await
    {
        Err(CasError::Lost) => Support::Supported,
        // The MinIO case: the second create SUCCEEDS, so two writers both believe they
        // created the object. Silent, and fatal to every create-once key in the design.
        Ok(_) => Support::Divergent(
            "second create-if-absent succeeded; the wildcard precondition is ignored".to_owned(),
        ),
        Err(e) => Support::Divergent(format!("unexpected error: {e}")),
    }
}

async fn probe_cas<S: BlobStore>(s: &S, run: u64) -> Support {
    let key = k(run, "cas");
    let Ok(first) = s
        .put_conditional(&key, Bytes::from_static(b"1"), Precondition::NotExists)
        .await
    else {
        return Support::Unsupported;
    };
    if s.put_conditional(
        &key,
        Bytes::from_static(b"2"),
        Precondition::Match(first.tag.clone()),
    )
    .await
    .is_err()
    {
        return Support::Divergent("CAS on the observed tag was refused".to_owned());
    }
    match s
        .put_conditional(
            &key,
            Bytes::from_static(b"3"),
            Precondition::Match(first.tag),
        )
        .await
    {
        Err(CasError::Lost) => Support::Supported,
        // The fencing property. If a stale tag lands, a paused writer can overwrite a
        // world that moved on, and no amount of care above this layer recovers it.
        Ok(_) => Support::Divergent("a stale tag was accepted: writers are not fenced".to_owned()),
        Err(e) => Support::Divergent(format!("unexpected error: {e}")),
    }
}

/// Whether a tag ever repeats for content the object held before.
///
/// Distinct from `compare_and_swap`, and the difference is exactly S3 versus GCS. Basic
/// fencing can pass while this fails: writing v1 -> v2 -> v1 returns a content-derived
/// ETag EQUAL to the first, so a writer that read v1 and paused has its CAS accepted
/// against a world that changed and changed back. A backend failing this needs the
/// monotonic epoch and nonce our manifest carries; one passing it does not.
async fn probe_aba_resistance<S: BlobStore>(s: &S, run: u64) -> Support {
    let key = k(run, "aba");
    let Ok(v1) = s
        .put_conditional(&key, Bytes::from_static(b"v1"), Precondition::NotExists)
        .await
    else {
        return Support::Unsupported;
    };
    if s.put(&key, Bytes::from_static(b"v2")).await.is_err()
        || s.put(&key, Bytes::from_static(b"v1")).await.is_err()
    {
        return Support::Unsupported;
    }
    match s
        .put_conditional(&key, Bytes::from_static(b"v3"), Precondition::Match(v1.tag))
        .await
    {
        Err(CasError::Lost) => Support::Supported,
        Ok(_) => Support::Divergent(
            "a stale tag was accepted after the content returned to its first value: tags \
             are content-derived, so the ABA hazard is live"
                .to_owned(),
        ),
        Err(e) => Support::Divergent(format!("unexpected error: {e}")),
    }
}

/// How many writers each race probe starts at once (M279): hsearch's provider suite uses 8.
pub const RACERS: usize = 8;

/// The verdict on one race: exactly one attempt won, every other lost **as a conflict** (412,
/// or a 409 the caller retries into one), and the object holds the winner's body.
async fn one_winner<S: BlobStore>(
    s: &S,
    key: &Key,
    bodies: &[Bytes],
    results: Vec<Result<pstore_blob::PutOutcome, CasError>>,
) -> Support {
    let mut winners = Vec::new();
    for (i, r) in results.into_iter().enumerate() {
        match r {
            Ok(_) => winners.push(i),
            Err(CasError::Lost | CasError::Contended) => {}
            Err(CasError::Io(e)) => {
                return Support::Divergent(format!(
                    "a losing writer got an error, not a conflict: {e}"
                ));
            }
        }
    }
    let [winner] = winners[..] else {
        return Support::Divergent(format!(
            "{} of {RACERS} concurrent conditional writes succeeded",
            winners.len()
        ));
    };
    match s.get(key).await {
        Ok(body) if bodies.get(winner) == Some(&body) => Support::Supported,
        Ok(_) => Support::Divergent("the object does not hold the winner's body".to_owned()),
        Err(e) => Support::Divergent(format!("the winner's object cannot be read: {e}")),
    }
}

fn racer_bodies() -> Vec<Bytes> {
    (0..RACERS)
        .map(|i| Bytes::from(format!("racer {i}")))
        .collect()
}

/// M279: [`RACERS`] creates of one absent key at once. The single-writer probe cannot see a
/// backend that evaluates `If-None-Match: *` against a stale view under concurrency.
async fn probe_concurrent_create<S: BlobStore>(s: &S, run: u64) -> Support {
    let key = k(run, "race-create");
    let bodies = racer_bodies();
    let results = futures_util::future::join_all(
        bodies
            .iter()
            .map(|b| s.put_conditional(&key, b.clone(), Precondition::NotExists)),
    )
    .await;
    one_winner(s, &key, &bodies, results).await
}

/// M279: [`RACERS`] compare-and-swaps on one observed tag at once: the property every HEAD
/// commit rests on, under the contention a single writer never produces.
async fn probe_concurrent_cas<S: BlobStore>(s: &S, run: u64) -> Support {
    let key = k(run, "race-cas");
    let Ok(first) = s
        .put_conditional(&key, Bytes::from_static(b"0"), Precondition::NotExists)
        .await
    else {
        return Support::Unsupported;
    };
    let bodies = racer_bodies();
    let results = futures_util::future::join_all(
        bodies
            .iter()
            .map(|b| s.put_conditional(&key, b.clone(), Precondition::Match(first.tag.clone()))),
    )
    .await;
    one_winner(s, &key, &bodies, results).await
}

/// M279: a compare-and-swap on a tag whose object was deleted since. AWS answers 404 or 412
/// depending on the race; either way another writer won, so it must surface as `Lost` and
/// never as an error the caller retries unchanged, nor as a success that recreates the object.
async fn probe_cas_after_delete<S: BlobStore>(s: &S, run: u64) -> Support {
    let key = k(run, "cas-deleted");
    let Ok(first) = s
        .put_conditional(&key, Bytes::from_static(b"1"), Precondition::NotExists)
        .await
    else {
        return Support::Unsupported;
    };
    if s.delete_batch(std::slice::from_ref(&key)).await.is_err() {
        return Support::Unsupported;
    }
    match s
        .put_conditional(
            &key,
            Bytes::from_static(b"2"),
            Precondition::Match(first.tag),
        )
        .await
    {
        Err(CasError::Lost) => Support::Supported,
        Ok(_) => Support::Divergent(
            "a compare-and-swap on a deleted object succeeded and recreated it".to_owned(),
        ),
        Err(e) => Support::Divergent(format!(
            "a compare-and-swap on a deleted object was not a conflict: {e}"
        )),
    }
}

/// The worse of two probes of one primitive: a race probe can only take support away.
fn worst(a: Support, b: Support) -> Support {
    if a == Support::Supported { b } else { a }
}

async fn probe_ranged_read<S: BlobStore>(s: &S, run: u64) -> Support {
    let key = k(run, "range");
    if s.put(&key, Bytes::from_static(b"0123456789"))
        .await
        .is_err()
    {
        return Support::Unsupported;
    }
    match s.get_range(&key, 2..5).await {
        Ok(b) if &b[..] == b"234" => Support::Supported,
        Ok(b) => Support::Divergent(format!("range 2..5 returned {} bytes", b.len())),
        Err(e) => Support::Divergent(format!("ranged read failed: {e}")),
    }
}

async fn probe_range_past_end<S: BlobStore>(s: &S, run: u64) -> Support {
    let key = k(run, "past");
    if s.put(&key, Bytes::from_static(b"0123")).await.is_err() {
        return Support::Unsupported;
    }
    match s.get_range(&key, 2..99).await {
        Err(_) => Support::Supported,
        // A short read here truncates a posting list and surfaces as a recall bug several
        // layers away from its cause.
        Ok(b) => Support::Divergent(format!("a range past the end returned {} bytes", b.len())),
    }
}

async fn probe_coalesced_read<S: BlobStore>(s: &S, run: u64) -> Support {
    let key = k(run, "coalesce");
    if s.put(&key, Bytes::from((0..=255u8).collect::<Vec<_>>()))
        .await
        .is_err()
    {
        return Support::Unsupported;
    }
    let wanted = [0..4u64, 8..12, 250..256];
    let Ok(got) = s.get_ranges(&key, &wanted).await else {
        return Support::Divergent("get_ranges failed".to_owned());
    };
    if got.len() != 3 {
        return Support::Divergent(format!("expected 3 slices, got {}", got.len()));
    }
    for (i, r) in wanted.iter().enumerate() {
        let want_len = (r.end - r.start) as usize;
        match got.get(i) {
            Some(b) if b.len() == want_len => {}
            // Returning the merged buffer rather than each range's slice is the mutation
            // this catches, and a length-only check at the top level would miss it.
            Some(b) => {
                return Support::Divergent(format!(
                    "slice {i} was {} bytes, wanted {want_len}",
                    b.len()
                ));
            }
            None => return Support::Divergent(format!("slice {i} missing")),
        }
    }
    Support::Supported
}

/// `Range: bytes=-N`. The primitive a self-describing footer is read with.
///
/// Without it a reader must `head` first, which is billed as a read and doubles the cost
/// of every cold open. A backend that answers a suffix range with the WHOLE object would
/// pass a functional test and cost a segment's bandwidth per open, so the length is
/// checked, not just the content.
async fn probe_suffix_read<S: BlobStore>(s: &S, run: u64) -> Support {
    let key = k(run, "suffix");
    if s.put(&key, Bytes::from_static(b"0123456789"))
        .await
        .is_err()
    {
        return Support::Unsupported;
    }
    match s.get_suffix(&key, 4).await {
        Ok(b) if &b[..] == b"6789" => {}
        Ok(b) => {
            return Support::Divergent(format!(
                "a 4-byte suffix returned {} bytes: {:?}",
                b.len(),
                String::from_utf8_lossy(&b)
            ));
        }
        Err(e) => return Support::Divergent(format!("suffix read failed: {e}")),
    }
    // Asking for more than exists must return the object, not fail: a small segment is
    // shorter than the fixed suffix fetch, and that is the common case.
    match s.get_suffix(&key, 1000).await {
        Ok(b) if b.len() == 10 => Support::Supported,
        Ok(b) => Support::Divergent(format!("an oversized suffix returned {} bytes", b.len())),
        Err(e) => Support::Divergent(format!("an oversized suffix failed: {e}")),
    }
}

/// `get_tag` — the rebase step of the commit protocol.
///
/// Every decorator forwards it and nothing else probed it, so a decorator that answered
/// `None` unconditionally passed the whole suite. A commit conditioned on `None` becomes a
/// create-if-absent, which on an existing key fails forever: the tenant simply stops being
/// able to commit.
async fn probe_get_tag<S: BlobStore>(s: &S, run: u64) -> Support {
    let key = k(run, "tag");
    // ⚠️ A probe that FAILED is not a probe that found nothing — `Err` here means the
    // backend refused the read, which says nothing about whether the key exists.
    match s.get_tag(&key).await {
        Ok(None) => {}
        Ok(Some(_)) => return Support::Divergent("an absent key reported a tag".to_owned()),
        Err(e) => return Support::Divergent(format!("probing an absent key failed: {e}")),
    }
    let Ok(put) = s.put(&key, Bytes::from_static(b"1")).await else {
        return Support::Unsupported;
    };
    match s.get_tag(&key).await {
        Ok(None) => Support::Divergent("an existing key reported no tag".to_owned()),
        Ok(Some(t)) if t == put.tag => Support::Supported,
        Ok(Some(_)) => {
            Support::Divergent("the tag did not match the one the write returned".to_owned())
        }
        Err(e) => Support::Divergent(format!("probing an existing key failed: {e}")),
    }
}

async fn probe_missing_key<S: BlobStore>(s: &S, run: u64) -> Support {
    // A 404 is a NORMAL answer, not an error: it is what bounds the WAL lane during
    // forward probing. A backend that hangs or 500s here breaks tail discovery.
    match s.get(&k(run, "definitely-absent")).await {
        Err(_) => Support::Supported,
        Ok(_) => Support::Divergent("a missing key returned a body".to_owned()),
    }
}

async fn probe_batch_delete<S: BlobStore>(s: &S, run: u64) -> Support {
    let keys = [k(run, "d1"), k(run, "d2")];
    for key in &keys {
        if s.put(key, Bytes::from_static(b"x")).await.is_err() {
            return Support::Unsupported;
        }
    }
    if s.delete_batch(&keys).await.is_err() {
        return Support::Unsupported;
    }
    match s.get(&keys[0]).await {
        Err(_) => Support::Supported,
        Ok(_) => Support::Divergent("a deleted key still returned a body".to_owned()),
    }
}

/// Runs every probe and derives the capability profile from what was observed.
pub async fn run<S: BlobStore>(s: &S, run_id: u64) -> Report {
    let probes = vec![
        Probe {
            name: "create_if_absent",
            outcome: probe_create_if_absent(s, run_id).await,
        },
        Probe {
            name: "compare_and_swap",
            outcome: probe_cas(s, run_id).await,
        },
        Probe {
            name: "concurrent_create_one_winner",
            outcome: probe_concurrent_create(s, run_id).await,
        },
        Probe {
            name: "concurrent_cas_one_winner",
            outcome: probe_concurrent_cas(s, run_id).await,
        },
        Probe {
            name: "cas_after_delete_is_lost",
            outcome: probe_cas_after_delete(s, run_id).await,
        },
        Probe {
            name: "aba_resistance",
            outcome: probe_aba_resistance(s, run_id).await,
        },
        Probe {
            name: "ranged_read",
            outcome: probe_ranged_read(s, run_id).await,
        },
        Probe {
            name: "suffix_read",
            outcome: probe_suffix_read(s, run_id).await,
        },
        Probe {
            name: "range_past_end_is_an_error",
            outcome: probe_range_past_end(s, run_id).await,
        },
        Probe {
            name: "coalesced_read",
            outcome: probe_coalesced_read(s, run_id).await,
        },
        Probe {
            name: "get_tag",
            outcome: probe_get_tag(s, run_id).await,
        },
        Probe {
            name: "missing_key_is_an_error",
            outcome: probe_missing_key(s, run_id).await,
        },
        Probe {
            name: "batch_delete",
            outcome: probe_batch_delete(s, run_id).await,
        },
    ];
    let find = |n: &str| {
        probes
            .iter()
            .find(|p| p.name == n)
            .map_or(Support::Unsupported, |p| p.outcome.clone())
    };
    let declared = s.capabilities();
    Report {
        backend: declared.backend.clone(),
        observed: Capabilities {
            backend: declared.backend.clone(),
            // ⚠️ Taken from the PROBE, not from what the backend claims. That difference
            // is the entire point of this module.
            // M279: and from the races. A backend that fences one writer and admits two winners
            // under concurrency does not fence; neither does one that reports a lost CAS on a
            // deleted object as an error.
            cas: worst(
                find("compare_and_swap"),
                worst(
                    find("concurrent_cas_one_winner"),
                    find("cas_after_delete_is_lost"),
                ),
            ),
            create_if_absent: worst(
                find("create_if_absent"),
                find("concurrent_create_one_winner"),
            ),
            delete_is_free: declared.delete_is_free,
            max_batch_delete: declared.max_batch_delete,
            coalesce_gap: declared.coalesce_gap,
            // M46: measured, as `cas` is. A backend that claims suffix reads and fails the
            // probe is recorded as having none, which is the one answer a segment open can
            // act on safely.
            suffix_read: find("suffix_read") == Support::Supported,
        },
        probes,
    }
}
