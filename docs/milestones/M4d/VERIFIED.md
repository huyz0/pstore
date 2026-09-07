# M4d — Verified (phase 1)

One line per acceptance criterion in [SPEC.md](SPEC.md), naming the test or command that
demonstrated it.

Gate: `scripts/check-verified.py`.

⚠️ **All three phases are done. The NVMe tier (D-23) remains `NOT-RUN`**, so a rolling
restart still flushes every cache — `affinity-and-coldstart.md` puts a full refill at ~10
hours per node, and that is a real correctness gap for deploys, not a tidy omission.

1. **A repeated read costs nothing.** `a_second_read_of_the_same_ranges_costs_nothing`
   (`cargo test -p pstore-cache`), covering `get_ranges`, `get_range` and `get_suffix`, with
   `Accounted` **beneath** the cache — the only layering in which a hit is observable as zero.
2. **A hit returns the bytes the store holds.** `a_cached_range_equals_what_the_store_holds`
   over identical, overlapping, adjacent and nested ranges, and
   `a_cached_suffix_equals_what_the_store_holds`. ⚠️ The suffix test exists because mutation
   testing showed `get_suffix` could return an **empty buffer** and pass criterion 1 — the
   earlier test counted requests and never looked at the bytes.
3. **Overlapping probe sets share their overlap.** `overlapping_probe_sets_share_their_overlap`
   — the second of two calls sharing one range of three issues **2** requests. Observed to
   fail against a cache keyed on the coalesced span, which scores 3.
4. **Concurrent misses collapse to one request.** `concurrent_misses_issue_one_request`, eight
   racing callers reaching the store **once**. Observed to fail with the in-flight claim
   disabled: `left: 2, right: 1`.
5. **The cache stays inside its budget.** `the_cache_stays_within_its_budget`, plus
   `resident_bytes_is_the_bytes_actually_held` — ⚠️ added because `<= budget && > 0` is
   satisfied by returning the constant 1, which mutation testing duly did.
6. **`get` is never served from cache.** `a_mutable_object_is_never_served_stale`. Observed to
   fail when `get` was cached. This is the lane registry: read with `get` (`lanes.rs:101`) and
   CAS-mutated (`lanes.rs:77-95`), so a cached one makes a newly registered lane permanently
   invisible and its bundles unrecoverable.
7. **Eviction does not corrupt.** `an_evicted_range_is_re_read_correctly`,
   `eviction_is_least_recently_used`, `an_entry_larger_than_the_budget_is_refused_not_ruinous`,
   and `a_repeated_range_within_one_call_fills_both_positions`.
8. ⚠️ **Bulk traffic never evicts metadata — D-21's entire claim.**
   `bulk_traffic_does_not_evict_metadata`: `Pinned` and `Meta` entries admitted, then **10×
   the whole budget** read as `Bulk`, then both metadata entries served with **zero** further
   requests. Observed to fail with the three arenas collapsed into one: *"bulk traffic evicted
   metadata: this is a global LRU wearing a class-aware name"*.
9. **Each class is bounded by its own quota**, and the total by the budget, checked on every
   admission of a 120-read sweep — `each_class_stays_within_its_quota`. Without it, criterion
   8 is satisfiable by never evicting anything.
10. ⚠️ **A hint survives a decorator stack.** `a_class_hint_survives_a_decorator_stack`, a
    `Pinned` read through `DepthCounting<Caching<_>>`. Observed to fail with that decorator's
    forwarding deleted: *"the hint was stripped on the way through the stack and admitted as
    Bulk"*. All **eight** decorators forward — `accounting`, `congestion`, `faulty`, and five
    in `pstore-testkit`.
11. **`get_immutable` caches and `get` does not.**
    `an_immutable_get_is_cached_and_a_plain_get_is_not` — the whole-object read is free on
    repeat, and criterion 6 still holds in the same test.
12. ⚠️ **The readers actually opt in**, asserted by what the cache *holds* after a real
    `Segment::open` rather than by reading the call site —
    `opening_a_segment_admits_its_index_as_meta`. Observed to fail with the hint changed to
    `Bulk`: *"the index section is Bulk, and a scan burst will evict what every query on this
    segment needs"*. `Segment::open` hints `Meta` for both its suffix and its large-index
    path; `VecIndex::open` hints `Pinned` for centroids via `get_immutable`.
13. **Warming fetches metadata and nothing else.** `warming_fetches_no_bulk_bytes`
    (`cargo test -p pstore-index --test warming`): after warming, the cache holds `Meta` and
    `Pinned` bytes and **zero** `Bulk`. Observed to fail with `warm` stubbed to a no-op.
14. **Warming costs O(1) per segment**, not one request per document:
    `warming_costs_the_same_at_any_row_count` — the same count at 600 rows and at 3,000, and
    ≤3 requests for one segment.
15. ⚠️ **The dip, as a depth reduction on the first query.**
    `warming_cuts_the_first_query_cost`, measured with `DepthCounting` beneath the cache: a
    cold first query takes **2** sequential rounds, a warmed one **1**. Requests are reported
    beside it — 3 warmed against 3 cold at this fixture size — and deliberately **not**
    bounded; see below.
16. **Warming is idempotent.** `warming_an_already_warm_index_is_free` — a second warm issues
    **0** requests, so a repeated placement change is free rather than a second fill.
17. **Coverage, mutation and gates.** `./scripts/coverage.sh --fail-under-regions 95` →
    **95.36%** region, 97.08% line; `pstore-cache` itself 96.34%, `vec_index.rs` 93.92%.
    `cargo mutants -p pstore-index --file crates/pstore-index/src/vec_index.rs --timeout 120`
    → **55 caught, 3 missed = 94.8%**, with **no warming-related survivors**; the three are
    pre-existing arithmetic in the index builder and the search path.
    `cargo mutants -p pstore-cache --timeout 60` → **38 caught, 1 missed = 97.4%** after phase
    2, against a floor of 80%. Phase 1 alone went 67.9% → 93.3%, and every point of both
    climbs was a real hole: the suffix read could return an **empty buffer** and still pass
    the request-count test, the resident-byte total could return a constant 1 and satisfy
    "≤ budget and > 0", LRU touch-on-hit could be deleted outright, a range requested twice in
    one call filled only one slot, and an entry exactly the size of the budget was refused. `cargo fmt
    --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test --workspace`,
    `cargo deny check`, `scripts/check-links.sh`, `scripts/build-index.py --check`,
    `scripts/check-verified.py` all green.

## ⚠️ The dip is a depth reduction, not a request-count ratio

The spec first asked for **"≥2× fewer requests"** on the first query after warming, and
measured **8 against 10**. The criterion was wrong, not the code: warming removes a **fixed
2** metadata requests while the query fans out to *p* probes, so any count ratio is
`(2 + p) / p` — 3× at one probe, 1.25× at eight. It is a property of the probe count.

What warming actually removes is a **data-dependent chain**: a cold query cannot know which
posting lists to read until it has the centroids, so it walks {footer, centroids} and only
then the lists. Warming moves that chain off the query path, and sequential depth goes from
2 to 1. That is what "cold is 30–60× warm" measures, and this architecture's own rule is that
width is free and depth is not.

## What review caught before any code was written

Two rounds against the draft produced **thirteen blocking findings**, and the two that decided
the design would each have shipped:

⚠️ **The cache was specified under `get_range`, which the read path barely uses.** Metadata
arrives by `get_suffix` (`reader.rs:31`) and whole-object `get` (`vec_index.rs:301`), and
queries use `get_ranges` (`reader.rs:219,324,468`). Worse, **no decorator overrides
`get_ranges`**, so the trait default coalesces at the *top* of the stack and calls
`get_range` — a cache underneath would have keyed on coalesced spans, whose shape changes with
the probe set, and two queries touching the same posting list would have shared nothing. The
cache is now the outermost decorator.

⚠️ **Caching `get` would have lost data.** Segment immutability is what makes an entry correct
forever and it does not extend to whole-object reads. Criterion 6 is that finding as a test.

Also caught: "class" collided with `Section` ids meaning the opposite thing and with
`OpClass`; **OQ-56 was misquoted** as "vs a plain LRU" when the corpus says
S3-FIFO/W-TinyLFU and OQ-101 refines it to real traces we do not have, so answering it was
dropped rather than answered against a weaker opponent; and the dip criterion's ≥2× was
unachievable because the cache self-warms on the first query, so both arms converge.

## What this milestone got wrong

⚠️ **A forced race that forced nothing.** The singleflight test used `tokio::sync::Barrier`
with **n=1**, which returns immediately — whether the eight callers overlapped was the
scheduler's business. It passed normally and **failed under coverage instrumentation**, which
is a flaky test announcing itself. The inner read now blocks until the test releases it, so
every caller has provably entered the cache before the single fetch completes.

⚠️ **Request counts that could not discriminate.** `MemoryStore` defaults to a 64 KiB
`coalesce_gap` (`memory.rs:81`), at which every range in a small fixture merges into one
fetch — and a cache keyed on the coalesced span scores exactly what a correct one scores.
Every criterion naming a count now fixes the gap at 256 and spaces its ranges wider.

## Not run

**The NVMe tier and `foyer`** (D-23). ⚠️ D-23 calls persistence *mandatory, not an
optimization*, and until it lands **a rolling restart still flushes every cache** —
`affinity-and-coldstart.md` puts a full refill at ~10 hours per node. It needs a real device
to mean anything and this environment has none. `NOT-RUN`, named rather than omitted.
