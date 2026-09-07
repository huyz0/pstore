# M4d — The read cache: correctness before cleverness

**Serves:** [M4a](../M4a/SPEC.md)'s claim that a fleet change copies nothing — true, and not
free — and the mechanism every later cache decision needs
([`cache-hierarchy.md`](../../research/07-caching/cache-hierarchy.md)).

**Depends on:** `pstore-blob`'s `BlobStore` and `pstore-format`'s reader.

⚠️ **Renumbered** from M4c when hierarchical membership took that letter.

⚠️ **218 lines against the `spec` skill's 200-line cap**, and the excess is deliberate: this
is three phases' worth of delta in one document, kept together because they are one
subsystem and one ledger. The cap exists to stop a spec restating the corpus; each phase
here is ~70 lines.

## ⚠️ Scope: three phases, split under review

Two review rounds against a draft bundling all three produced **thirteen blocking findings**,
and the review budget's own rule is that the remedy at round two is to *split*:

| Phase | What | State |
|---|---|---|
| **1** | The cache: what it intercepts, how it is keyed, singleflight, and what it must refuse to cache | **shipped** |
| **2** | Class-aware admission (D-21), the `Class` hint and its plumbing, cacheable `get` for centroids | specified below |
| **3** | Shadow warming (D-44) and the post-scale-out dip | specified below |

Each phase was decomposed only when it became next — both were mis-specified in the original
draft precisely because they were written three steps ahead.

## ⚠️ Two findings that decide the design

**Outermost**, because no decorator overrides `get_ranges` and the trait default coalesces
before calling `get_range` — a cache underneath would key on spans whose shape changes with
the probe set. **It refuses `get`**, because immutability does not extend to whole-object
reads: `lanes.rs:101` reads the mutable lane registry that way. `get_immutable` is the caller
taking that judgement explicitly, which is what lets centroids be cached at all.

## Delta

**Adds** — `pstore-cache`, a `BlobStore` decorator over **`get_range`, `get_ranges` and
`get_suffix`**, the three the read path actually uses. Keyed by the **requested** range, not
the fetched one; misses pass to the inner `get_ranges` so coalescing survives beneath.
**Singleflight** on concurrent misses, which `load-and-hotspots.md` calls *mandatory, not
optional*. A byte budget with plain LRU eviction — ⚠️ plain, and labelled plain: D-21 says
admission must be class-aware and phase 1 does not implement it.

**Does not add** — class-aware admission and cacheable `get` (phase 2); warming and the dip
(phase 3); the NVMe tier and `foyer` (D-23, `NOT-RUN` — it needs a real device and until it
lands **a rolling restart still flushes every cache**); OQ-56, which asks about
S3-FIFO/W-TinyLFU and which OQ-101 refines to *real traces* we do not have.

## Acceptance criteria

⚠️ Every criterion naming a request count fixes `coalesce_gap = 256` and spaces its ranges
wider than that. At `MemoryStore`'s default of 64 KiB (`memory.rs:81`) every range in a small
fixture merges into one fetch, and a broken cache scores what a correct one scores.

**Phase 1 — the cache**

1. **A repeated read costs nothing.** A second identical `get_ranges`, `get_range` and
   `get_suffix` each issue **0** requests, counted by `Accounted` **beneath** the cache.
2. **A hit returns the bytes the store holds**, swept over identical, overlapping, adjacent
   and nested ranges against the undecorated store. ⚠️ A cache that is fast and wrong passes
   criterion 1.
3. **Overlapping probe sets share their overlap.** Two `get_ranges` of three ranges sharing
   one, spaced beyond the gap: the second issues **2** requests, not 0 and not 3.
4. **Concurrent misses collapse to one request**, forced by blocking the inner read rather
   than trusting the scheduler.
5. **The cache stays inside its budget** across a sweep admitting 10× the budget, and
   `resident_bytes` is the bytes actually held.
6. ⚠️ **`get` is never served from cache.** A `get` of a key changed under the cache returns
   the **new** bytes — the lane registry is read that way and CAS-mutated.
7. **Eviction does not corrupt**: survivors still equal the store, evicted ranges re-read
   correctly, eviction is least-recently-used, and an oversized entry is refused rather than
   ruinous.

**Phase 2 — class-aware admission (D-21)**

8. ⚠️ **Bulk traffic never evicts metadata.** After reading **10× the whole budget** in `Bulk`
   bytes, every `Pinned` and `Meta` entry still hits. This is D-21's entire claim, and the one
   a plain LRU fails.
9. **Each class is bounded by its own quota** and the total by the budget throughout that
   sweep — so criterion 8 cannot be met by never evicting anything.
10. ⚠️ **A hint survives a decorator stack.** A `Meta` read through `DepthCounting<Caching<_>>` is
    admitted as `Meta`, shown by surviving criterion 8's sweep. Observed to fail with the
    forwarding removed from one decorator.
11. **`get_immutable` caches; `get` still does not.** The centroid read is served from cache on
    repeat, and criterion 6 continues to hold unchanged.
12. **The readers actually opt in.** `Segment::open` admits its index section as `Meta` and
    `VecIndex::open` its centroids as `Pinned` — asserted by what the cache *holds* after a
    real open, never by reading the call site.

**Phase 3 — shadow warming (D-44) and the dip**

13. **Warming fetches metadata and nothing else.** After warming an index, the cache holds
    `Pinned` and `Meta` bytes and **zero** `Bulk` — D-44 is a rule about *what*, and a warm-up
    that pulls the vector section is a cache fill wearing another name.
14. **Warming costs O(1) requests per segment**, not one per document: the same **2** requests
    at 500 rows and at 5,000, asserted by the counter.
15. ⚠️ **The dip is a DEPTH reduction, measured on the first query.** A warmed node's first
    query has **sequential depth 1**; a cold node's has **≥2**. Requests saved are *reported*
    beside it, not bounded.

    Measured on the first query because that is where the dip is: the cache self-warms on
    query one, so a mean over 200 queries differs by about two requests and reports ~1.005×.

    ⚠️ And measured as **depth, not request count**, because a count ratio is not a property
    of warming at all. Warming removes a fixed **2** metadata requests while the query fans
    out to *p* probes, so the ratio is `(2 + p) / p` — 3× at one probe, 1.25× at eight.
    A first draft asserted "≥2× fewer requests" and measured **8 against 10**. Depth is what
    "cold is 30–60× warm" is actually about, and this architecture's own rule is that width is
    free and depth is not.
16. **Warming is idempotent**: warming an already-warm index issues **0** requests, so a
    repeated placement change is free rather than a second fill.

**All phases**

17. Region coverage ≥95% on shipped crates, mutation ≥80%, full gate set green.

## Test plan

| # | Test that must fail first | Mutation it catches |
|---|---|---|
| 1 | `a_second_read_of_the_same_ranges_costs_nothing` | a cache that stores and never looks up |
| 2 | `a_cached_range_equals_what_the_store_holds` | an off-by-one range key — fast, plausible, wrong bytes |
| 3 | `overlapping_probe_sets_share_their_overlap` | keying on the coalesced span instead of the requested range |
| 4 | `concurrent_misses_issue_one_request` | dedupe that works only sequentially, which is the case that never happens |
| 5 | `the_cache_stays_within_its_budget` | a budget checked but never enforced |
| 6 | `a_mutable_object_is_never_served_stale` | caching `get`, which loses a lane permanently |
| 7 | `an_evicted_range_is_re_read_correctly` | eviction that drops bytes but keeps the key |
| 8 | `bulk_traffic_does_not_evict_metadata` | one global LRU wearing a class-aware name |
| 9 | `each_class_stays_within_its_quota` | quotas that never evict, passing 8 by hoarding |
| 10 | `a_class_hint_survives_a_decorator_stack` | a defaulted hint a decorator forgets to forward |
| 11 | `an_immutable_get_is_cached_and_a_plain_get_is_not` | `get_immutable` delegating to the uncached path |
| 12 | `opening_a_segment_admits_its_index_as_meta` | a caller that hints `Bulk`, losing D-21 silently |
| 13 | `warming_fetches_no_bulk_bytes` | a warm-up that pulls the vector section |
| 14 | `warming_costs_the_same_at_any_row_count` | a warm-up that scales with documents |
| 15 | `warming_cuts_the_first_query_cost` | warming that fetches nothing, or the wrong classes |
| 16 | `warming_an_already_warm_index_is_free` | a warm-up that refetches what it just admitted |

## RA budget

| Operation | W | Rseq | Rpar | List | Depth |
|---|---|---|---|---|---|
| Cached read (hit) | 0 | **0** | 0 | 0 | **0** |
| Cached read (miss) | 0 | unchanged | unchanged | 0 | **unchanged** |
| *n* concurrent misses, one range | 0 | **1** | 0 | 0 | 1 |
| `get` (any) | 0 | unchanged | unchanged | 0 | unchanged — never cached |
| Placement change itself | 0 | 0 | 0 | 0 | 0 — nothing is copied |
| Warm one index | 0 | **2** | 0 | 0 | **2** — suffix, then centroids, fanned out |

⚠️ Warming is a **separate row** on purpose: an earlier draft claimed a placement change cost
0 *while adding warming triggered by one*. Warming has its own cost and its own completion,
and the caller decides whether to await it — in production it runs while the previous owner
still serves.

## Risks

- ⚠️ **A wrong cache is invisible**: every functional test passes while queries get bytes from
  the wrong range. Criterion 2 sweeps against the store for exactly this, and criterion 3
  exists because the obvious keying is wrong in a way criterion 1 cannot see.
- ⚠️ **Layering is load-bearing and unenforced.** `Accounted<Caching<_>>` bills every hit and
  `Caching` under anything sees coalesced spans. Nothing in the type system says so; the
  criteria assert one order and the module doc has to say why.
- **A plain LRU is not D-21.** Shipping phase 1 and calling caching done would leave bulk
  traffic evicting centroids — the failure the corpus wrote D-21 to prevent.
- **`get` stays uncached, so centroids stay uncached**, which is the highest-value class. That
  is a real cost of splitting, paid deliberately for a phase that cannot lose data.

## Phase 2 — class-aware admission (D-21)

**D-21 is the reason the cache exists in the shape it does**: a byte of centroid table is
worth thousands of bytes of raw vectors because every query needs it.

⚠️ **Names, never numbers.** `cache-hierarchy.md` numbers cache classes 1–9; `Section` ids
1–11 (`format/src/lib.rs:38-75`) mean the **opposite** — `Section::Vectors = 2` is
full-precision vectors, cache class **9** — and `OpClass` is a third. The mapping:

| `Class` | Holds | Policy |
|---|---|---|
| `Pinned` | centroid tables | own quota; **never evicted by anything else** |
| `Meta` | segment index section (via `get_suffix`), `Section::{Blocks, TermDict, Fields}` | own quota |
| `Bulk` | `Section::{Vectors, RaBitQ, Sq8, SparsePostings, Positions, FieldVectors, FieldRaBitQ, FieldSq8}` | the rest, and **the default** |

⚠️ Per-class **quotas**, not priorities: each class evicts only within itself. A priority
scheme still lets a large enough bulk burst walk the metadata out; a quota cannot.

### Adds

- `Class`, and `get_range_as` / `get_ranges_as` / `get_suffix_as` on `BlobStore`, **defaulted**
  to the unhinted calls so nothing breaks.
- `get_immutable(key, class)` — a whole-object read the caller **asserts is immutable**, which
  is what lets centroids be cached at all. ⚠️ Phase 1 refuses to cache `get` because the lane
  registry is read that way and CAS-mutated; the new method is the caller taking that
  judgement, and its name is the warning.
- **Forwarding in all eight decorators** — `accounting.rs`, `congestion.rs`, `faulty.rs`, and
  five in `pstore-testkit` (`audit`, `broken`, `depth`, `flaky`, `gated`). ⚠️ The cache is
  outermost by design, so nothing *should* sit above it — but tests compose these freely, and
  a defaulted method that a decorator does not forward arrives classless and is admitted as
  `Bulk`, disabling D-21 while every test still passes.
- Callers opt in: `Segment::open`'s suffix and index-section reads as `Meta`,
  `VecIndex::open`'s centroid read as `Pinned`. Query ranges stay `Bulk` by default.

### Does not add

Shadow warming and the dip (phase 3); the NVMe tier (D-23, still `NOT-RUN`); OQ-56, which asks
about S3-FIFO/W-TinyLFU on *real traces* we do not have.

## Tasks

| ID | Task |
|---|---|
| M4d.1 | `pstore-cache`: exact-range identity over `get_range`/`get_ranges`/`get_suffix` |
| M4d.2 | Byte budget and LRU eviction, with the budget bound asserted |
| M4d.3 | Singleflight on concurrent misses |
| M4d.4 | Refuse `get`, with the lane-registry staleness test |
| M4d.5 | `Class` and the defaulted hint methods on `BlobStore` |
| M4d.6 | Forward the hints through all eight decorators |
| M4d.7 | Per-class quotas, and eviction within a class |
| M4d.8 | `get_immutable`, and centroids cached as `Pinned` |
| M4d.9 | `Segment` and `VecIndex` opt in to their classes |
| M4d.10 | `warm`: fetch `Pinned`+`Meta` for an index, and nothing else |
| M4d.11 | The dip measurement: first-query requests, warmed against cold |
