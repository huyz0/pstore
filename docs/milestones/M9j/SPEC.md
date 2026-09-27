# M9j — A restarted writer resumes its lane

**Serves:** [BACKLOG](../BACKLOG.md) row 39, and OQ-91's rule that a lane is **dense**: a
reader takes the first missing sequence as the lane's end. `docs/deploy.md` tells operators
to keep `PSTORE_LANE` stable across restarts, and today that is the unsafe choice.

## What is true today

- `Engine::new` starts the lane at sequence 0, and nothing reloads it.
- A process restarted on its lane therefore PUTs bundles at sequences it already wrote:
  - **below HEAD's watermark**: overwriting bundles already folded, which no fold reads again;
  - **at or above it**: overwriting bundles not yet folded, which destroys acknowledged rows.
- Its own view loses the rows too: `prune` retains batches at or above the watermark, so a
  batch at a lower sequence is dropped from memory on the next query.

## Delta

**Resume once, before the first flush.** An engine's first flush (the first with rows to
write) resumes the lane before choosing a sequence:
1. It reads HEAD **afresh**.
   - The read is the resume's own: neither the schema cache nor a HEAD read on another path
     stands in for it (spec review, M1). A stale watermark whose folded bundles `gc` reaped
     would make the tail stop early.
   - The schema check uses the schemas this read returns, so it needs no second read. A
     refused flush stops here: it neither probes nor registers, as today.
2. It sets the sequence to `lanes::tail(store, tenant, lane, from = w)`, where `w` is that
   HEAD's watermark for this lane, or 0 when HEAD has none. The tail is the **resume point
   `r`**. This probes with HEAD requests and never LISTs.
   - (Implementation) The lane registers (`lanes::register`) alongside the probes, as the
     zero-sequence flush did before. It is one GET when the lane is already recorded.

Later flushes use the in-memory sequence, as today.

**`strong` after a resume (spec review, B1).** The previous incarnation's bundles in
`[w, r)` are neither folded nor in this memtable. So `settled` probes its own lane:
- at `w`, while HEAD's `w < r` or the lane is not yet resumed;
- at `max(w, next)` otherwise, as M9i.2 does today.

Failure handling:
- If the HEAD read, a probe or the registration fails, the flush fails.
- In that case nothing is written and nothing is resumed: the rows stay pending, and the next
  flush retries the whole resume.
- A lane longer than the probe bound fails the flush with `LaneTooLong`, as a fold would.

Bundles the previous incarnation left unfolded stay in the store. The next fold of the
tenant folds them, since folds cover every registered lane. They are not loaded into the new
memtable: another process's unfolded writes are not loaded either (M9i.1).

**Corrections:**
- `docs/deploy.md`: a restart on the same lane is now safe, and the lane stays single-writer.
- M9i's spec and VERIFIED name the restart defect; a banner there points here.
- The BACKLOG row is closed.

**Does not change:** a flush after the first, which is still exactly 1 PUT; the fold; any
read path except `strong`'s own-lane probe above; the format.

## Acceptance criteria

1. **Restart after a fold.** Engine E1 on lane L:
   - writes and flushes `a`, folds, then writes and flushes `b` (unfolded).
   - Then E2, a new engine on L, writes and flushes `c`.
   - E2's query returns `c` before any fold.
   - E2's `strong` query is refused with `NotFolded` before that fold, because `b` is unfolded.
     After it, the same query is served.
   - After a fold, a scan returns `a`, `b` and `c`, each exactly once.
   1b. (Mutation sweep) The resume's HEAD read is its own. After bundle 0 is folded and then
   deleted (as `gc` reaps it), E2 runs a query, which fills the schema cache, and then
   flushes. The flush lands at `Seq(2)`.
2. **Restart with nothing folded.** E1 flushes `a`, then `b`. E2 on L flushes `c`, and its
   flush returns `Seq(2)`. A fold then returns all three rows, each once.
3. **Cost.** For E2 in criterion 2:
   - the first flush issues 0 LISTs, 1 PUT, and exactly 10 reads: HEAD, the registry, and one
     window of 8 probes;
   - the second flush issues 1 PUT and 0 reads.
4. **A failed resume consumes nothing.** On a store refusing the first probe, E2's first flush
   errors and its rows stay pending. The next flush succeeds at the tail, and a fold returns
   every row once.
5. **Through the server.** A second `Api` on the same lane (a restart) takes a durable PUT.
   After a fold, a query returns the rows written before and after the restart.
6. `./scripts/gates.sh` passes, and `./scripts/mutants.sh` over the diff misses 0.

## Test plan

| # | Fails first | Mutation it catches |
|---|---|---|
| 1 | `c` overwrites bundle 0 and is pruned, then never folded | resume from 0 instead of the watermark; a resume skipped; `settled` probing at `next` after a resume |
| 1b | lands at `Seq(0)` with the read skipped | the schema cache standing in for the resume's read |
| 2 | the flush returns `Seq(0)` and `a` is lost | the tail not probed |
| 3 | 2 reads today, not 10 | a resume on every flush; a registration on every flush |
| 4 | new test; red with the resume marked done before its reads succeed | the resume latched on failure |
| 5 | the pre-restart row is lost | — (wiring) |

## RA budget

- An engine's first flush adds one window of probes: 8 HEAD requests in parallel, for a lane
  with fewer than 8 unfolded bundles. So it issues 10 reads, not 2.
  - (Implementation) Two existing budget tests pinned 2 reads and 4 requests. They now say 10
    and 12, and each says why: `the_whole_flow_stays_inside_its_request_budget` and
    `a_durable_write_costs_its_lane_registration_once_then_one_put`. The first also gains a
    depth bound.
- Depth is unchanged, because the probes run beside the registry's read:
  - a new lane: HEAD, then {probes, registry GET}, then the CAS, then the PUT (4);
  - a restart: HEAD, then {probes, registry GET}, then the PUT (3).
- Once per engine lifetime, not per write. No LIST.

## Risks

- A HEAD read that is stale by a fold, whose bundles `gc` then reaps before the probe, makes
  the tail stop at the old watermark. This is the same precondition `strong` states: it holds
  while fewer than `retention` epochs commit between the read and the probe.
- Two live processes on one lane still collide. That is the operator's single-writer
  contract, and this change does not detect it.

## Tasks

- **M9j.1** — resume on the first flush, the docs corrections, and BACKLOG row 39 closed;
  criteria 1–6.
