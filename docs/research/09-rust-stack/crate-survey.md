# Rust Crate Survey

**Answers:** Q28
**Status:** Complete (v1)
**Retrieved:** 2026-09-05

## Selections

| Concern | Choice | Alternatives considered | Note |
|---|---|---|---|
| Blob store | **`object_store`** (Apache Arrow) wrapped in our `BlobStore` | `opendal`, native SDKs | See `02-object-storage/rust-object-store-crates.md`. Has `get_ranges` (coalescing) + conditional put. |
| Async runtime | **`tokio`** (multi-threaded) | `monoio`, `glommio`, `compio` | See `runtime-and-io.md`. |
| HTTP server | **`axum`** + `hyper` | `actix-web`, `poem` | Tower ecosystem, tokio-native. |
| Internal RPC | **`tonic`** (gRPC) or a custom framed protocol over `tokio` | — | Fan-out is latency-critical; measure before committing. |
| Columnar memory | **`arrow-rs`** | `arrow2` (unmaintained direction) | Official ASF implementation; interop with DataFusion/Polars/Parquet. |
| Query execution | **hand-written vectorized operators** over Arrow | `datafusion` | DataFusion is superb for SQL analytics but heavy for a 3-round-trip search path. Consider it for the *aggregation* subsystem only. |
| SIMD distances | **`simsimd`** | hand-written `std::arch`, `simdeez`, `pulp` | 100+ SIMD kernels: L2, cosine, dot, **Hamming/Jaccard bit-level** (exactly what 1-bit quantization needs), KL/JS. Covers x86/ARM/RISC-V/WASM. |
| Bitmaps | **`roaring`** (RoaringBitmap/roaring-rs) | `croaring` (C bindings), `bitvec` | Pure Rust; used for filter bitmaps and delete vectors. |
| Full-text | **`tantivy`** behind a custom `Directory` | hand-rolled | See `06-indexing/full-text-search.md`. |
| Hybrid cache | **`foyer`** | `moka`, `quick-cache`, `cachelib` (C++) | Memory + NVMe, beats `moka` in benchmarks. |
| In-memory cache | `quick-cache` / `moka` | — | For small pure-RAM caches. |
| Gossip | **`memberlist`-equivalent** (Rust port) or `foca` | build our own | Needs SWIM **+ Lifeguard**; verify Lifeguard support (OQ-66). |
| Hashing | `xxhash-rust` (xxh3), `blake3` | `ahash` (in-memory only) | xxh3 for keys/placement; blake3 for content integrity. |
| Compression | `zstd`, `lz4_flex` | `snap` | Per-block choice. |
| Serialization (internal) | **`rkyv`** or `bitcode` for zero-copy; `prost` for wire | `serde_json`, `bincode` | Manifests want zero-copy reads and forward compatibility. |
| Serialization (public API) | `serde` + JSON, plus a binary codec | — | See `11-design/api-design.md`. |
| FST / term dict | `fst` (BurntSushi) — also what tantivy uses | — | |
| Metrics | `metrics` + Prometheus exporter, `tracing` | `opentelemetry` | Per-tenant blob-request accounting is mandatory (Design rule 13). |
| Testing | `proptest`, `turmoil` (deterministic network sim), `madsim` | `loom` for concurrency | See below. |
| Allocator | **`tikv-jemallocator`** + `background_thread` | `mimalloc`, system | Decay-timer purging suits idle-between-bursts workers; best stats surface. See [memory-management](memory-management.md) §8. |
| Memory accounting | own `MemoryPool` (DataFusion-shaped) | `datafusion`'s directly | Ours must track fetch buffers, which DataFusion explicitly does not. |
| Error handling | `thiserror` (lib), `anyhow` (bin) | — | |
| Config | `figment` / `serde` | — | |

## Testing stack — disproportionately important here

The correctness of this system lives in **failure interleavings against a blob store**, not in
unit-level logic. So the test infrastructure is a first-class deliverable:

1. **A fault-injecting `BlobStore` implementation** (from `rust-object-store-crates.md` D-3):
   configurable latency distributions, 412/409/503 injection, delayed visibility, partial
   failures, and *deterministic* replay from a seed. Most of our real bugs will be found here.
2. **Deterministic simulation** (`madsim` / `turmoil`) for the cluster: run 10,000 logical
   nodes on one machine with a controlled clock and network, and assert invariants (especially
   **Invariant I1** from `03-metadata-consistency/consistency-model.md`). It must model **gray**
   failures — injected latency, partial packet loss, asymmetric partitions, slow blob responses
   confined to one AZ — not only crash-stop (D-89). A simulator that only kills nodes tests the
   easy case.
3. **Property tests** over the commit protocol: any interleaving of N committers must produce
   a linearizable epoch sequence with no lost or duplicated data.
4. **Recall regression tests** in CI (`10-benchmarks-cost/evaluation-methodology.md`).
5. **Differential fuzzing of SIMD kernels against a scalar reference**, plus Miri and
   ASan/UBSan on the kernel crates. A buggy SIMD kernel does not crash — it silently returns
   bad recall, so this is the primary defence, not a nicety
   ([`hot-loop-performance.md`](hot-loop-performance.md) §8).

> **D-32.** The simulator is built **before** the distributed features, not after. A
> masterless design's whole risk is in rare interleavings; without deterministic simulation we
> cannot claim correctness, only hope for it.

## Workspace shape

```
pstore/
  crates/
    pstore-blob/        BlobStore trait, backends, congestion control, request accounting
    pstore-format/      Segment format: encode/decode, footer, blocks, zone maps
    pstore-manifest/    HEAD, manifest, CAS commit protocol, epochs
    pstore-wal/         Lanes, group commit, tail discovery, lane bitmap
    pstore-index-vec/   SPANN-family clustered index, LIRE maintenance
    pstore-index-fts/   Tantivy Directory + BM25/sparse
    pstore-quant/       RaBitQ, int8 SQ, SIMD kernels
    pstore-cache/       foyer wrapper, class-aware admission
    pstore-cluster/     gossip, LRH placement, work assignment
    pstore-query/       planner + vectorized execution
    pstore-api/         HTTP/gRPC surface, auth, quotas
    pstore-node/        the binary: one binary, all roles
    pstore-sim/         deterministic simulation harness
    pstore-fake-s3/     S3 fake on `s3s`: exact AWS semantics + protocol fault injection
                        (standalone; no dependency on pstore internals)
    pstore-conformance/ backend capability probes; populates the Capabilities matrix
    pstore-bench/       benchmark + recall harness
```

**One binary, all roles.** Any node can serve, index, compact, and GC
(`04-cluster/ownership-and-leases.md`). Roles are runtime *biases* advertised in gossip, not
separate deployables. This keeps ops trivial at 10,000 nodes.

## Open questions raised

- OQ-66: Is there a maintained Rust SWIM implementation with Lifeguard? (`foca`, `chitchat`
  from Quickwit — evaluate both; `chitchat` is used at Quickwit's scale and is a strong
  candidate.)
- OQ-67: `rkyv` vs `bitcode` vs flatbuffers for the manifest — needs forward/backward
  compatibility rules first.
- OQ-68: Is DataFusion worth adopting for aggregations, or does it drag in too much?

## Sources

- [apache/arrow-rs — GitHub](https://github.com/apache/arrow-rs)
- [simsimd — crates.io](https://crates.io/crates/simsimd/4.3.0)
- [SimSIMD / NumKong — GitHub](https://github.com/ashvardanian/NumKong)
- [RoaringBitmap/roaring-rs — GitHub](https://github.com/RoaringBitmap/roaring-rs)
- [foyer-rs/foyer — GitHub](https://github.com/foyer-rs/foyer)
- [object_store — docs.rs](https://docs.rs/object_store/latest/object_store/)
- [What is Tantivy? — Spice AI](https://spice.ai/learn/tantivy)
