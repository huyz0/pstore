# Engineering Standards: Architecture, Modularity, and Test Rigour

**Answers:** Q46
**Status:** Complete (v1)

## 1. The asks, and one honest push-back

| Ask | Position |
|---|---|
| Best code quality and clean architecture | Agreed — with Rust-specific mechanisms rather than generic dogma (§3–4) |
| **Test coverage > 95%** | Adopted as a **floor**, with differentiated targets and a mutation-score gate beside it (§6) |
| Modular, but not too many modules | **9 library crates + 2 test crates**, each boundary justified by an invariant (§2) |

The push-back is on coverage, and it matters for *this* system specifically:

> **95% line coverage is a hygiene floor, not an assurance argument.** Our hard bugs do not live
> in unexecuted lines — they live in **interleavings, failure paths, and semantics**: two nodes
> racing a CAS, a bundle recovery that misses a record, a SIMD kernel with a wrong lane index. A
> distributed commit protocol can have 100% line coverage and still be wrong.

The failure mode is also predictable: a hard 95% gate produces tests written to touch lines
rather than to assert behaviour, which is *worse* than 85% with sharp tests — the number goes up
while confidence goes down.

So: keep 95% as the floor, and pair it with **mutation testing**, which measures whether tests
actually detect behavioural change. *"Coverage measurements tell you what code is reached by a
test, but mutation tests give different information about whether the tests really check the
code's behavior."* A test that executes a line and asserts nothing scores on coverage and fails
on mutation — which is exactly the distinction we need.

## 2. Modularity: the criterion, then the crates

> **D-108.** A crate boundary must **encode an architectural invariant that the compiler
> enforces**. If a split does not buy an enforced invariant, it is a module, not a crate.

Splitting for its own sake has real costs — *"splitting crates has a cost in that you need to
define the interface well"*, plus disk, IDE memory, and dependency churn. Splitting too late is
also a trap, because *"your code will depend on and use private interfaces that you don't want
it to."* The invariant test resolves both.

### Nine library crates, strictly layered

| # | Crate | Invariant the boundary enforces |
|---|---|---|
| 1 | `pstore-types` | Shared newtypes and errors. **Types only, no logic, no dependencies** — the rule that stops it becoming a junk drawer. |
| 2 | `pstore-blob` | **Nothing above may touch `object_store` directly**, so request accounting and congestion control are unavoidable (D-2). Owns the cache too — "how we get bytes" is one layer. |
| 3 | `pstore-kernel` | **The only crate permitted `unsafe`** (D-98). SIMD, quantization, bitmaps. |
| 4 | `pstore-format` | Segment format stability and versioning live in exactly one place. |
| 5 | `pstore-engine` | The correctness core: manifest/CAS, WAL lanes and bundles, compaction, MVCC, GC. **Invariant I1 is proven here.** |
| 6 | `pstore-index` | SPANN vector index, sparse/BM25 postings, filtering — sharing one posting-list machinery (D-72). |
| 7 | `pstore-cluster` | Gossip, LRH placement, work assignment, health and gray detection. |
| 8 | `pstore-query` | Planner, vectorized execution, session tokens. |
| 9 | `pstore-server` | API surface, auth, quotas, the node binary. |

Plus two that never ship:

| Crate | Note |
|---|---|
| `pstore-testkit` | Simulator, conformance suite, fault injection, fixtures. `dev-dependencies` only. |
| `pstore-fake-s3` | **Standalone, no `pstore` dependencies** — so it cannot drift toward "whatever makes our tests pass" ([`blob-store-fakes.md`](blob-store-fakes.md) §8). |

**Layering is enforced by construction**: Cargo forbids cycles, and each crate's `Cargo.toml`
lists only layers below it. There is no separate mechanism to maintain — the dependency list
*is* the architecture, and a PR that adds an upward dependency is visible in the diff.

The bonus is compile time: *"only crates with changes have to be recompiled"*, which matters on
a WSL2 box with capped cores.

## 3. Invariants the compiler enforces

The cheapest quality mechanism available: make the rule un-writable rather than reviewed.

```toml
# Cargo.toml (workspace root)
[workspace.lints.rust]
unsafe_code                  = "forbid"     # overridden to "allow" ONLY in pstore-kernel
missing_docs                 = "warn"
unreachable_pub              = "warn"
rust_2018_idioms             = "warn"

[workspace.lints.clippy]
unwrap_used                  = "deny"       # allowed in #[cfg(test)]
expect_used                  = "deny"
panic                        = "deny"
indexing_slicing             = "deny"       # forces .get(); pairs with the bounds-check work
todo                         = "deny"
unimplemented                = "deny"
dbg_macro                    = "deny"
float_cmp                    = "deny"
mem_forget                   = "deny"
```

> **D-109.** `unsafe_code = "forbid"` at the workspace root, relaxed in **exactly one** crate.
> This converts D-98's "unsafe is concentrated in the kernels" from a convention into a
> compile error — and makes `git diff Cargo.toml` the complete audit of where unsafe may exist.

`indexing_slicing = deny` is deliberate: it forces `.get()` at call sites and pushes the
deliberate `get_unchecked` decisions into `pstore-kernel`, where they are reviewed and fuzzed
([`hot-loop-performance.md`](hot-loop-performance.md) §5).

## 4. Clean architecture, the parts that pay in Rust

Not ports-and-adapters dogma — four mechanisms that actually prevent bugs here:

**Newtypes for every identifier.** `TenantId`, `IndexId`, `Epoch`, `LaneId`, `Seq`, `ShardId`,
`SegmentId` are distinct types, not `u64`. Our key derivation mixes five such values
([`../11-design/key-layout.md`](../11-design/key-layout.md)); passing them in the wrong order is
the single most likely silent bug in the system, and newtypes make it a compile error. Cheapest
high-value discipline available.

**Typestate for protocols.** `Manifest<Draft>` cannot be published; only `Manifest<Staged>` has
`commit()`. Likewise `Segment<Open>` vs `Segment<Sealed>`. Invalid states become
unrepresentable rather than guarded.

**Generics in hot paths, `dyn` in cold ones.** Trait boundaries are what make the fake/simulator
substitution possible, but `dyn BlobStore` in the scan loop costs a virtual call per block.
Monomorphize the hot path (`fn scan<S: BlobStore>`), use `Arc<dyn BlobStore>` for wiring and
control paths. This tension is real and should be decided per seam, not by blanket policy.

**Errors are types, not codes.** `CasError::Lost` vs `CasError::Contended` is a *type-level*
distinction because conflating 412 and 409 causes rebase storms
([`../03-metadata-consistency/manifest-and-cas.md`](../03-metadata-consistency/manifest-and-cas.md) §5).
`thiserror` per crate; **no `anyhow` in libraries** — only in `pstore-server`'s binary.

## 5. The test pyramid for this system

| Layer | Tool | What it actually proves |
|---|---|---|
| Unit | `#[test]` | Pure functions: key derivation, placement, encoding |
| **Property** | `proptest` | Commit protocol linearizability, lane merge ordering, roaring ops, encode/decode round-trips |
| **Differential fuzz** | `cargo-fuzz` | SIMD kernels vs. a scalar reference — **a wrong lane index returns bad recall, not a crash** |
| **Simulation** | `pstore-testkit` (`madsim`/`turmoil`) | Distributed invariants under adversarial schedules, gray faults, partitions |
| **Conformance** | `pstore-conformance` | What each blob backend actually does (D-100) |
| Integration | `pstore-fake-s3` | Client path: wire, XML, error mapping, protocol faults |
| **Quality gates** | bench harness | Recall@10, NDCG, round-trip depth, blob-request counts, memory ceiling |

The middle three carry most of the assurance. Coverage barely sees them, which is precisely why
§6 needs a second metric.

## 6. Coverage: 95% floor, differentiated targets, mutation backstop

**Tooling:** `cargo-llvm-cov` — LLVM source-based instrumentation, tracking *region* coverage
rather than lines only, with broader platform support than tarpaulin. The current recommendation
for new projects.

> **D-110.** Differentiated targets, because a flat number under-tests the core and over-tests
> the wiring:

| Crate | Line/region | **Mutation score** |
|---|---|---|
| `pstore-engine`, `pstore-kernel`, `pstore-format` | **100%** | **≥90%** |
| `pstore-blob`, `pstore-index`, `pstore-query`, `pstore-cluster` | ≥95% | ≥80% |
| `pstore-server` (wiring) | ≥90% | ≥70% |
| **Workspace floor** | **≥95%** | ≥80% |

> **D-111.** **Mutation score is a gate, not a report.** `cargo-mutants` injects bugs and checks
> whether tests catch them; a surviving mutant in the correctness core blocks the merge. This is
> what makes the 95% meaningful rather than performative.

**Exclusion policy.** Genuinely untestable paths (allocation-failure aborts, `unreachable!` on
enum exhaustiveness) may be excluded — but each exclusion carries a comment saying *why*, and
exclusions are reviewed like code. Without this, the gate silently erodes.

**Anti-pattern, stated explicitly:** never write a test whose purpose is to touch a line. If a
test asserts nothing, mutation testing will find it, and the correct response is to delete or
strengthen it — not to add another.

## 7. Architectural invariants as executable tests

This is the part that distinguishes *this* project. We have derived hard invariants across the
research; every one is testable, and each has been asserted somewhere in the docs as though it
were documentation. It should be a test.

| Invariant | Source | Test mechanism |
|---|---|---|
| **≤3 sequential blob round trips** on any cold user query | D-34 | Depth counter in the fault-injecting store; assert |
| **RA(write batch) = 1 W** | write-path | Request-class counter assertion |
| **Zero LIST** on read/write/startup paths | Design rule 4 | `list_unrestricted` is the only listing API; assert count = 0 |
| **Invariant I1** — no in-place mutation; every transition CASes on the observed version | consistency-model | Simulation + property test |
| **Query memory = O(k + resident)**, not O(bytes scanned) | D-54 | Memory-pool assertion under a large scan |
| **Bundle recovery finds every un-folded record** | OQ-91 | Simulation under churn + node death |
| **Zero cross-AZ bytes** in steady state | az-topology | Byte counter by direction; assert zero |
| **Recall@10 ≥ target**, NDCG no regression | D-35 | CI gate on a fixed corpus |
| **SIMD kernel ≡ scalar reference** | D-98 | Differential fuzz |
| **Blob route does not traverse NAT** | D-78 | Startup assertion, plus a deployment test |

> **D-112.** Each invariant gets a named test (`invariant_roundtrip_depth`,
> `invariant_no_list_on_read_path`, …) in a dedicated module. When one fails, the failure names
> the architectural property that broke — not a line number. These are the tests that must never
> be weakened to make a change land.

## 8. Toolchain and CI gates

| Gate | Tool | Blocks merge |
|---|---|---|
| Format | `cargo fmt --check` | ✅ |
| Lints | `cargo clippy --all-targets -- -D warnings` | ✅ |
| Tests | `cargo nextest run` (faster; matters on a capped WSL2 box) | ✅ |
| Coverage | `cargo llvm-cov` vs. §6 targets | ✅ |
| **Mutation** | `cargo mutants` on changed files (full run nightly) | ✅ core crates |
| Unsafe audit | `cargo geiger` / lint config diff | ✅ |
| Deps | `cargo deny check` (advisories, licenses, bans) | ✅ |
| Miri | `cargo miri test -p pstore-kernel` | ✅ |
| Sanitizers | ASan/UBSan on `pstore-kernel` | nightly |
| **Invariants** | §7 named tests | ✅ **never waived** |
| Recall / NDCG | bench harness on fixed corpus | ✅ |
| Docs | `cargo doc --no-deps -D warnings` | ✅ |
| MSRV | pinned in `rust-toolchain.toml` | ✅ |

Mutation testing on *changed files only* per PR keeps it affordable; the full run goes nightly,
since a whole-workspace mutation run is expensive.

## 9. What we deliberately do not do

- **No ports-and-adapters ceremony.** Traits exist where substitution is genuinely needed (blob
  store, index, clock) — not one per struct.
- **No `anyhow` in libraries.** Callers must be able to match on failure modes.
- **No mocking frameworks.** Real fakes (`pstore-fake-s3`, the in-process store, the simulator)
  test real behaviour; mocks test that we called what we thought we'd call.
- **No coverage-driven test writing.** See §6.
- **No `unsafe` outside `pstore-kernel`** — compiler-enforced.
- **No premature crate splits.** A module becomes a crate when it acquires an invariant worth
  enforcing (D-108), not when a file gets long.

## 10. Open questions raised

- OQ-157 — Is 100% region coverage on `pstore-engine` realistic, or does the CAS-retry and
  error-path surface make the last few percent produce brittle tests? Revisit after M0a with
  real numbers rather than defending the target on principle.
- OQ-158 — `cargo-mutants` runtime on a workspace this size; may need per-crate scheduling to
  stay affordable even nightly.
- OQ-159 — Where exactly is the generics/`dyn` boundary? Monomorphizing `BlobStore` through the
  whole engine could hurt compile times badly on a capped box.
- OQ-160 — Should `pstore-types` exist, or do the newtypes belong with their owning layer?
  Risk is a junk drawer; the "types only, no logic, no deps" rule is the mitigation, but it
  needs enforcing.

## Sources

- [cargo-llvm-cov — GitHub](https://github.com/taiki-e/cargo-llvm-cov)
- [Test Coverage — Rust Project Primer](https://rustprojectprimer.com/measure/coverage.html)
- [How to do code coverage in Rust — rng0.io](https://blog.rng0.io/how-to-do-code-coverage-in-rust/)
- [cargo-mutants — GitHub (sourcefrog)](https://github.com/sourcefrog/cargo-mutants)
- [Mutations vs coverage — cargo-mutants documentation](https://mutants.rs/vs-coverage.html)
- [Mutation Testing — Rust Project Primer](https://rustprojectprimer.com/testing/mutations.html)
- [cargo-mutants — Thoughtworks Technology Radar](https://www.thoughtworks.com/radar/tools/cargo-mutants)
- [Workspace — Rust Project Primer](https://rustprojectprimer.com/organization/workspace.html)
- [Tips For Faster Rust Compile Times — corrode.dev](https://corrode.dev/blog/tips-for-faster-rust-compile-times/)
- [Organize Rust projects for faster compilation with Cargo workspaces — InfoWorld](https://www.infoworld.com/article/4050654/organize-rust-projects-for-faster-compilation-with-cargo-workspaces.html)
