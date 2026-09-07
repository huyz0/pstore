# M4d — The read cache: correctness before cleverness

**Serves:** [M4a](../M4a/SPEC.md)'s claim that a fleet change copies nothing — true, and not
free — and the mechanism every later cache decision needs
([`cache-hierarchy.md`](../../research/07-caching/cache-hierarchy.md)).

**Depends on:** `pstore-blob`'s `BlobStore` and `pstore-format`'s reader.

⚠️ **Renumbered** from M4c when hierarchical membership took that letter.

## ⚠️ Scope: this is phase 1 of three, and it was split under review

Two review rounds against a draft that bundled the cache, class-aware admission (D-21) and
shadow warming produced **thirteen blocking findings**. The review budget's own rule is that
the remedy at round two is to *split*, not to argue a third round. So:

| Phase | What | State |
|---|---|---|
| **1 — this spec** | The cache: what it intercepts, how it is keyed, singleflight, and what it must refuse to cache | specified |
| 2 | Class-aware admission (D-21/D-22), the `Class` hint and its plumbing, cacheable `get` for centroids | named only |
| 3 | Shadow warming (D-44) and the post-scale-out dip | named only |

Phases 2 and 3 are deliberately **not** decomposed here: the spec skill says decompose only
what is next, and both were mis-specified precisely because they were written three steps
ahead.

## ⚠️ Two findings that decide the design

**Where it sits: outermost.** No decorator in this tree overrides `get_ranges` —
`accounting.rs:189`, `congestion.rs:125`, `faulty.rs:141`, and five in `pstore-testkit`, all
inherit the trait default. That default *coalesces* and then calls `get_range`
(`store.rs:25-50`), so a cache placed underneath sees only coalesced spans, whose shape
changes with the probe set: two queries touching the same posting list would share nothing.

So the cache is the **outermost** decorator, with accounting beneath it —
`Caching<Accounted<_>>`, never the reverse. A hit then issues no counted request, which is
also the only layering in which criterion 1 can be asserted at all.

**What it must refuse: `get`.** Segment immutability is what makes a cache entry correct
forever, and it does **not** extend to whole-object `get`: `lanes.rs:101` reads the lane
registry with `get`, and `lanes.rs:77-95` CAS-mutates it. Caching that would make a newly
registered lane permanently invisible and its bundles unrecoverable — silent data loss, from
a cache that passes every hit-rate test. **`get` is not cached in this phase**, and centroids
(which use it, `vec_index.rs:301`) wait for phase 2 and an explicit immutability marker.

## Delta

**Adds**
- `pstore-cache`: a `BlobStore` decorator over **`get_range`, `get_ranges` and `get_suffix`**
  — the three the read path actually uses (`reader.rs:31,70,219,247,283,324,468`,
  `vec_index.rs:401`).
- Keyed by the **requested** range, not the fetched one; misses pass to the inner
  `get_ranges` so coalescing survives beneath.
- **Singleflight**: concurrent misses for one range collapse into one request.
  `load-and-hotspots.md` calls this *mandatory, not optional*.
- A byte budget with plain LRU eviction. ⚠️ Plain, and labelled plain: D-21 says admission
  must be class-aware, and phase 1 does **not** implement it. Calling this "the cache" and
  stopping would be the defect D-21 exists to prevent.

**Does not add** — class-aware admission and the `Class` hint (phase 2); warming and the dip
(phase 3); cacheable `get` (phase 2); the NVMe tier and `foyer` (D-23, `NOT-RUN` — it needs a
real device and this environment has none, and until it lands **a rolling restart still
flushes every cache**); OQ-56, which asks about S3-FIFO/W-TinyLFU and which OQ-101 refines to
*real traces* we do not have.

## Acceptance criteria

⚠️ Every criterion naming a request count fixes `coalesce_gap = 256` and spaces its ranges
wider than that. At `MemoryStore`'s default of 64 KiB (`memory.rs:81`) all the ranges in a
small fixture merge into one fetch, and a broken cache scores the same as a correct one.

1. **A repeated read costs nothing.** A second identical `get_ranges`, `get_range` and
   `get_suffix` each issue **0** requests, counted by `Accounted` **beneath** the cache.
2. **A hit returns the bytes the store holds**, swept over identical, overlapping, adjacent
   and nested ranges against the undecorated store. ⚠️ A cache that is fast and wrong passes
   criterion 1.
3. **Overlapping probe sets share their overlap.** Two `get_ranges` of three ranges sharing
   one, spaced beyond the gap: the second issues **2** requests, not 0 and not 3. A cache
   keyed on the coalesced span scores 3 here and 0 on criterion 1, so the pair pins it.
4. **Concurrent misses collapse to one request** — *n* tasks on one range issue exactly 1,
   forced with `Gated` rather than hoped for.
5. **The cache stays inside its budget**: resident bytes ≤ the configured maximum across a
   sweep that admits 10× the budget.
6. ⚠️ **`get` is never served from cache.** A `get` of a key whose bytes were changed under
   the cache returns the **new** bytes. This is the lane-registry landmine as a test.
7. **Eviction does not corrupt.** After the sweep in 5, every surviving entry still equals the
   store, and every evicted one re-reads correctly.
8. Region coverage ≥95% on shipped crates, mutation ≥80%, full gate set green.

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

## RA budget

| Operation | W | Rseq | Rpar | List | Depth |
|---|---|---|---|---|---|
| Cached read (hit) | 0 | **0** | 0 | 0 | **0** |
| Cached read (miss) | 0 | unchanged | unchanged | 0 | **unchanged** |
| *n* concurrent misses, one range | 0 | **1** | 0 | 0 | 1 |
| `get` (any) | 0 | unchanged | unchanged | 0 | unchanged — never cached |
| Placement change | 0 | 0 | 0 | 0 | 0 — nothing is copied, and nothing is warmed yet |

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

## Tasks

| ID | Task |
|---|---|
| M4d.1 | `pstore-cache`: exact-range identity over `get_range`/`get_ranges`/`get_suffix` |
| M4d.2 | Byte budget and LRU eviction, with the budget bound asserted |
| M4d.3 | Singleflight on concurrent misses |
| M4d.4 | Refuse `get`, with the lane-registry staleness test |
