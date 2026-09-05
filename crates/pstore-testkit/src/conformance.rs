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
            name: "aba_resistance",
            outcome: probe_aba_resistance(s, run_id).await,
        },
        Probe {
            name: "ranged_read",
            outcome: probe_ranged_read(s, run_id).await,
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
            cas: find("compare_and_swap"),
            create_if_absent: find("create_if_absent"),
            delete_is_free: declared.delete_is_free,
            max_batch_delete: declared.max_batch_delete,
            coalesce_gap: declared.coalesce_gap,
        },
        probes,
    }
}
