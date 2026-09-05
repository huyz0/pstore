# Hot-Loop Performance: SIMD, Alignment, Streaming, and Disciplined `unsafe`

**Answers:** Q43 — *Rust techniques for the scan path, and what Polars actually does.*
**Status:** Complete (v1)
**Retrieved:** 2026-09-05

## 1. Start with the roofline, not the intrinsics

Compute is **92% of our cost** ([`../10-benchmarks-cost/cost-model.md`](../10-benchmarks-cost/cost-model.md)),
so this matters. But the first measurement reframes everything:

**768-dim RaBitQ codes, 96 B/vector, XOR + `VPOPCNTQ` + accumulate ≈ 4 cycles/vector:**

| | Throughput |
|---|---|
| Compute, per core @3 GHz | **750 M vectors/s** |
| Memory, per core @10 GB/s | **104 M vectors/s** |
| Memory, per node @60 GB/s | 625 M vectors/s |

> **Finding H-1. The scan is memory-bound by ~7× per core.** A hand-tuned kernel twice as fast
> buys nothing. **The win is fewer bytes, not faster instructions.**

Which is confirmed by simply varying the representation:

| Representation | Bytes/vector | M vectors/s @60 GB/s |
|---|---|---|
| f32 | 3,072 | 19.5 |
| f16 | 1,536 | 39.1 |
| int8 | 768 | 78.1 |
| **RaBitQ 1-bit** | **96** | **625** |

**32× on the metric that binds** — the same lever that drives the S3:cache ratio
([`../07-caching/storage-to-cache-ratio.md`](../07-caching/storage-to-cache-ratio.md)) drives
scan throughput. This is the most leveraged decision in the project and it has now paid off in
three separate places.

> **D-90.** Budget SIMD effort accordingly: get a *competent* kernel, then stop. Spend the
> effort saved on compression, cache residency, TLB behaviour, and avoiding bytes altogether.
> Micro-optimizing a memory-bound loop is the most seductive waste of time available to us.

## 2. The cheapest wins are not SIMD at all

### Huge pages — probably the single biggest untapped win

A 96 MB scan with 4 KiB pages touches **23,438 page entries** against an L2 TLB of ~1,536–2,048.
That is guaranteed thrashing on every scan. With 2 MiB huge pages: **46 entries**.

> **D-91.** Allocate scan buffers and the RAM cache from **2 MiB huge pages** (explicit
> `madvise(MADV_HUGEPAGE)` on our own arenas, not blanket THP, which has its own latency
> pathologies). Measure first — but the arithmetic says this is a large, cheap win that costs
> no code complexity.

### Batch to the register width
768 dims = 96 B = 1.5 ZMM, which is the awkward case. Batching resolves it exactly:

| Dims | B/vector | Exact-ZMM batch |
|---|---|---|
| 512 | 64 | **1** |
| **768** | 96 | **2** (192 B = 3 ZMM) |
| 1024 | 128 | 1 |
| 1536 | 192 | 1 |
| 3072 | 384 | 1 |

> **D-92.** The inner loop processes **2 vectors per iteration at 768 dims**, so every load is a
> full ZMM with no masking or tail handling. Generalize: batch = `lcm(bytes, 64) / bytes`.

### Non-temporal loads and prefetch
We score-and-drop ([`memory-management.md`](memory-management.md) D-54), so scanned blocks are
**never reused**. Filling L2/L3 with them evicts the metadata and centroids we *do* reuse.
Streaming (non-temporal) loads plus software prefetch of the next block are the right tools,
and both are cheap to try.

## 3. SIMD in Rust: what changed, and what to use

The landscape shifted recently and a lot of advice is stale.

| Approach | Portability | Control | `unsafe` |
|---|---|---|---|
| **Autovectorization** | ✅ | ✗ — fragile, silently regresses | none |
| **`std::simd`** (portable, nightly) | ✅ compiles everywhere, identical behaviour | medium | none |
| **`std::arch` intrinsics** | ✗ per-arch implementations | ✅ total | **mostly none since Rust 1.87** |
| **Abstraction crates** (`wide`, `pulp`, `fearless_simd`) | ✅ | good | contained |

**The important change: as of Rust 1.87, most `std::arch` SIMD intrinsics are callable from
safe code** when the target features are enabled at compile time — the compiler now tracks the
required instruction sets instead of making it the programmer's obligation. `safe_unaligned_simd`
covers the remainder (unaligned loads/stores).

> This substantially deflates "SIMD requires unsafe." It largely doesn't any more. Advice
> written before 2025 assumes otherwise.

### Runtime dispatch
One binary must run on AVX2, AVX-512, and NEON. Two idioms:

- **`multiversion`** — macros clone the function per target feature and dispatch. Pairs well
  with `std::simd`. Simplest.
- **Zero-sized capability tokens** (`pulp`, `fearless_simd`) — functions are generic over a
  token that *proves* the CPU supports a feature, so intrinsic calls are safe by construction;
  monomorphization generates each version. Detection happens **once at startup**, avoiding
  per-call-site checks. The acknowledged cost is ergonomic — threading tokens through call
  chains is *"an ergonomic paper cut."*

> **D-93.** Use **`simsimd` for the standard kernels** (it already has Hamming/Jaccard bit-level
> distances, which is exactly our 1-bit case, across x86/ARM/RISC-V/WASM) and reach for
> hand-written `std::arch` behind `multiversion` **only** where profiling shows simsimd's
> generic path losing. Given H-1 that should be rare. Detect CPU features **once at startup**,
> never per batch.

## 4. Memory alignment: what matters and what is cargo cult

| Concern | Alignment | Why |
|---|---|---|
| SIMD loads | **64 B** (ZMM) | Aligned loads are faster and avoid split-line penalties; Arrow standardizes on 64 B for this reason |
| **False sharing** between threads | **128 B on x86-64** | Not 64 — adjacent-line prefetch makes the effective unit 128 on x86-64 and aarch64. `crossbeam::CachePadded` handles this. |
| Struct packing | natural | Only matters for hot structs |

Two practical notes:
- `slice::align_to::<T>()` is the idiomatic way to reinterpret a byte slice as SIMD lanes: it
  returns `(prefix, aligned_middle, suffix)` so the aligned fast path and the scalar tails are
  explicit. Still `unsafe`, but bounded and auditable — far better than `transmute`.
- **Alignment only pays if the data is actually aligned.** Our blocks come from the blob store
  through the cache; the buffer allocator must guarantee 64 B, or every "aligned" load is
  silently unaligned. Assert it in debug builds.

> **D-94.** All quantized-code buffers are 64-byte aligned end-to-end — blob response → cache →
> decompressed block → kernel — with a debug assertion at the kernel boundary. Per-thread
> accumulators are `CachePadded` (128 B on x86-64).

## 5. Bounds checks and autovectorization

Bounds checks themselves are cheap; the damage is that they **block loop transforms**. When
unrolling and autovectorization are unavailable *"the impact on performance may be 5x or more."*

- `chunks_exact()` is the right primitive and std has had its internal bounds checks removed
  precisely so LLVM can optimize around it. **Prefer it to manual indexing.**
- `get_unchecked` works but is a last resort — and note it does not automatically produce
  vectorized code either.
- std uses `assume` liberally internally; it is unstable, so *"regular stable Rust has no real
  recourse"* beyond iterators and `chunks_exact`.

> **D-95.** Iterators and `chunks_exact` first; `get_unchecked` only with a benchmark in the
> commit message showing it mattered. **Verify vectorization actually happened** (`cargo-asm`,
> `llvm-mca`, or a codegen test) rather than assuming — an autovectorized loop that silently
> stops vectorizing after a refactor is a 5× regression with no test failure.

## 6. Streaming: morsel-driven parallelism

Polars' streaming engine — *"the single biggest engine rewrite the project has shipped"* — is
the model, and it maps onto a constraint we already derived independently.

Its design:
- Work is split into **morsels**, fixed-size units of ~**128k rows**.
- Workers **pull** morsels from a scheduler rather than having them pushed.
- Each operator compiles to a **Rust async state machine**, with a `WaitToken` or
  `SemaphorePermit` on every morsel giving **exact backpressure** — an operator can refuse to
  pull another morsel while it flushes.
- The result is *"a hybrid push/pull based engine that benefits from cache locality, parallelism
  and NUMA-awareness"*, with out-of-core group-by, join and sort wired to a lock-free memory
  manager and spillable sinks.

> **Finding H-2.** Morsel-driven parallelism with permit-based backpressure is *precisely* the
> mechanism [`memory-management.md`](memory-management.md) D-54 requires: query memory becomes
> O(k + resident morsels) rather than O(bytes scanned), and the permit count **is** the
> in-flight byte reservation. We derived the requirement from OOM analysis; Polars arrived at
> the same structure from query-engine scaling. Adopt the pattern rather than inventing one.

> **D-96.** The scan pipeline is morsel-driven: fixed-size morsels pulled by workers, one permit
> per morsel drawn from the query's memory reservation, releasing on drop. Morsel size is tuned
> to L2, not to row count — for us a morsel is a **block** (64 KiB–4 MiB), which is already the
> unit of fetch, decode, and cache. **One unit of work throughout the system.**

Note we do *not* need Polars' spill-to-disk sinks: our intermediates are top-k lists (tiny) and
the bulk data is re-fetchable from the blob store, so narrowing the morsel window is strictly
better than spilling ([OQ-108](../00-plan/open-questions.md)).

## 7. German strings — directly applicable to attributes and doc ids

Polars rewrote its string type around the Umbra/Hyper "German string" layout, now also in
Arrow (`BinaryView`/`StringView`), DuckDB, and Velox. A fixed **16-byte view**:

| ≤12 bytes (inline) | >12 bytes (reference) |
|---|---|
| 4 B length + 4 B prefix + 8 B remainder | 4 B length + 4 B prefix + 4 B buffer id + 4 B offset |

Why it matters to us:
- **`filter` and `gather` go from O(n·k) to O(n)** — *"completely independent from the string
  length."* Polars reports pathological cases "completely resolved"; DataFusion measured
  **20–200%** on string-heavy ClickBench queries and ~2× faster `BinaryViewArray` loading.
- The **4-byte inline prefix short-circuits failed comparisons** without touching the heap —
  most filter predicates reject on the prefix alone.
- **Document ids are ≤64 bytes and often short**, so a large fraction inline entirely: no
  indirection on the doc-id → row lookup that every query performs.

> **D-97.** Use the Arrow `BinaryView`/German-string layout for document ids and string
> attributes in the segment format. It is a format decision — expensive to retrofit, free to
> adopt now — and the filter path is exactly the O(n·k) shape it fixes.

The known cost is buffer garbage: filtered results can retain whole buffers, so a GC heuristic
is required. Polars ships one; we need the equivalent, and it interacts with our arena-per-query
model (OQ-146).

## 8. `unsafe`: discipline, not avoidance

The Polars lesson is architectural rather than about any specific trick: **unsafe is
concentrated in kernels with safe APIs on top**, not sprinkled through the engine.

| Technique | Verdict |
|---|---|
| `std::arch` intrinsics | **Largely safe since 1.87.** Use freely under `#[target_feature]`. |
| `slice::align_to` | ✅ The right way to reinterpret buffers as SIMD lanes. |
| `chunks_exact` | ✅ Safe, and already bounds-check-free in std. |
| `get_unchecked` | ⚠️ Only with a benchmark proving it mattered. |
| `MaybeUninit` + `set_len` / `spare_capacity_mut` | ⚠️ Justified for large output buffers to skip zeroing; a top source of UB. |
| `transmute` for reinterpretation | ❌ Use `align_to`. |
| `unreachable_unchecked` / `assume` | ❌ Unstable or hazardous for marginal gain. |

> **D-98.** All `unsafe` lives in `pstore-quant` and `pstore-format` kernels behind safe APIs.
> Every `unsafe` block carries a `// SAFETY:` comment stating the invariant *and* what enforces
> it. The kernel crates run under **Miri** and **ASan/UBSan** in CI and are **fuzzed** against a
> scalar reference implementation — differential fuzzing is the only realistic way to catch a
> wrong lane index, which produces plausible-but-wrong distances rather than a crash.

That last hazard is worth stating plainly: **a buggy SIMD kernel does not crash, it silently
returns bad recall.** Our recall regression gates (D-35) are the backstop, but differential
fuzzing against a scalar reference is the primary defence.

## 9. What to measure

- **Achieved memory bandwidth** in the scan loop vs. theoretical — the only number that matters
  given H-1.
- Cycles per vector; IPC; whether the loop actually vectorized (codegen test, not vibes).
- TLB miss rate before/after huge pages.
- L2/L3 miss rate — rising means non-temporal loads are missing or misapplied.
- Morsel permit wait time — the backpressure signal.
- Recall delta of every kernel against the scalar reference, continuously.

## 10. Open questions raised

- **OQ-144 (Tier 2)** — Measure the actual roofline on target instance types. H-1's "7×
  memory-bound" uses a 4-cycle estimate; if the real kernel is 12 cycles the conclusion softens
  and kernel work becomes worthwhile again.
- OQ-145 — Huge pages: real TLB win vs. allocation-latency and fragmentation cost. Explicit
  `madvise` on our arenas or THP?
- OQ-146 — German-string buffer GC interacting with arena-per-query allocation.
- OQ-147 — Non-temporal loads: do they help (less cache pollution) or hurt (losing L2 reuse
  within a morsel)? Almost certainly workload-dependent; measure.
- OQ-148 — `simsimd` generic dispatch overhead at our batch sizes (refines OQ-71); does it
  handle the 2-vector batching of D-92, or do we need our own kernel for 768 dims?
- OQ-149 — Is `std::simd` (nightly) worth the toolchain constraint versus `multiversion` +
  `std::arch` on stable? Leaning stable.

## Sources

- [The state of SIMD in Rust in 2025 — Sergey Davidoff](https://shnatsel.medium.com/the-state-of-simd-in-rust-in-2025-32c263e5f53d)
- [Safe SIMD in Rust, even on the inside — Sergey Davidoff](https://shnatsel.github.io/safe-simd-in-rust-even-on-the-inside/)
- [Towards fearless SIMD, 7 years later — Linebender (Raph Levien)](https://linebender.org/blog/towards-fearless-simd/)
- [std::arch — Rust documentation](https://doc.rust-lang.org/std/arch/index.html)
- [std::simd (portable SIMD) — Rust documentation](https://doc.rust-lang.org/std/simd/index.html)
- [Bounds Checks — The Rust Performance Book (Nicholas Nethercote)](https://nnethercote.github.io/perf-book/bounds-checks.html)
- [Carefully remove bounds checks from some chunk iterator functions — rust-lang/rust #86988](https://github.com/rust-lang/rust/pull/86988)
- [Polars in Aggregate: Streaming engine — pola.rs](https://pola.rs/posts/polars-in-aggregate-dec25/)
- [Inside Polars' Streaming Engine: How Spillable Sinks Handle Larger-Than-RAM Joins](https://python-news.com/inside-polars-streaming-engine-how-spillable-sinks-handle-larger-than-ram-joins)
- [Why we have rewritten the string data type — pola.rs](https://pola.rs/posts/polars-string-type/)
- [Using StringView / German Style Strings to Make Queries Faster, Part 1 — Apache DataFusion](https://datafusion.apache.org/blog/2024/09/13/string-view-german-style-strings-part-1/)
- [German Strings Explained — e6data](https://www.e6data.com/blog/german-strings-faster-analytics)
- [CachePadded — crossbeam documentation](https://docs.rs/crossbeam/latest/crossbeam/utils/struct.CachePadded.html)
- [Alignment and Packing — Algorithmica](https://en.algorithmica.org/hpc/cpu-cache/alignment/)
- [SimSIMD / NumKong — GitHub](https://github.com/ashvardanian/NumKong)
- Arithmetic reproducible in `09-rust-stack/simd-roofline.py`.
