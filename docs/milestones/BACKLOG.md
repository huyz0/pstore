# Backlog

⚠️ **The ledgers are the source, this is the index.** Every row below is carried forward from
a `VERIFIED.md`, and each links to the ledger that stated it. Nothing new is invented here; if
a row and its ledger disagree, the ledger wins.

⚠️ **This file is not a plan for the whole system.** It is what previous milestones said they
had not finished. Work not yet specified — M7's remaining three bullets, and anything past
them — is in [`roadmap.md`](../research/11-design/roadmap.md), not here.

## Ordering, and why

Three principles, applied in this order:

1. **A criterion someone already wrote outranks a criterion nobody has written.** M0a's
   criterion 4 says "412, 409, 503 **and latency**" and the ledger says `NOT-RUN` for latency.
   That is a milestone reporting itself incomplete, and closing it costs less than arguing
   about what to do next.
2. **A cost that rises with time outranks one that does not.** Block-max pruning is the only
   item here that gets *more* expensive every week: segments are immutable, so every segment
   written before the retrofit is permanently unprunable ([C-11](../research/06-indexing/full-text-search.md)).
3. **A gate that measures less than it claims outranks the code it fails to measure.** If
   `scripts/coverage.sh` cannot see `pstore-engine`, every coverage number quoted about the
   correctness core has been meaningless, including this session's.

## Phase 1 — close what a ledger already calls unfinished

| # | Task | From | Size |
|---|---|---|---|
| ~~1~~ | ~~**M0a.10** — latency injection in `Faults`~~ **DONE.** M0a criterion 4 is now met as written, and the second random stream is what makes "independently" true rather than merely intended. | [M0a](M0a/VERIFIED.md) | S |
| ~~2~~ | ~~**`Engine::gc` chunks its delete batch**~~ **DONE.** Chunked at the capability, not a constant; a cap of zero errors rather than panicking. | [M7a](M7a/VERIFIED.md) | S |
| ~~3~~ | ~~**The roster CAS**~~ **DONE — stays unguarded, and now says why in the code.** The roster only unions, so a CAS that fails to fence delays convergence rather than losing a member; pinned by a test against a store that ignores the precondition. | [M7a](M7a/VERIFIED.md) | S |
| ~~4~~ | ~~**M0a.11 + `pstore-blob` coverage**~~ **DONE.** Coverage 94.31% → **97.99%**, mutation 76.8% → **95.8%** (206/215 viable). The coverage gap turned out to be the class-forwarding methods, whose own comments say dropping the class silently disables D-21 — and nothing tested it. | [M0a](M0a/VERIFIED.md), [M7a](M7a/VERIFIED.md) | M |

## Phase 2 — the gate that cannot see the correctness core

| # | Task | From | Size |
|---|---|---|---|
| ~~5~~ | ~~**`scripts/coverage.sh` excludes `pstore-engine`**~~ **DONE.** A crate declares `[package.metadata.pstore] ships = false` instead of the gate inferring it, so the default is *measured* and opting out shows up in a diff. The gate still passes with the correctness core in scope: **95.41%**. | [M7a](M7a/VERIFIED.md) | M |

## Phase 3 — the format decision that gets dearer every week

| # | Task | From | Size |
|---|---|---|---|
| 6 | **[M5d](M5d/SPEC.md) — the block-max *format*.** Specced, built, **reverted, and amended**. ⚠️ Measured at gate scale, the block table grows the dictionary sidecar from 28,030 to 616,994 bytes — **22×** — and that object is fetched `Pinned` and whole on every text query. A dictionary scales with terms; a block table scales with postings. The existing test `the_term_dictionary_scales_with_terms_not_documents` caught it and is right. Needs a place for per-posting metadata that is not fetched whole per query, before anything else. | [M5c](M5c/VERIFIED.md), C-11 | M |
| 6b | **M5e — the pruning**, unwritten. ⚠️ Two spec-review rounds found a blocking defect in each draft of the pruning half: an upper bound alone cannot prune before a fetch (θ = 0), and a lower bound fixes that for one term but is **unsound for disjunctive multi-term queries**, where a block bounds one term's addend and not a document's score. Corrected, the gain mostly evaporates — the witness term's own maximum exceeds θ, so no other term's blocks are prunable. M5d's spec carries the five questions M5e must answer, including whether the answer is impact-ordered postings instead. | [M5d](M5d/SPEC.md) | L |

## Phase 4 — the handoffs named by three milestones each

| # | Task | From | Size |
|---|---|---|---|
| 7 | **A stable cross-segment identity.** Blocks cross-segment fusion and the multi-segment BM25 caller that two-pass IDF was built for. `pstore_query::query` takes one key and `Hit.row` is within-segment. | [M3b](M3b/VERIFIED.md), [M5a](M5a/VERIFIED.md), [M5b](M5b/VERIFIED.md), [M5c](M5c/VERIFIED.md) | L |
| 8 | **A schema in HEAD.** `DEFAULT_FIELD` and `DEFAULT_TEXT_FIELD` are placeholders; M5c said the replacement was "M6's catalog", and M6a's `TenantRecord` carries index names only. | [M5c](M5c/VERIFIED.md), [M6a](M6a/VERIFIED.md) | M |

## Phase 5 — the milestone that was split off and never written

| # | Task | From | Size |
|---|---|---|---|
| ~~9~~ | **[M6b](M6b/SPEC.md) — quotas and metering. DONE** ([VERIFIED](M6b/VERIFIED.md)): `pstore-meter`, 99.26% regions, **73 of 73 viable mutants caught**. Specced over two review rounds. ⚠️ Three blocking findings in round one, all about shape rather than absence: a reservation counting ranges where `Accounted` counts *coalesced fetches*; a byte quota that cannot be reserved before a transfer whose size is unknowable without a forbidden `head`; and `Usage` in `TenantRecord`, which is unsound in both directions and is now out of scope. The billing rollup and the 1M workload are named as not-this. | [M6a](M6a/VERIFIED.md) | L |
| 10 | **M6a leftovers** — bucket splitting (OQ-8) and run reaping. Both need a protocol of their own: splitting must keep an old-width reader stale rather than wrong; reaping needs a retention window or a reader mid-enumeration loses the run it is on. | [M6a](M6a/VERIFIED.md) | M |

## Blocked, and by what

⚠️ **Named rather than omitted, and none of these is blocked on effort.**

| Task | Blocked on |
|---|---|
| **M0b** — real-cloud capability profiles, latency, CAS contention, cost | Cloud accounts. Every emulator profile says so in its own header (D-99). |
| **M3's exit** — 90–95% recall@10 on **100M vectors** (measured at 20,000 × 384d) | ~300 GB and a machine that is not WSL2; brute-force ground truth alone exceeds the gate budget. |
| **M5's exit** — MS MARCO evaluation | No network, no dataset. |
| **M1.13 / M4d** — the NVMe tier and `foyer` | A real device. ⚠️ D-23 calls persistence *mandatory*: until it lands a rolling restart flushes every cache, ~10 h to refill per node. The tier could be *built* against a temp directory; the numbers that justify it cannot. |
| **GCS bulk delete** | `object_store` 0.14.1 maps GCS's `delete_stream` to one request per location; the JSON API it uses has no bulk delete. Upstream, not ours. |
| **OQ-153** — `fake-gcs-server`'s `ifGenerationMatch` fidelity | The emulator routes the XML upload path to its JSON handler. Needs a newer emulator, a different client, or the JSON upload path. |
| **M4e / OQ-59** — the balance cost of AZ-aware LRH | Not effort: one sample per configuration measures ring-position luck. Needs an experiment design, which is a task, not a measurement. |
