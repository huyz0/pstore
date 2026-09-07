# M4d — Verified (phase 1)

One line per acceptance criterion in [SPEC.md](SPEC.md), naming the test or command that
demonstrated it.

Gate: `scripts/check-verified.py`.

⚠️ **This is phase 1 of three, and caching is not done.** Eviction is a plain LRU; **D-21
requires class-aware admission** so a burst of bulk traffic cannot evict the centroid table
every query needs, and this does not implement it. Phases 2 (class-aware admission) and 3
(shadow warming, the post-scale-out dip) are specified only by name, deliberately.

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
8. **Coverage, mutation and gates.** `./scripts/coverage.sh --fail-under-regions 95` →
   **95.60%** region, 97.34% line. `cargo mutants -p pstore-cache --timeout 60` → **28 caught,
   2 missed = 93.3%**, from 67.9% before the tests above; the one remaining behavioural
   survivor was killed by criterion 7's duplicate-range test, verified by hand. `cargo fmt
   --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test --workspace`,
   `cargo deny check`, `scripts/check-links.sh`, `scripts/build-index.py --check`,
   `scripts/check-verified.py` all green.

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
