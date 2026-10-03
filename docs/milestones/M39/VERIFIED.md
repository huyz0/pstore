# M39 — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ **Where this ran:** a Linux x86-64 cloud container, on `MemoryStore`, `Accounted` and M17's
unreliable store, under paused tokio time. Every number is a request count or a test outcome.

Commands: `cargo test -p pstore-engine --test lane_taken` and
`cargo test -p pstore-server --test scheduled_reap`.

⚠️ **A test that fails to compile is not counted as red.** `with_lane_recheck` is new, so these
tests do not compile on the parent. Each criterion names the hand mutation seen to fail it.

1. **An idle writer is refused, not lost.** `an_idle_writer_is_refused_not_lost`: A idles 11 s
   past a 10 s bound, and B writes, folds and reaps sequence 1 on A's lane. A's flush is
   refused with `LaneTaken` at sequence 1, and bundle 1 does not exist.
   - Killed: the recheck dropped. A then created sequence 1 below the watermark, the
     residue the row names.
   - 1b: `a_late_landed_write_is_not_taken_by_the_recheck`. On the unreliable store, A's write
     that timed out, landed late and was folded is resolved as its own. The flush succeeds,
     and a fold holds `r0`, `r1` and `r2` once each.
     - Killed: the recheck placed after M17's resolution loads the watermark, which gave
       `LaneTaken` (spec review's major 1).
2. **The recheck is once per window.** `the_lane_recheck_is_once_per_window`:
   - flushes at 5, 10, 11 and 21 s cost 0, 1, 0 and 1 reads, and the boundary rechecks;
   - an empty flush past the window reads nothing;
   - the first flush costs exactly an unbounded engine's;
   - an unbounded engine reads nothing after its resume, an hour later.
   - Killed: `>=` as `>`, the recheck's instant not recorded, the resume's instant not
     recorded, the idle guard dropped, and the resumed guard dropped.
3. **The server wires it.**
   - `the_lane_recheck_is_half_the_reap_age_on_or_off`: `PSTORE_GC_AGE_S=60` gives 30 s with
     the reap on or off, and unset gives 30 minutes. Killed: the whole age.
   - `the_server_rechecks_within_half_the_reap_age`: an `Api` given 30 s builds engines whose
     flush 29 s after their last read issues no HEAD read, and whose flush at 31 s issues one.
     Killed: the bound not passed to the engines.
4. **Gates.**
   - `./scripts/gates.sh`: green by the pre-commit hook on M39.1, and on this ledger's commit.
   - The sweep over M39's source diff: 29 mutants over `4fc726a..9639dcd` (code review then changed one test comment only), 13 caught, 14 unviable, 2 timed out (a flush made to do nothing leaves a test waiting), **0 missed**.
   - Hand mutations: 9, all killed.

**Residue (stated in the spec's risks):**
- a suspended host's monotonic clock under-counts idle time, with no revealer;
- a fleet with mixed `PSTORE_GC_AGE_S`, with no revealer;
- the manual epoch-based reap is outside the time guarantee. There, the next fresh HEAD read
  refuses the lane loudly.
