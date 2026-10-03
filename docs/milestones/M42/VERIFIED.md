# M42 — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ **`provisional`:** a Linux x86-64 cloud container, on an in-memory store. Bytes and requests
are exact, and latency is not measured. A real bucket (M0b) is what would price the round trip.

1. **The table is reproduced, and pinned.** `cargo run --release -p pstore-engine --example id_round`
   exits 0, and its assertions of the pinned totals over 20 queries hold (100,000 rows, p = 8,
   gap 0): candidates 688,215, total bytes 112,243,752, round-4 bytes 1,678,932, and depth 4 on
   every query. Every planning row reproduced exactly. The run, as printed:

| rows | path | gap | p | candidates | leg req | leg B | round-4 req | round-4 B | total req | total B | code rows fetched | +ids beside codes B | net after dropping round 4 |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 20000 | exact | 0 | - | 20000 | 1.0 | 10240000 | 9.8 | 27425 | 12.8 | 10276058 | 20000 | 740000 (+7.2%) | +6.9% |
| 20000 | exact | 1MiB | - | 20000 | 1.0 | 10240000 | 1.0 | 746948 | 4.0 | 10995581 | 20000 | 740000 (+6.7%) | -0.1% |
| 100000 | ivf | 0 | 2 | 8508 | 4.0 | 1361272 | 7.5 | 84510 | 14.5 | 1468303 | 8508 | 314794 (+21.4%) | +15.7% |
| 100000 | ivf | 1MiB | 2 | 8508 | 3.1 | 1691765 | 1.1 | 198880 | 7.2 | 1913166 | 19634 | 726452 (+38.0%) | +27.6% |
| 100000 | ivf | 0 | 8 | 34411 | 16.0 | 5505720 | 7.5 | 83947 | 26.4 | 5612188 | 34411 | 1273198 (+22.7%) | +21.2% |
| 100000 | ivf | 1MiB | 8 | 34411 | 4.2 | 8361120 | 1.1 | 207331 | 8.2 | 8590972 | 87717 | 3245540 (+37.8%) | +35.4% |
| 100000 | ivf | 0 | 32 | 100000 | 50.0 | 16000000 | 7.5 | 83947 | 60.5 | 16106468 | 100000 | 3700000 (+23.0%) | +22.5% |
| 100000 | ivf | 1MiB | 32 | 100000 | 1.0 | 16000000 | 1.1 | 207331 | 5.1 | 16229852 | 100000 | 3700000 (+22.8%) | +21.5% |
| 400000 | ivf | 0 | 2 | 11411 | 4.0 | 1825768 | 4.7 | 209529 | 11.7 | 2100118 | 11411 | 422209 (+20.1%) | +10.1% |
| 400000 | ivf | 1MiB | 2 | 11411 | 3.6 | 1924902 | 1.0 | 227553 | 7.7 | 2217276 | 13758 | 509028 (+23.0%) | +12.7% |
| 400000 | ivf | 0 | 8 | 49248 | 16.0 | 7879736 | 4.7 | 209529 | 23.6 | 8154086 | 49248 | 1822189 (+22.3%) | +19.8% |
| 400000 | ivf | 1MiB | 8 | 49248 | 10.0 | 10015844 | 1.0 | 227553 | 14.0 | 10308218 | 122994 | 4550796 (+44.1%) | +41.9% |
| 400000 | ivf | 0 | 32 | 184432 | 64.0 | 29509200 | 4.7 | 209529 | 71.7 | 29783550 | 184432 | 6824002 (+22.9%) | +22.2% |
| 400000 | ivf | 1MiB | 32 | 184432 | 11.7 | 39861737 | 1.0 | 227553 | 15.7 | 40154111 | 391373 | 14480797 (+36.1%) | +35.5% |

   - "Candidates" on the exact-path rows is every row of the segment.
   - The ids column is (36 + 1) bytes times the code rows fetched. At 1 MiB that is an upper
     bound: ids at 37 bytes a row would bridge fewer rows than codes at 24 (code review).
2. **The self-check was seen red.** With every round-4 request dropped from the harness's log,
   `cargo run --release -p pstore-engine --example id_round` panicked with "the log holds 3
   requests and Accounted counted 13". The file was then restored byte for byte.
3. **Row 26 records the price and the recommendation.** `grep -n "priced by \[M42\]" docs/milestones/BACKLOG.md`
   finds it.
4. **Gates.** `./scripts/gates.sh`: green by the pre-commit hook on M42.1, and on this ledger's
   commit.

**What the run corrected in the spec** (code review; the spec is amended, never silently replaced):
- the penalty is about a fifth only at gap 0. At the production backend's 1 MiB gap it is +35.4%
  at 100,000 rows and +41.9% at 400,000 (p = 8, net of round 4);
- the break-even id length at 400,000 rows and p = 2 is about 17 bytes (209,529 / 11,411 − 1),
  about 15.5 at 1 MiB, where the spec said "about 8".

**Residue (code review, minor):** suffix reads are logged at `0..n`, which would fail loudly, not
wrongly, if a layout put codes in a segment's first 8 KiB; the 1 MiB rows are not pinned.
