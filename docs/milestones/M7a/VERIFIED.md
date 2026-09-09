# M7a — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ Every "observed red" is a **mutation applied to the shipped code**, the named test run, and
the failure read. The emulator criteria (1–5) are evidenced by a command and by the checked-in
[`capability-matrix.md`](../../profiles/capability-matrix.md), which is the artifact.

1. **The matrix exists, dated, with ten outcomes each for MinIO and Azurite** —
   `./scripts/conformance.sh` writes
   [`docs/profiles/capability-matrix.md`](../../profiles/capability-matrix.md), dated
   2026-09-09, ten probes for `minio` and ten for `azurite`. The floor bites: it is met by two
   backends answering, not by three being recorded.
2. **Unreachable is recorded as an absence** — `NOT-RUN` as a test, verified as an artifact.
   `./scripts/conformance.sh`; in the matrix,
   fake-gcs-server is **UNREACHABLE** with the 400 that caused it and no probe table.
   ⚠️ `conforms()` is `all(..)` over the probe list, **true for an empty list**, so a backend
   that could not be reached would otherwise be indistinguishable from a perfect one — which is
   why unreachability is a different variant of the outcome rather than an empty report.
   ⚠️ **Two corrections here, both from code review.** The test the spec's plan names,
   `an_unreachable_backend_is_not_a_conforming_one`, was **never written** — hence the
   `NOT-RUN` above. The property ended up enforced by the type instead, which is stronger, but
   it means nothing executable kills that mutation — the probe is outside `cargo test`, outside the coverage gate, and guarded
   only by `--check`, which needs Docker. And the script **swallowed the probe's exit status**:
   every arm ended in an `echo`, so with no containers up it rewrote the matrix with three
   UNREACHABLE sections and exited 0. Fixed and exercised: azurite stopped → **4**, everything
   up → **4** (fake-gcs), `--check` on the recorded matrix → **0**, `--check` on an edited one
   → **1**. The matrix is no longer overwritten at all when the probe itself fails. ⚠️ The
   statuses are a **bitmask** — 4 divergent, 8 unreachable, 12 both — because the first attempt
   ordered them, and an ordering let an unreachable backend mask a divergence: in the real
   state fake-gcs is unreachable *and* MinIO diverges, so the divergence OQ-150 rests on never
   reached the exit status.
3. **`--check` compares, and does not regenerate** — `./scripts/conformance.sh --check` exits
   0 against the checked-in matrix and **1** with one probe row edited from Divergent to
   Supported, leaving the file untouched either way (`grep -c . docs/profiles/capability-matrix.md`
   unchanged, and `git diff` empty, after a failed check). Only two things are normalised: the
   generated date, and the request duration the storage client embeds in its error strings —
   "in 1.46ms" against "in 1.53ms" is scheduler noise, and a check that reddens on it is a
   check that gets switched off. ⚠️ **Run eight times consecutively to prove it is not flaky,
   and the first attempt was**: 0 0 1 0 1 over five runs, because the probe seeded its key
   namespace from a **seconds** clock, so a `--check` running seconds after a generate reused
   the previous run's keys and reported every backend `Unsupported`. That is indistinguishable
   in the output from a backend that broke overnight. Nanoseconds now; eight consecutive runs,
   all 0.
4. **OQ-150 answered — Azurite, and the stated obstacle was not the real one.**
   `./scripts/conformance.sh`: both emulators pass the create-if-absent and compare-and-swap
   probes. MinIO fails the **aba_resistance** probe — its ETags are content-derived, so
   v1 → v2 → v1 returns the first tag — and Azurite passes it, so on the CAS axis it is 3/3
   against 2/3. ⚠️ That does **not** make Azurite the better dev target; see C-14 below.
5. **OQ-153 — `NOT-RUN` for the number, answered for the obstacle.**
   `./scripts/conformance.sh` records it: fake-gcs-server 1.52 cannot be probed through the
   storage client at all. The GCS client writes with the XML API (`PUT /{bucket}/{object}`) and
   the emulator routes that path to its JSON upload handler, answering `400 invalid uploadType`;
   reads over the XML path do work. No request gets close enough to carry an
   `ifGenerationMatch`, so the fidelity question stays open — with a named way to close it.
6. **`admits_durable_writes()` is true iff both primitives are `Supported`** —
   `only_a_fully_supported_backend_admits_durable_writes` over **all nine** combinations
   (`cargo test -p pstore-blob --test capabilities`), plus
   `the_divergence_names_the_primitive_that_is_wrong`. Observed red under
   `!= Unsupported` for `== Supported`: `true` against `false` on
   `cas=Supported create_if_absent=Divergent`. That mutation is the tempting implementation,
   and `Divergent` is the *dangerous* state — the call returns success and did not fence.
7. **Four doors refuse, at zero requests** —
   `a_divergent_backend_is_refused_before_anything_is_written` (`cargo test -p pstore-engine
   --test divergent`) drives `flush`, `fold`, `compact` and `gc` and asserts `Write`, `Read`,
   `Delete` and `List` all at 0 on `Accounted`. Observed red with the `gc` guard removed. And
   `a_refused_gc_deletes_nothing`, which is the one that needed a fixture: with the guard moved
   to *after* `delete_batch`, **1 delete against 0**. `the_refusal_names_the_backend_and_the_primitive`
   pins the message; red when the backend label is blanked.
8. **Both CAS sites refuse, reached past the doors** — `a_commit_reached_past_the_doors_is_refused`
   uses `commit_head_for_test` and `commit_stale_for_test`, which are exactly the shape of a
   path added later; observed red with the guard removed from `head::commit`.
   `a_lane_registration_on_a_divergent_backend_is_refused` covers the second site, and ⚠️ **it
   is the only thing that can**: `lanes::register`'s one caller in the engine is `flush`, which
   has already refused at its own door, so deleting that guard leaves the whole suite green and
   the mutation sweep blind to it. Observed red with the guard removed. ⚠️ **Code review found
   this half of the criterion presented as verified with no evidence at all** — the same
   omission as criterion 2's, which the first pass had fixed only there.
   `a_catalog_write_on_a_divergent_backend_is_refused` covers `pstore-catalog`'s `record`,
   `observe`, `fold` and `write_root`, also at zero requests.
9. **A conforming backend is untouched** — `a_conforming_backend_is_untouched`, and the real
   defence: `cargo test --workspace --all-features` green, **91 suites**, unchanged.
10. **`delete_batch` is one call, and over-cap is refused by both** —
    `a_batch_delete_is_one_call_into_the_backend` records batch **sizes** through a counting
    `ObjectStore`: `[250]`, red as `[1; 250]` when the per-key loop is restored and `[]` when
    the stream is created and never drained. ⚠️ Sizes, not a per-key counter, and that is a
    fact about the library: `ObjectStoreExt::delete` is itself `delete_stream` over a
    one-element stream (`object_store` 0.14.1 `lib.rs:1530`), so "how many `delete` calls"
    cannot be observed and would have been a vacuous assertion.
    `an_over_cap_batch_is_refused_by_both_implementations` asserts the two errors are the same
    string; red with the cap check disabled.
11. **Gates** — `./scripts/gates.sh` green, `cargo deny check` green, `cargo test --workspace
    --all-features` 91 suites green, and `./scripts/coverage.sh --fail-under-regions 95` green
    at **95.27%** workspace regions. Mutation on the changed modules: **134 mutants, 111
    caught, 23 unviable, 0 missed, 0 timeouts** — every viable mutant killed. ⚠️ **Coverage
    on the changed crates is met per *file* and not per *crate*, and the shortfall is stated
    rather than averaged away**: every file this milestone touched is at or above the floor —
    `types.rs` 100%, `store.rs` 100%, `memory.rs` 100%, `object_store_backend.rs` 96.09%,
    `head.rs` 96.00% — while `pstore-blob` as a crate is **94.31%**, up from 94.21% before this
    change. The deficit is entirely in `congestion.rs` and `faulty.rs`, which M7a does not
    touch. Named as a follow-up; no threshold moved.

## What measuring found that reading had not

Both corrections came from running the suite, which is the entire argument for D-100.

- **[C-13](../../research/09-rust-stack/dev-and-test-environment.md) — MinIO's wildcard works.**
  The corpus says `If-None-Match: *` "does not prevent a second write" (minio#20346). On
  `RELEASE.2025-04-22T22-12-26Z`, **it does**. T-1's conclusion survives for a different
  reason: MinIO's ETags are content-derived, so `aba_resistance` fails, and MinIO is usable only
  because HEAD carries a nonce — a decision taken before anyone measured this.
- **[C-14](../../research/02-object-storage/request-efficiency-patterns.md) — the suffix GET
  does not reach Azure.** `api-semantics.md` says `Range: bytes=-N` is "supported by S3, GCS and
  Azure alike" and Pattern 6 rests on it. Three observations, kept apart because they say
  different things: the **client** refuses before building a request (`object_store` 0.14.1,
  `src/azure/client.rs:1176`), which is what the matrix's `suffix_read` row records; **Azurite**
  answers `bytes=0-99` with 206 and `bytes=-1` with **500** (`./scripts/conformance.sh
  --azure-suffix`); and the **service** documentation specifies `bytes=startByte-endByte` with
  no suffix form, which is not measured here. ⚠️ **The first version of this bullet, and of the
  banner, presented the client's refusal as a fact about the service** — in the milestone whose
  thesis is measured-not-declared. Caught in code review, twice: once in the banner and once
  here. The consequence holds on all three readings, because our adapter goes through
  `object_store`: **cold open on Azure is 3 sequential round trips, not 2**, and the extra one
  is a `head`, which Pattern 7 forbids on a hot path. Measured, not fixed.

## Two gates that were reporting on less than they appeared to

Neither is in the spec. Both were found by running the gates this milestone's criteria demand,
and both are the same failure this project keeps returning to — a check that looks green while
checking less than it says.

- ⚠️ **The mutation sweep was not building feature-gated code.** `pstore-blob`'s
  `object_store` adapter sits behind a feature; `.cargo/mutants.toml` set no features, so
  `cargo-mutants` — which enumerates mutants by *parsing* source, not by building it — listed
  every mutant in that file and reported them **MISSED**. In the output that is indistinguishable
  from a weak test, and two of this milestone's six survivors were exactly that: code the sweep
  had never compiled. Fixed by adding `--features pstore-blob/object_store` to
  `.cargo/mutants.toml`, which is what turned 6 missed into 0. `object_store` and not `compat`,
  because `compat` would pull three cloud clients into all 134 per-mutant builds for nothing.
- ⚠️ **`scripts/coverage.sh` excludes `pstore-engine`.** Its scope is computed — "a crate that
  no other crate depends on outside `[dev-dependencies]` does not ship" — and `pstore-engine` is
  named only as a **dev**-dependency of `pstore-index`, because the composition root that would
  depend on it does not exist yet. So the correctness core, where the commit protocol and
  Invariant I1 live, **is not in the coverage gate at all**: `pstore-cache pstore-engine
  pstore-testkit main.rs` is printed on every run and had not been read. Measured directly for
  this ledger: `lib.rs` 94.55%, `head.rs` 96.00%, `lanes.rs` 93.29%, `bundle.rs` 90.79%.
  **Not fixed here** — including it changes the workspace number and may put the gate red, which
  is a decision with its own change, not a side effect of this one.

## What is not built, and named rather than omitted

- **Real clouds.** M0b. Every profile says so in its own header, because a `Supported` from an
  emulator is a statement about plumbing (D-99) and nothing else.
- **The `delete_batch` fix does not help GCS.** `object_store` 0.14.1 maps GCS's `delete_stream`
  to one request per location — the JSON API it uses has no bulk delete — so GC's request rate
  still scales with objects there. In the spec's RA budget, and unfixed.
- **`Engine::gc` builds an unbounded batch** and so fails above `max_batch_delete` on every
  backend. Pre-existing — `MemoryStore` has always refused over-cap — and made *uniform* rather
  than worse by criterion 10. It is a task, not a discovery.
- **`pstore-cluster`'s roster CAS is unguarded.** Cluster state is derived and re-converges by
  gossip; tenant state does not. Deliberate, and the argument is in the spec rather than left
  to be inferred from silence.
- **Four of six `Capabilities` fields are declared, not measured**, and the matrix marks each
  one. `coalesce_gap` is `G*` — OQ-2, blocked on real clouds.
- **The two gate findings above.** Neither the coverage classifier nor `pstore-blob`'s crate
  coverage is fixed here.
- **`--check` runs only when someone runs it.** `gates.sh` must work without Docker, so nothing
  invokes it automatically. That is the weakest rung of the gate-design ladder this can occupy,
  and it is named in the spec's Risks rather than dressed up.
