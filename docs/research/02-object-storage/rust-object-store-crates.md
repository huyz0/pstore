# Rust Blob-Store Client Options

**Answers:** Q7
**Status:** Complete (v1)

## Candidates

| Crate | Backends | CAS | Verdict |
|---|---|---|---|
| **`object_store`** (Apache Arrow) | S3, GCS, Azure, local, memory, HTTP/WebDAV | ✅ `PutMode::Create` / `PutMode::Update(etag)` | **Primary candidate.** |
| `opendal` (Apache) | ~50 backends | ✅ | Wider reach, measurably slower on large S3 uploads; heavier. Consider only if we need exotic backends. |
| `aws-sdk-s3` + `google-cloud-storage` + `azure_storage_blobs` | native each | ✅ | Maximum control, 3× the integration surface, no unified abstraction. |
| Hand-rolled on `hyper`/`reqwest` | any | ✅ | Only worth it if `object_store` blocks us on a hot-path optimization. |

## Why `object_store` is the default choice

- Exactly the portable primitive set we identified: `put_opts` with `PutMode`, `get_opts`
  with `range` / `if_match` / `if_none_match` / `version`, `get_ranges` (vectored,
  coalescing), `put_multipart_opts`, `delete_stream`.
- **`get_ranges` already implements Pattern 2 (range coalescing)** — it merges nearby ranges
  and fetches in parallel. That is our single most important read primitive, already
  written and battle-tested.
- Conditional put maps to `If-None-Match: *` / `If-Match` on S3 and Azure, and to
  `ifGenerationMatch` on GCS — our CAS primitive, portable, one API.
- Apache governance, small dependency footprint, used by InfluxDB IOx, DataFusion, crates.io.
- Faster than OpenDAL in published comparisons for large S3 uploads.

## Gaps we will likely have to fill ourselves

These are the reasons we wrap it rather than use it directly:

1. **Adaptive concurrency + `503 SlowDown` handling per prefix-group.** `object_store` retries,
   but not with a shared, prefix-aware congestion controller. We need one (Design rule 6).
2. **Request accounting.** We need per-op-class counters (W/R/List) wired to metrics and to
   per-tenant cost attribution. This is a first-class product feature, not an afterthought.
3. **Storage-class routing.** WAL → S3 Express directory bucket, data → Standard, within one
   logical store (Pattern 9). Needs a composite store that routes by key prefix.
4. **Capability probing.** The `Capabilities` struct from `api-semantics.md` §6.
5. **Tuning the coalescing threshold `G*`** — `get_ranges` has a fixed policy; we want it
   backend- and workload-adaptive.
6. **Zero-copy / `Bytes` discipline** end-to-end into the cache and the decoder, avoiding a
   copy per block.

## Proposed shape

```rust
/// The only way pstore touches durable storage.
#[async_trait]
pub trait BlobStore: Send + Sync + 'static {
    fn capabilities(&self) -> &Capabilities;

    async fn get_range(&self, key: &Key, range: Range<u64>) -> Result<Bytes>;
    /// Coalescing + parallel fan-out. The workhorse (Pattern 2).
    async fn get_ranges(&self, key: &Key, ranges: &[Range<u64>]) -> Result<Vec<Bytes>>;

    async fn put(&self, key: &Key, body: Bytes) -> Result<PutOutcome>;
    async fn put_multipart(&self, key: &Key) -> Result<Box<dyn MultipartSink>>;

    /// CAS. `Precondition::NotExists` or `Precondition::Match(Tag)`.
    async fn put_conditional(&self, key: &Key, body: Bytes, pre: Precondition)
        -> Result<PutOutcome, CasError>;   // CasError::Lost | ::Contended(409) | ::Io

    async fn delete_batch(&self, keys: &[Key]) -> Result<()>;

    /// Permitted only behind `#[allow(list_scan)]` — GC, DR, admin.
    async fn list_unrestricted(&self, prefix: &Key) -> BoxStream<Result<ObjectMeta>>;
}
```

The `list_unrestricted` naming and lint gate are deliberate: making the expensive operation
awkward to call is the cheapest possible enforcement of Design rule 4.

## Decisions

- **D-1:** Build `pstore-blob` as a thin wrapper over `object_store`, exposing `BlobStore`.
- **D-2:** No direct `object_store` usage above `pstore-blob`. Everything goes through the
  trait so request accounting and congestion control are unavoidable.
- **D-3:** Keep an in-memory + local-fs `BlobStore` impl with **injectable latency and
  fault behaviour** (412s, 409s, 503s, delayed visibility). Most of our correctness testing
  depends on being able to simulate a nasty blob store deterministically.

> **Promoted.** With no cloud accounts and **no emulator implementing `If-None-Match: *`
> faithfully** (MinIO requires an exact ETag; Azurite and SeaweedFS have their own divergences),
> this is not a testing aid — it is the **primary correctness vehicle**, the only backend whose
> semantics we control and can assert. Emulators test plumbing; real clouds test economics.
> See [`../09-rust-stack/dev-and-test-environment.md`](../09-rust-stack/dev-and-test-environment.md).

## Sources

- [object_store — docs.rs](https://docs.rs/object_store/latest/object_store/)
- [ObjectStore trait — docs.rs](https://docs.rs/object_store/latest/object_store/trait.ObjectStore.html)
- [apache/arrow-rs-object-store — GitHub](https://github.com/apache/arrow-rs-object-store)
- [Rust Object Store — lib.rs](https://lib.rs/crates/object_store)
- [opendal #5929: slower than object_store for large S3 uploads](https://github.com/apache/opendal/issues/5929)
