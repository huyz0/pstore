# M7a — The capability matrix, measured against three emulators

**Serves:** **D-100** (`Capabilities` populated by the conformance suite, not hand-written, with
a dated profile per backend checked in), **Design rule 7**, and **T-1**
([dev-and-test-environment](../../research/09-rust-stack/dev-and-test-environment.md) §2).
Closes **M0a.12** and **M0a.13**, both carried forward. Answers **OQ-150** and **OQ-153**.

**Depends on** [M0a](../M0a/SPEC.md), which built the adapter and the suite and then said what
it could not do: *"the matrix has two rows and both are local."*

## ⚠️ Why this is its own milestone, and what M7 is

The roadmap's M7 is "production hardening (ongoing)" — four bullets and **no exit criterion**,
which by this project's first non-negotiable is not a milestone. M7a takes the first bullet,
*GCS + Azure backends and the `Capabilities` matrix*, because it is the one whose deliverable
can be checked by something other than an opinion. Time travel, observability and BYOC each
need their own spec, and M6b (quotas and metering) is still unbuilt.

## ⚠️ The failure this milestone exists to prevent

Three documents say a backend whose recorded profile marks CAS `Divergent` **must refuse to
serve `durable` writes, failing loudly rather than corrupting silently**
([api-semantics](../../research/02-object-storage/api-semantics.md) §6,
[dev-and-test-environment](../../research/09-rust-stack/dev-and-test-environment.md) §3,
`object_store_backend.rs:40`). **No code enforces it.** `Capabilities` is read for
`coalesce_gap` and `max_batch_delete` and nothing else, so MinIO — which the corpus says
accepts `If-None-Match: *` and then ignores it — would serve commits today, and two writers
would both believe they created the tenant. A rule stated in three places and implemented in
none is what [`gate-design`](../../../.agents/skills/gate-design/SKILL.md) exists to refuse.

The second failure is already recorded and still live: `delete_batch` issues **one request per
key**, so GC's request rate scales with *objects* — an AGENTS.md "Never", in the tree with a
comment acknowledging it.

## The numbers this milestone is pinned to

| Name | Value | Why this value |
|---|---|---|
| Backends probed | **3** — MinIO, Azurite, fake-gcs-server | The compose stack already runs them and `dev/README.md` already promises `--features compat`, a feature that does not exist. |
| Backends that must **answer** | **2** — MinIO and Azurite | A floor, because "unreachable" is a legitimate recorded outcome and three unreachable backends must not satisfy this milestone. fake-gcs-server is the one allowed to fail, for the reason in Risks. |
| Probes per backend | **10**, the suite's existing set | Adding probes and adding backends in one change makes a divergence ambiguous. |
| Fields **measured** | **2 of 6** — `cas`, `create_if_absent` | Stated because the alternative is a document headlined "measured" that hand-writes half its content. See below. |
| `delete_batch` calls | **1** into the backend, and **over-cap is refused** | `BlobStore::delete_batch` is documented "capped at `Capabilities::max_batch_delete`" and `MemoryStore` *enforces* it. Chunking in the adapter would make the same trait method error on one implementation and succeed on the other — so the adapter refuses too, and the contract is one contract. |

⚠️ **`Capabilities` is only half measurable, and D-100 is scoped accordingly.** The ten probes
settle **`cas` and `create_if_absent`, and nothing else**. `backend` is a label copied from
what the backend declares, not a measurement. `delete_is_free` is a billing fact no probe can
see; `max_batch_delete` would need a binary search that is its own probe; `coalesce_gap` is
`G*`, which is **OQ-2** and blocked on real clouds. The matrix marks each field `measured` or
`declared`, and the D-100 claim this milestone makes is about the two primitives only.

## Delta

**Adds**
- `compat` feature on `pstore-blob`, enabling `object_store`'s `aws`, `gcp` and `azure`.
- `crates/pstore-blob/examples/probe.rs` — builds a backend per emulator from the
  `PSTORE_S3_ENDPOINT` / `PSTORE_AZURE_ENDPOINT` / `PSTORE_GCS_ENDPOINT` the compose file
  already sets, runs `pstore_testkit::conformance::run` against each, and prints the matrix.
- `scripts/conformance.sh` — runs it, writes `docs/profiles/capability-matrix.md`, and
  `--check` re-derives and fails on any disagreement. **Outside `cargo test`**, for the reason
  `recall.sh` and `depth.sh` are: it needs containers, and a suite that cannot run without
  Docker is one that stops running.
- `docs/profiles/capability-matrix.md` — generated, dated, one section per backend, including
  any that could not be reached and why, and marking each field measured or declared.
- `Capabilities::admits_durable_writes()` — false unless **both** `cas` and `create_if_absent`
  are `Supported`.

**Changes**
- **Every conditional write in `pstore-engine` is guarded**, and the door is guarded too:
  `head::commit` and `lanes::register` refuse on a backend that does not admit durable writes,
  and `Engine::flush`, `Engine::fold`, `Engine::compact` **and `Engine::gc`** refuse **before
  writing anything**. ⚠️ `gc` is in that list for a reason the others are not: it
  `delete_batch`es *before* it commits (`lib.rs:545`), so a guard only at the CAS would let it
  destroy objects and then refuse — and a divergent CAS is exactly the condition under which
  HEAD can regress and the doomed set be computed against a world another writer still names.
  ⚠️ Both, deliberately, and the codebase already argues for exactly this pair: `write()`
  refuses at the door with `check_storable` while `try_finish` still refuses at the far end
  (`lib.rs:206`). The door guard is what makes the refusal cost zero requests; the CAS guard is
  what stops a future path slipping past it.
- `ObjectStoreBackend::delete_batch` hands the batch to `delete_stream` in **one** call
  instead of looping `delete` per key, and **refuses a batch over `max_batch_delete`** exactly
  as `MemoryStore` already does — so the trait's documented cap is a caller-side precondition
  on both implementations rather than on one.
- **`pstore-catalog`'s writes are guarded at the same predicate**: `Appender::record`,
  `fold` and `write_root` refuse on a backend that does not admit durable writes. The catalog
  is *tenant* data that does not re-converge, and `write_head`'s create-if-absent is literally
  the failure this milestone's motivation names.
- `dev/README.md`'s emulator row, which names `cargo test --features compat` and a
  `pstore-conformance` crate that does not exist, is corrected to name `scripts/conformance.sh`.
- `docs/research/11-design/roadmap.md` gains the M7a split note and links, as M5 and M6 have.

**Does not add** — **real cloud accounts** (M0b's job); **new probes** — `412` vs `409`,
multipart-ETag ABA and read-after-overwrite are named in the corpus's probe list and belong to
`pstore-fake-s3`; **`pstore-fake-s3`** itself; **a probe for the declared fields**; **a guard
on `pstore-cluster`'s roster CAS** — cluster state is derived and re-converges by gossip,
tenant data does not, and mixing the two arguments in one change is how the smaller one gets
waved through; **chunking in `Engine::gc`**, whose batch is unbounded (`lib.rs:532`) and
therefore already fails above `max_batch_delete` on `MemoryStore` today — pre-existing, made
*uniform* rather than worse by the refusal above, and named here so it is a task rather than a
discovery. All three are follow-ups, not omissions.

⚠️ **Deviation, stated rather than smuggled.** The corpus says "fail loudly **at startup**".
There is no startup object here: `Engine::new` is infallible across **79** call sites, and
turning it into a `Result` would be a mechanical diff larger than this milestone's content,
through M2-verified tests. `admits_durable_writes()` is public so a server, when one exists,
can refuse at startup; until then the guard is at the door and at the CAS.

## Acceptance criteria

1. `scripts/conformance.sh` writes `docs/profiles/capability-matrix.md` with a date, and with
   **ten probe outcomes each for MinIO and Azurite** — the floor, so three unreachable backends
   cannot satisfy this milestone.
2. A backend that cannot be reached is recorded as **unreachable, with the error**, and never
   as conforming; the script's exit status distinguishes *unreachable* from *divergent*.
3. `scripts/conformance.sh --check` fails when the checked-in matrix disagrees with a fresh
   run — a backend that changes under us is a red gate rather than a surprise.
4. **OQ-150 answered**: the matrix names which backend has the highest CAS fidelity **for
   day-to-day dev**, with the probe outcomes that decide it. An answer contradicting the
   corpus's table is a correction banner on `dev-and-test-environment.md`, not a failure.
5. **OQ-153 answered** for fake-gcs-server, or recorded `NOT-RUN` naming the specific obstacle
   — "the client could not be built without a credential provider" is an answer; silence is not.
6. `admits_durable_writes()` is true **iff** `cas` and `create_if_absent` are both `Supported`
   — asserted over all nine combinations of the three `Support` values.
7. On a divergent backend, **each of `flush`, `fold`, `compact` and `gc`** returns an error
   naming the backend and the primitive, and issues **zero** requests — asserted on the
   accounted store, per entry point, not read from the code. `gc`'s `OpClass::Delete` count is
   part of that zero.
8. `head::commit` and `lanes::register` refuse on a divergent backend even when reached
   directly, so a path added later cannot commit past the door guard. `pstore-catalog`'s
   `Appender::record`, `fold` and `write_root` refuse on the same predicate, with zero
   requests.
9. A backend that admits durable writes is unaffected: the whole existing workspace suite is
   green with both guards in place.
10. `delete_batch` of 250 keys invokes the backend's `delete_stream` **once** and its per-key
    `delete` **zero** times, asserted by a counting `ObjectStore` double; a batch over
    `max_batch_delete` is refused by `ObjectStoreBackend` **and** by `MemoryStore`, with the
    same shape of error, so the contract does not depend on which one is underneath.
11. Region coverage ≥95% on the changed crates, mutation ≥80% on the changed modules, full gate
    set green.

## Test plan

| # | Test | The mutation it kills |
|---|---|---|
| 1,3 | `scripts/conformance.sh --check` against the checked-in matrix | a `--check` that regenerates instead of comparing, and so passes forever |
| 2 | `an_unreachable_backend_is_not_a_conforming_one` | an error swallowed into an empty probe list, whose `conforms()` is vacuously true |
| 6 | `only_a_fully_supported_backend_admits_durable_writes` | `\|\|` for `&&`; `Divergent` read as supported because it is "present" |
| 7 | `a_divergent_backend_is_refused_before_anything_is_written` (all four entry points), `the_refusal_names_the_backend_and_the_primitive` | the guard placed after the bundle PUT — the write lands, the error returns, the object is orphaned. A guard on `flush` alone, which `fold` and `compact` reach past. And `gc` unguarded, which **deletes** and then refuses. |
| 8 | `a_commit_on_a_divergent_backend_is_refused`, `a_lane_registration_on_a_divergent_backend_is_refused`, `a_catalog_write_on_a_divergent_backend_is_refused` | the door guard alone, which a new committer would walk around |
| 9 | the existing workspace suite | a guard that refuses `Supported` backends too |
| 10 | `a_batch_delete_is_one_call_into_the_backend`, `an_over_cap_batch_is_refused_by_both_implementations` | the per-key loop restored; the cap read from a constant rather than from `Capabilities`, or enforced on one implementation only |

⚠️ Criterion 2's mutation is the one that matters most: `Report::conforms()` is
`probes.iter().all(..)`, which is **true for an empty list**, so a backend that could not be
reached is indistinguishable from a perfect one unless the script says otherwise. That is the
failure mode of every integration suite that reports green while connected to nothing.

## RA budget

| Operation | W | Rseq depth | Rpar | List |
|---|---|---|---|---|
| `flush` / `fold` / `compact` / `gc`, conforming backend | **unchanged** | unchanged | unchanged | 0 |
| Any of them, divergent backend | **0** — deletes included | **0** | 0 | 0 |
| `delete_batch` of *n* ≤ cap keys, S3 | **1** call, one bulk request — was *n* | 1 | 0 | 0 |
| `delete_batch` of *n* ≤ cap keys, Azure | 1 call, which the backend re-chunks at **256** | 1 | 0 | 0 |
| `delete_batch` of *n* ≤ cap keys, **GCS** | 1 call, and the backend still issues ***n* requests** | 1 | 0 | 0 |
| `delete_batch` over the cap | **refused, 0** | 0 | 0 | 0 |
| The conformance run | ~40 per backend, off every serving path | — | — | **0** |

⚠️ **The "Never" is fixed for S3 and Azure and not for GCS.** `object_store` 0.14.1's GCS
`delete_stream` maps each location to its own `delete_request` — there is no bulk delete in the
JSON API it uses — so GC's request rate still scales with objects there. Recorded, not hidden:
fixing it needs a batch endpoint this adapter does not reach, and pretending otherwise in a
milestone whose subject is *measured* capability would be the exact failure it exists to
prevent.

## Risks

- **Emulators are not clouds, and nothing here may be read as saying they are.** D-99 is
  explicit: emulators test *plumbing*. A `Supported` recorded here means "this emulator did the
  right thing on ten probes", never "S3 does". Every profile section carries that sentence.
- **The GCS client may not build against fake-gcs-server** without a credential provider
  `object_store` does not expose for anonymous use. Then OQ-153 is answered with the obstacle
  rather than a number — criterion 5's escape, and the reason the floor in criterion 1 is two
  backends rather than three. It is not a reason to loosen criterion 2.
- **A checked-in matrix rots.** `--check` is what stops it, and nothing runs `--check`
  automatically because `gates.sh` must work without Docker. Named: this is a gate that has to
  be *run*, the weakest rung on the ladder this can occupy.
- **Both guards touch a path every one of 79 constructors reaches.** Criterion 9 is the whole
  defence, and it is the existing suite rather than a new test.
- **`pstore-node` wires an `unprobed` — therefore divergent — S3 backend today**, and the
  only CAS it reaches is the roster's, which this change deliberately does not guard. So the
  node's behaviour is **unchanged**, and that is worth saying plainly rather than implying a
  hardening that does not happen: the first thing to run an `Engine` or an `Appender` on that
  backend is the first thing this guard will stop, and nothing does today.

## Tasks

| Id | Commit |
|---|---|
| **M7a.1** | `admits_durable_writes()`, the door guards on `flush`/`fold`/`compact`, and the CAS guards on `head::commit`/`lanes::register` |
| **M7a.2** | `delete_batch` in one call per chunk, and the counting double that proves it |
| **M7a.3** | The `compat` feature and `examples/probe.rs` — three emulators, ten probes each |
| **M7a.4** | `scripts/conformance.sh`, the checked-in matrix, and OQ-150 / OQ-153 answered |
