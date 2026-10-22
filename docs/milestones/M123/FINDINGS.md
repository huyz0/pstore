# M123 — startup, shutdown, scaling and scalability: every finding

Four read-only reviews, each traced from the binaries' entry points: **startup** (process
start to first query and write), **shutdown and drain**, **scaling and rebalancing** (placement,
membership, routing), and **one node's scalability** (state and work that grow with tenants).
Each finding was checked in the code before it was acted on. "Task" names the commit that
closes it; [SPEC.md](SPEC.md) says what each delta does.

| # | Review | Finding | Outcome |
|---|---|---|---|
| 1 | shutdown, startup | **A `batched` write is never flushed** -- no timer, no stop. A deploy loses every batched row since the tenant's last durable write; a batched-only tenant's memtable grows without bound | **Fixed, M123.1**: the flush loop (`PSTORE_FLUSH_AGE_MS`, `PSTORE_FLUSH_BYTES`) and a stop's settle. `a_batched_write_is_flushed_within_the_flush_age`, `a_stop_flushes_and_folds_what_it_holds` |
| 2 | startup | A graceful stop leaves its durable bundles unfolded and undue: due-ness lived in the stopped process | **Fixed, M123.1**: the stop folds them; a resume past HEAD's watermark makes the tenant due at once. `a_stop_flushes_and_folds_what_it_holds`, `a_successor_folds_its_predecessors_bundles` |
| 3 | shutdown | The HTTP drain has no deadline | **Fixed, M123.1**: `PSTORE_DRAIN_MS`. `a_stop_returns_by_its_deadline_and_still_flushes` |
| 4 | shutdown | A second signal cannot hurry a stuck stop | **Fixed, M123.1**: `stop_then_exit`. `a_second_signal_exits_at_once` |
| 5 | shutdown | The replication worker finishes a whole tick of copies after the stop, and keeps its claims until their TTL | **Fixed, M123.1**: a halt flag, and claims released. `a_halted_tick_starts_no_copy`, `a_served_process_runs_its_worker` |
| 6 | shutdown, scaling | No leave message: a stopping node answers gossip through its drain and ~40 s after; no readiness route | **Fixed, M123.2**: a stop leaves first -- its record, dead, to `max(8, 3·log2 N)` peers -- and `GET /v1/ready` answers 503. `a_leaving_node_is_dead_everywhere_within_a_few_rounds_and_may_return`, `a_member_that_leaves_is_gone_from_its_peers_view_at_once`, `a_server_that_leaves_is_dropped_within_a_few_periods`, `a_stopping_server_is_not_ready` |
| 7 | scalability | Per-tenant request counters are never dropped: ~350 B a tenant ever touched | **Fixed, M123.2**: an evicted tenant's go with it. `an_evicted_tenants_counters_go_with_it`; `eviction_costs_no_request` now counts the process's totals, since per-tenant counters of an evicted tenant read zero -- red with a request per evicted tenant |
| 8 | scalability | A fold leaves an idle engine's stale fresh segments resident until its next query | **Fixed, M123.2**. `a_fold_drops_the_fresh_segments_it_made_stale` |
| 9 | scalability | The eviction pass checks every engine's idleness under the registry's lock every request waits on | **Fixed, M123.2**: decided on a snapshot outside the lock, rechecked under it for the chosen only. No test can see a lock's hold time: checked by reading `evict_tick`; the registry suite passes unchanged |
| 10 | scaling | **A tenant's lane registry fails for good at 4,097 lanes**; every fold probes 8 bundles of every lane | M123.3 |
| 11 | scaling, scalability | A split query places each segment over every server: O(N) per segment, C-6 unused | M123.3 |
| 12 | scaling, startup | Gossip and the roster never forget a dead node; a booting node seeds them all alive | M123.4 |
| 13 | startup | The store-id read has no timeout or retry: a fleet restart's herd exits | M123.4 |
| 14 | startup | The three disk tiers open one after another before the bind | M123.4 |
| 15 | startup | `pstore-node`'s announce is a GET and a CAS with no timeout | M123.4 |
| 16 | startup | The replication worker claims its 64 shards one at a time at start | M123.4 |
| 17 | scaling | The peer list has no AZ cell -- the zone field carries the URL -- so shares cross AZs | **Owner decision**: a cell per peer needs the gossip record to carry both, a wire change; recorded, not built |
| 18 | scaling | Lanes grow with every server that writes a tenant, because a load balancer spreads writes | **Owner decision** for the cure -- routing a tenant's writes to an owner changes read-your-writes; the cliff and the probe cost are fixed in M123.3 |
