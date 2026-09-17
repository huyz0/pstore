# Service objectives, and which of them are enforced

**Synthesizes:** D-34 (the round-trip budget), D-104 (WSL2 and emulator numbers are relative
only), Design rule 13. **Status:** M7f.

⚠️ **Every row is `enforced` or `blocked`. There is no third status**, because the third status
is how *"we aim for p99 under 100 ms"* gets written by someone who has measured nothing. An
`enforced` row names the gate that fails when it is violated; a `blocked` row names what is
missing. `scripts/check-slos.py` is the gate on this file itself.

⚠️ **No objective here is stated in seconds.** Every latency number this repository could
produce today comes from WSL2 against an in-memory store, which D-104 says is relative only.
Latency objectives are therefore `blocked` on M0b, each with the measurement it needs.

## Request-count and depth objectives — enforced

| Objective | Value | Status | Gate |
|---|---|---|---|
| A write batch costs one write-class request, whatever it carries | 1 PUT | enforced | `the_whole_flow_stays_inside_its_request_budget` |
| A lane's first flush additionally registers the lane and reads the schemas | 2 W + 2 R, once per process | enforced | `a_durable_write_costs_its_lane_registration_once_then_one_put` |
| A batched write costs nothing | 0 requests | enforced | `a_batched_write_costs_nothing` |
| A cold vector query's sequential depth | ≤ 3 round trips | enforced | `a_cold_query_from_head_costs_three_round_trips` |
| A query through the API, including id resolution | **4**, flat in the segment count | enforced | `a_query_costs_four_round_trips_however_many_segments_it_has` |
| A query answered from the memtable reads HEAD once and nothing else | 1 R | enforced | `a_memtable_query_reads_head_and_nothing_else` |
| Time travel costs no more than a live query | 1 HEAD read, 0 LIST | enforced | `time_travel_costs_one_head_read_and_no_lists` |
| Enumerating a tenant's indexes | 1 R, 0 LIST | enforced | `no_endpoint_lists` |
| Catalog enumeration is independent of tenant count | `2 × width + 1` | enforced | `scripts/scale.sh` |
| **Zero LISTs on every serving path** | 0 | enforced | `no_endpoint_lists` |
| Recall@10 on the gate corpus | ≥ 0.90 | enforced | `scripts/recall.sh` |
| NDCG@10 against a control ranker | above floor | enforced | `scripts/ndcg.sh` |
| Round-trip depth and query bytes at gate scale | ≤ 3, bounded | enforced | `scripts/depth.sh` |

## Correctness objectives — enforced

| Objective | Value | Status | Gate |
|---|---|---|---|
| No answer is wrong under injected faults; every failure carries a code | 0 wrong answers over 10 seeds | enforced | `under_injected_faults_every_answer_is_correct_or_a_refusal` |
| A document acknowledged durable and folded is never missing from a successful query | 0 missing | enforced | `an_acknowledged_document_is_never_missing_under_faults` |
| The epoch sequence is linearizable under adversarial scheduling | 100+ writers | enforced | `every_acknowledged_document_survives_contention` |
| A backend that cannot fence serves nothing | refused at the door | enforced | `a_backend_that_cannot_fence_is_refused_before_the_socket_is_bound` |

## Latency objectives — blocked

⚠️ Each names the measurement that unblocks it. None is stated as a number, because a number
here would be a WSL2 number wearing a production label.

| Objective | Status | Blocked on |
|---|---|---|
| p50 / p99 / p999 cold query latency | blocked | M0b: TTFB percentiles per backend, per object size, from inside the cloud (OQ-3) |
| p50 / p99 write acknowledgement latency, `durable` | blocked | M0b: PUT latency distribution against a real backend |
| Sustained CAS rate per tenant before contention collapses | blocked | M0b: OQ-5 contention curves; M0c produced the *shape* on a local store, which is relative only |
| Time to first query after a cold start | blocked | M1.13: the NVMe cache tier, which D-23 calls mandatory and which needs a real device |
| Recall at 100M vectors | blocked | M3's exit: ~300 GB and a machine that is not WSL2 |

## Availability objectives — blocked

| Objective | Status | Blocked on |
|---|---|---|
| Error budget per tenant per month | blocked | A fleet-wide rollup of `Meter::usage`, which does not exist |
| Time to detect a dead node | blocked | M4b measured detection in **gossip periods**, which is a count and not a duration until a real fleet sets the period |
