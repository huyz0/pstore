# M39 — A flush re-reads its lane's watermark within half the reap age

**Serves:** [BACKLOG](../BACKLOG.md) row 24, which [M17](../M17/VERIFIED.md) narrowed.

## What is true today

- **Bundles are created, never replaced** (M17). A second writer on a lane is refused with
  `LaneTaken` and erases nothing, and a refusal consumes no sequence.
- **The residue.** Engine A writes on lane L and goes idle at next sequence N. Another writer
  B, misconfigured onto the same lane, writes bundle N, a fold passes it, and a reap deletes it.
  A's next flush then creates N again: the key is free, the create succeeds, and the write is
  **acknowledged durable**. But it sits below L's watermark, so no fold ever reads it, and GC
  later reaps it.
- A refuses only at its next **fresh** HEAD read. A write-only engine makes one only on its
  first flush (the resume, M9j). So a long-lived writer never makes another.
- **The scheduled reap is time-based** (M18). `reap_due(age)` takes only commits at least `age`
  old on the committing engine's monotonic clock, measured after its CAS returned. So a bundle
  is reaped no sooner than `age` after the fold that dereferenced it. The server's default age
  is an hour (`PSTORE_GC_AGE_S`).

## Delta

**A flush re-reads HEAD when its last fresh read of its lane is older than half the reap age.**

1. `Engine::with_lane_recheck(within)` sets the bound. Without it nothing changes.
2. Every fresh HEAD read a flush makes (the resume's, and this one) records when it was made.
   The instant is taken **before** the GET is issued, as the head cache's is, so a slow read
   straddling a fold counts as early. It is never the `bounded` cache's HEAD.
   - ⚠️ **First in the flush, under its lock, before anything else** (spec review, major 1):
     a flush whose last such read is older than `within` reads HEAD afresh before it resolves
     an uncertain write.
   - Placed just before the `PUT`, it raised the watermark after M17's resolution had already
     loaded it. Take a write that answered `Io`, was found absent, then landed late and was
     folded: it was refused as `LaneTaken`, sticky, though the bundle was the engine's own.
   - First, the raised watermark sends that record through `resolve_uncertain`, whose GET
     says whose it is. A watermark past the next sequence then refuses with `LaneTaken`,
     sticky as today.
3. **The server sets `within` to half of `PSTORE_GC_AGE_S`** (default an hour, so 30 minutes)
   for every engine it builds, **whether or not this process reaps** (spec review, major 2).
   Another server reaping the tenant reaps this lane's bundles too.

**Why half the age suffices, with no clock agreement between servers.** Let A's last fresh read
be at time r, and its flush at f, with f − r < age/2.
- If B's fold passed N before r, that read showed the watermark past N, and the flush refuses
  without a request.
- Otherwise the fold came after r. Bundle N is then reaped no sooner than fold + age > r + age >
  f, so it still exists at the flush, and the `PUT` is refused as `Lost`, which is `LaneTaken`.
- The half is margin for the two servers' monotonic clocks running at different rates, and for
  the commit that the fold's age is measured from.

**Not changed:** the manual `/v1/admin/gc?retention=` reap. It counts epochs, not time, so it
carries no time guarantee: an operator who reaps by hand while a lane is misconfigured is
outside this one. The guarantee also assumes the fleet shares one `PSTORE_GC_AGE_S`.
Also unchanged:
- other fresh HEAD reads (a fold's, a strong query's) do not reset the window. They could, but
  they need not (spec review, nit);
- `pstore-node`, which does not depend on `pstore-engine`.

## Acceptance criteria

1. **An idle writer is refused, not lost.** On one store, engines A and B share lane L, and A
   rechecks within 10 seconds (paused tokio time).
   - A writes and flushes bundle 0, then idles for 11 seconds.
   - B resumes onto L, writes bundle 1, folds, and reaps everything.
   - A's flush is refused with `LaneTaken` at sequence 1, and bundle 1 does not exist.
   - **Parent:** the flush succeeds at sequence 1, below the watermark, and the row is never
     folded.
1b. **A late-landed, folded write of its own is not taken** (spec review, major 1). On M17's
   unreliable store, A's `PUT` at sequence 0 answers `Io` and is found absent. It then lands,
   another engine on the tenant folds it, and 11 s pass. A's next flush succeeds with sequence
   0 resolved as its own: no `LaneTaken`, and the row folded once.
   - **Mutation:** the recheck placed after the resolution's load. (The parent passes: it has
     no recheck to misplace.)
2. **The recheck is once per window.** A writer that rechecks within 10 seconds flushes at 0 s
   (the resume's read), 5 s, 10 s, 11 s and 21 s. Those flushes cost, in reads, ≥ 1, 0, 1, 0
   and 1 (counted on `Accounted`). The boundary rechecks: due at `>=` the window.
   - **Without a bound:** 0 reads after the resume, as today. Pinned so that the cost of the
     default stays unchanged.
3. **The server wires it.**
   - The configuration's recheck is half of `PSTORE_GC_AGE_S`, with `PSTORE_GC` on or off:
     60 s gives 30 s, and unset gives 30 minutes.
   - An `Api` given 30 s builds engines whose flush at 29 s after their last read issues no
     HEAD read, and whose flush at 31 s issues one, through `hold_engine_for_test` under
     paused time.
4. **Gates.** `./scripts/gates.sh` is green, and the sweep over M39's source diff misses 0.

## Test plan

| # | Test | Red on the parent because / mutation it catches |
|---|---|---|
| 1 | `an_idle_writer_is_refused_not_lost` (`pstore-engine/tests/lane_taken.rs`) | the recheck dropped (⚠️ on the parent the test does not compile, since `with_lane_recheck` is new, and a compile failure is not red: its red is this mutation) |
| 1b | `a_late_landed_write_is_not_taken_by_the_recheck` (same file) | the recheck placed after the resolution's load |
| 2 | `the_lane_recheck_is_once_per_window` (same file) | a read on every flush; the read's time not recorded; `>=` as `>` at the window |
| 3 | `the_server_rechecks_within_half_the_reap_age` (`pstore-server`) | no recheck wired; the whole age instead of half; the age read only with the reap on |

## RA budget

| Op | W | Rseq | Rpar | List | Depth |
|---|---|---|---|---|---|
| A flush, its last fresh read younger than `within` | 1 (unchanged) | 0 | 0 | 0 | unchanged |
| A flush, its last fresh read older | 1 | +1 (HEAD) | 0 | 0 | +1 |

At most one GET per engine per `within`, and only on a flush: an idle engine issues nothing. With
the default reap age of an hour, that is one GET per half hour per writing engine.

## Risks

- **A flush after a long idle waits one round trip longer**, once per window.
- **Monotonic clocks** pause during a host or VM suspend (WSL2 among them), so an engine that
  slept under-counts its idle time. The factor of two covers rate drift, not a suspended host.
  `tokio::time::Instant` is what the reap's age uses too.
  - **Revealer: none.** A write lost this way is lost silently; this hole stays open, stated.
- **A fleet with mixed `PSTORE_GC_AGE_S`** breaks the assumption. **Revealer: none.** The
  startup line does not print the reap policy (spec review, round 2), and adding it is not this
  milestone's.
- **The manual reap is outside the guarantee**, as above. Revealer: the next fresh HEAD read
  refuses the lane with `LaneTaken`, so the loss is loud afterwards, never prevented.
- **The RA budget is the steady state's.** The M17 path adds its resolution's GET as today.

## Tasks

- **M39.1** — The recheck, the server's wiring, tests 1–3.
- **M39.2** — The ledger, `BACKLOG.md` row 24 closed, and the roadmap row.
