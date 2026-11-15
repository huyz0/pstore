# M236 verification

Tests: `cargo test -p pstore-server --test it snapshot` (12 tests, M131's and M164's seven
unchanged, assertions untouched; one doc comment reworded). Suites:
`cargo test -j 3 -p pstore-engine -p pstore-server --all-features --no-fail-fast`, green but
for the two `[::1]` peer tests this container cannot run (no IPv6), as on main.

Red runs: against the parent (engine and server sources stashed, the new tests kept), five
new tests failed and one passed, as the spec says it should: `a_cached_pin_does_not_serve_history`
is a regression guard, green on the parent (which never pinned an `as_of` leg), seen red only
against its mutation. Parent failures: 6 probes for 3 strong legs over 2 lanes; 3 HEAD reads
for mixed levels; 3 for the `as_of` legs; 2 for the past faceted query; epochs `[1, 2]` for
two `bounded` tolerances.

Mutations, applied one at a time by a script and reverted: 7 of 7 killed. No shared verdict
(criterion 1, and 2's probe count); a pin serving only its own level (2); the looser
`bounded` covering the tighter (`the_tightest_tolerance_pins`); `strongest` keeping the first
leg's level (2); `as_of` reading its own HEAD under a pin (3, 4); the pin's HEAD used for
history on a cache hit (6). A first draft also cleared a weaker leg's lanes under a strong
pin; its mutation survived, because the shared verdict makes a weaker leg's probe free and
the request refuses as a whole anyway, so the clearing was dead and was removed. No
`cargo mutants` run is claimed.

1. `strong_legs_probe_each_lane_once`: green. 2 lanes registered, 2 probes (parent 6), 1 HEAD
   and 1 registry read, epochs `[2, 2, 2]`.
2. `mixed_levels_share_one_snapshot`: green. `eventual`, `bounded` 60 s and `strong` legs read
   1 HEAD and 1 registry and probe 2 lanes, every leg at epoch 2; the weaker two with the
   rival folding after the snapshot answer `[1, 1]` without its row; with the rival's write
   unfolded, `503 not_folded`. `the_tightest_tolerance_pins`: green; a cached HEAD 300 ms old
   is not served to a 100 ms leg beside a 600 s one: one HEAD read, epochs `[2, 2]`.
3. `as_of_legs_read_the_pinned_head`: green. A live leg and two `as_of` 1 legs (ranked,
   ordered) read HEAD once (parent 3), epochs `[2, 1, 1]`, each leg's results equal to the
   same query sent alone.
4. `a_past_faceted_query_reads_head_once`: green. 1 HEAD read (parent 2); `epoch` and
   `facets_epoch` 1; the facet count 2.
5. Request counts, from the counters of `strong_legs_probe_each_lane_once`,
   `mixed_levels_share_one_snapshot`, `as_of_legs_read_the_pinned_head`,
   `a_past_faceted_query_reads_head_once`, `the_tightest_tolerance_pins` and
   `a_cached_pin_does_not_serve_history`, parent then M236:

   | request | HEAD reads | registry reads | lane probes |
   |---|---|---|---|
   | 3 `strong` legs, 2 lanes | 1, 1 | 1, 1 | 6, 2 |
   | `eventual` + `bounded` + `strong` | 3, 1 | 1, 1 | 2, 2 |
   | live + 2 past-epoch legs | 3, 1 | 0, 0 | 0, 0 |
   | past-epoch query with `facets` | 2, 1 | 0, 0 | 0, 0 |
   | two `bounded`, cache 300 ms old | 1, 1 | 0, 0 | 0, 0 |
   | `bounded` (cache hit) + past-epoch past it | 1, 1 | 0, 0 | 0, 0 |

   Never more after; depth unchanged (the snapshot is the round each leg spent on HEAD). So
   no setting, per the spec skill's first case. One cost moves rather than vanishes: a
   `bounded` leg served a fresh snapshot leaves the `bounded` cache unfilled, so the next
   `bounded` request may read the HEAD this one skipped: over the two, the same count.
6. `a_cached_pin_does_not_serve_history`: green, a guard (see Red runs): with a cached HEAD at
   epoch 1 and the rival folded to 2, a `bounded` leg and an `as_of` 2 leg answer `[1, 2]` from
   one HEAD read; its mutation answers `[2, 2]`.
