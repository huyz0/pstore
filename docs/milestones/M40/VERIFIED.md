# M40 — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ **Where this ran:** a Linux x86-64 cloud container, on `MemoryStore`. Every result is a
test outcome.

Commands: `cargo test -p pstore-server --test filters --test distance` and
`cargo test -p pstore-engine --test filters`.

⚠️ **A test that fails to compile is not counted as red.** Each criterion names a red on the
parent, or the hand mutation that was seen to fail it.

1. **The API refuses a reserved name.** `a_filter_naming_a_reserved_attribute_is_refused`:
   - each of these answers 400 with "attribute … is reserved" naming the attribute:
     - `["$metric", "Eq", 2]`;
     - `$text` nested under `Not` inside `And`;
     - the empty name.
   - A patch by filter and a delete by filter naming `$op` are refused for that reason and
     write nothing.
   - An ordinary attribute in the same positions is still read.
   - Red on the parent: 200.
   - Killed: the refusal dropped, the empty name allowed, and an exact name instead of the
     `$` prefix.
   - `the_metric_a_row_carries_is_never_returned_or_filtered_on` changes from 200/[] to 400,
     as the spec declares. Its assertion that no result carries `$metric` is unchanged.
2. **A scan's filter sees what it serves.** `a_scan_filters_what_it_serves`: a euclidean row
   scanned with `Eq("$metric", euclidean's code)` matches nothing, unfolded or folded, and an
   unfiltered scan finds it.
   - Red on the parent: the unfolded row matched.
   - Killed: the filter applied before stripping.
3. **Gates.**
   - `./scripts/gates.sh`: green by the pre-commit hook on M40.1, and on this ledger's commit.
   - The sweep over M40's source diff: 5 mutants over `d29a4d6..cb1071c` (code review then tightened two test assertions only), 3 caught, 2 unviable, **0 missed**.
   - Hand mutations: 4, all killed.

**Left open, BACKLOG row 56 narrowed to it:** a library engine's fresh view indexes its own text
field while the schema records none, so a default engine's fresh view can match a `body`
writer's ordinary `text` attribute until a fold. No server reaches it, since every server
engine uses the default field.
