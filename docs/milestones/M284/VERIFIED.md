# M284 verification

The spec was not sent to a separate reviewer before code; the PR's review covers it.

1. PASS. The ladder in release (`./scripts/write-ladder.sh`; built here with thin LTO off,
   since the full `it` binary's LTO link ran out of memory in this container), 4 cores.
   ⚠️ `provisional`: in process, in-memory store at 20 ms a request, relative only.

   | shape | rate/s | acked | 429 | p50 ms | p99 ms | PUTs | GETs |
   |---|---|---|---|---|---|---|---|
   | one tenant | 25 | 100% | 0 | 23 | 26 | 75 | 0 |
   | one tenant | 50 | 100% | 0 | 113 | 202 | 142 | 0 |
   | one tenant | 100 | 65.7% | 103 | 739 | 1,188 | 143 | 0 |
   | 64 tenants | 250 to 2,000 | 100% | 0 | 22 to 23 | 23 to 24 | 1 a write | 0 |
   | 64 tenants | 4,000 | 58.1% | 5,026 | 25 | 57 | 6,799 | 0 |
   | 64 tenants, bulks of 10 | 250 to 2,000 | 100% | 0 | 22 to 23 | 23 to 24 | 1 a write | 0 |
   | 64 tenants, bulks of 10 | 4,000 | 58.2% | 5,016 | 25 | 51 | 6,839 | 0 |

   Knees: one tenant 50 writes/s (12 docs/s a core); 64 tenants 2,000 writes/s (500 docs/s
   a core); in bulks of 10, 2,000 writes/s (5,000 docs/s a core).
2. PASS. `a_low_rate_holds_on_every_shape`.
3. PASS. Lesson 8's section of `docs/comparisons/lessons-decisions-2026-11-17.md`:
   `grep -n "Lesson 8" docs/comparisons/lessons-decisions-2026-11-17.md`.
