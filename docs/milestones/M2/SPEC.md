# M2 — Multi-writer correctness

**Serves:** Invariant I1, D-32 (the simulator is built before the distributed features, not
after), D-101, and the commit protocol's fencing property. **Settles
[OQ-91](../../research/00-plan/open-questions.md)** — the Tier-1 risk that, if it fails,
makes cross-index bundling unsafe and collapses the cost model. Closes M1.14.

**Exit condition** (roadmap): *linearizability of the epoch sequence under adversarial
scheduling, at 100+ logical writers, plus the bundle-recovery proof.*

## Delta

**Adds**
- `pstore-sim` — deterministic simulation: seeded schedules, injected pauses, partitions
  and CAS storms, and an auditing store that can *prove* Invariant I1 rather than assert it
  in prose.
- Lane tail discovery — the lane bitmap and forward probing, replacing M1's injected
  watermark, so a successor finds a predecessor's bundles from the blob store alone.
- Compaction as optimistic work plus CAS-on-publish, and GC bounded by epoch retention.

**Does not add**
- Real network partitions or process death. The simulator models them; it does not
  reproduce a kernel. That distinction is stated wherever a result depends on it.
- Placement, gossip, or anything multi-node above the store. M4.
- ANN, quantization, caching. M3, M4.

## Acceptance criteria

1. A simulation run is **reproducible from its seed**: the same seed yields an identical
   sequence of operations and the same verdict, and a different seed diverges.
2. **Invariant I1 holds**: no object is ever mutated except through a compare-and-swap on
   the exact version the writer observed. Asserted by an auditing store that fails the run
   when any key is overwritten by an unconditional write.
3. With **≥100 logical writers** committing concurrently, the epoch sequence is dense and
   strictly increasing — no gaps, no repeats — and every acknowledged document is present
   exactly once.
4. The same holds under **injected pauses, CAS contention storms, and lost writers**: a
   writer paused past many commits cannot corrupt anything when it wakes.
5. **Lane tail discovery works from the blob store alone**: a successor finds a
   predecessor's flushed bundles using the lane bitmap and forward probing, with **no
   injected watermark and no LIST**.
6. **OQ-91.** Under writer death, a successor taking over, and fallback writes to a
   different lane, recovery finds **every** un-folded record — no loss, no duplication —
   across many seeds.
7. Compaction is **optimistic**: several nodes may compact the same input concurrently,
   exactly one commit lands, the losers discard, and the visible answer is unchanged.
8. Compaction leaves **no orphan referenced by HEAD**, and a losing compactor's output is
   unreferenced and therefore reapable.
9. **GC respects epoch retention**: an object dereferenced by a newer epoch is only reaped
   after the window, and a reader holding an older epoch is never broken mid-scan.
10. Region coverage ≥95% on shipped crates, mutation ≥80%, and the full gate set green.

## Test plan

| # | Test that must fail first | Mutation it catches |
|---|---|---|
| 1 | `a_seed_reproduces_the_same_schedule` | using wall-clock or thread order, making failures unreplayable |
| 1 | `different_seeds_diverge` | ignoring the seed entirely |
| 2 | `the_auditing_store_rejects_an_unconditional_overwrite` | an auditor that never fails |
| 2 | `the_engine_never_mutates_an_object_it_did_not_cas` | any in-place mutation added later |
| 3 | `one_hundred_writers_produce_a_dense_epoch_sequence` | a lost update, or an epoch gap |
| 3 | `every_acknowledged_document_survives_contention` | rows dropped under a rebase |
| 4 | `a_writer_paused_past_many_commits_cannot_corrupt` | accepting a stale tag |
| 4 | `a_cas_storm_still_converges` | livelock, or an unbounded retry |
| 5 | `a_lane_bitmap_records_a_lane_in_one_cas` | one CAS per write rather than per lane |
| 5 | `a_successor_finds_the_tail_by_probing_not_listing` | a LIST creeping onto the recovery path |
| 6 | `recovery_finds_every_unfolded_record_across_seeds` | **the milestone's reason for existing** |
| 6 | `recovery_after_a_fallback_write_to_another_lane` | a successor that only probes its own lane |
| 7 | `concurrent_compactors_produce_one_winner` | two commits both landing |
| 7 | `compaction_does_not_change_the_answer` | rows lost or duplicated by a merge |
| 8 | `a_losing_compactors_output_is_unreferenced` | a HEAD pointing at a discarded segment |
| 9 | `a_dereferenced_object_survives_the_retention_window` | reaping while a reader holds it |
| 9 | `gc_never_reaps_a_referenced_object` | reaping something HEAD still names |

## RA budget

| Operation | Budget |
|---|---|
| Lane registration | **1 CAS per lane lifetime**, not per write |
| Tail discovery | `k` parallel probes, **1 Rseq**, and **0 LIST** |
| Compaction of *n* inputs | *n* Rpar + 1 W + 1 commit |
| GC | derived from manifest diffs, **0 LIST** |

## Risks

- **The simulator may model a world kinder than the real one.** Mitigated by making the
  auditing store *refuse* rather than report, and by driving faults from the seed rather
  than from timing — but a simulator is not a kernel, and every result here says so.
- **OQ-91 may fail.** That is the point of running it. If recovery cannot be shown to find
  every record, cross-index bundling is unsafe and the finding matters more than the
  milestone.
- **Duplicate-work suppression is not yet testable.** It depends on placement, which is M4.
  This milestone shows only that duplicate work is *safe*, not that it is rare.

## Tasks

| ID | Task |
|---|---|
| M2.1 | `pstore-sim`: seeded schedules, deterministic replay |
| M2.2 | Auditing store: proves Invariant I1 by refusing any non-CAS overwrite |
| M2.3 | Lane bitmap + forward probing; remove M1's injected watermark |
| M2.4 | Recovery: a successor folds a predecessor's bundles from the store alone |
| M2.5 | **OQ-91 proof** across seeds, with death, succession and fallback lanes |
| M2.6 | Linearizability at ≥100 logical writers under injected faults |
| M2.7 | Compaction: optimistic, CAS-on-publish, losers discard |
| M2.8 | GC with epoch retention |
