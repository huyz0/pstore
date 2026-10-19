# M109 — a tenant's error budget, rolled up across the fleet

**Serves:** [`slos.md`](../../research/11-design/slos.md)'s availability row *"Error budget per
tenant per month — blocked on a fleet-wide rollup of `Meter::usage`, which does not exist"*,
and `pstore-meter`'s own gap: *"summing across the fleet is the billing rollup's problem and it
is not built."* No backlog row.

## What is true today

These facts come from reading `pstore-meter` at M108.

- `Meter::usage(tenant)` is one node's requests and bytes per `OpClass` since the process
  started. It lives in memory, so a restart loses it and a month boundary does not reset it.
- Nothing counts **answers**. The meter sees blob requests, which are what a tenant is billed
  for. An error budget is about the API requests a tenant made and how many failed. A blob
  request that failed and was retried is not a tenant-visible failure.
- Nothing calls `Meter::new` outside the crate's tests. Like `pstore-catalog`, the crate is
  built and not wired.

## The design

**Each node writes one report per month, and the rollup reads them all.** A report is one
object: the node's cumulative counts for the month, for every tenant it served. The rollup
sums the reports per tenant. Requests scale with nodes, never with tenants: a report costs one
PUT, and a rollup costs one GET per node that served in the month.

⚠️ **The reports' keys have to come from somewhere, and the roster is the wrong place.** The
roster lists the nodes that are members now. A node that served half the month and then died
is no longer in it, so a rollup keyed on it drops that node's counts. A node that dies is the
node most likely to have failed requests. So the month gets its own **index**: a node adds its
id the first time it reports in a month, and the rollup reads the index and then the reports.
That is two rounds, and nothing is LISTed.

⚠️ **The index is sharded, 64 ways by a hash of the node id**, because every node registers at
the start of every month. One object would put every node in the fleet on one CAS within the
same minute. With 64 shards at 10,000 nodes, about 156 nodes share each shard, once a month.
The rollup reads all 64 shards in one round, whatever the fleet size.

## Delta

1. `Meter::answer(tenant, Answer::{Ok, Failed})` counts the API answers in `Usage`, with two new
   fields: `answered` (every answer) and `failed` (the ones the budget spends). Which status
   codes count as `Failed` is the caller's decision; the doc says server-side failures, never a
   refusal the tenant caused, such as a bad request or a quota refusal.
2. `pstore_meter::Month` is a calendar month in UTC, derived from a `Duration` since the UNIX
   epoch.
3. `pstore_meter::Reporter`, one per node process:
   - `Reporter::resume(store, node, month, &meter)` reads the node's report for the month
     (one GET; absent means it has not reported this month) and keeps it as the base. A
     restarted node continues its month rather than starting it over.
   - `Reporter::publish(store, &meter, month)` writes the report, which is the base plus what
     the meter counted since the base was taken. When the month has changed since the last
     publish, the base is reset to empty, and what was counted since the old month's last
     publish goes to the new month. It registers the node in the month's index shard the
     first time it publishes for that month.
   - The report is written conditional on the tag this reporter last saw. A lost CAS means
     another process holds the same node id, and that is returned as an error, never
     overwritten. A write that answers `Io` may have landed. The next publish reads the
     report once and adopts it only if it holds that write's bytes (review round 1). ⚠️ Not guarded by the fencing profile, as the roster's CAS is not: this is
     telemetry, and a backend that ignores the precondition costs a miscount, never tenant
     data.
4. `pstore_meter::rollup(store, month) -> Rollup`: it reads the 64 index shards, then every
   report they name, and sums them per tenant.
5. `Rollup::budget(tenant, target_ppm) -> Budget { answered, failed, allowed, remaining, exhausted }`.
   The target is the answers in a million that must succeed, in integers: a float floored
   four nines over 10,000 answers to zero (review round 1).
   `allowed` is `floor((1_000_000 − target_ppm) × answered / 1_000_000)`, `remaining` is `allowed − failed`, signed, and
   `exhausted` is `failed > allowed`.

**Not changed:** who calls them. The server constructs no `Meter`, and wiring the meter into the
serving path is its own milestone. The budget's **target** stays unset too: what a tenant should
be promised is a number that needs a real fleet's failure rate.

## Acceptance criteria

1. **The fleet's counts are the sum of the nodes'.** Three nodes serve tenants A and B with
   known answers and failures, and publish. `rollup` returns each tenant's sum, across all
   three.
2. **A node that left is still counted.** A node publishes once and is never heard from
   again, and no list of members is passed to the rollup. The rollup includes it. It is seen
   red against a publish that does not register in the index.
3. **A restart continues the month.** A node publishes, its process is replaced by a fresh
   `Meter` and a `Reporter::resume`, it serves more, and it publishes again. The rollup holds
   both halves. It is seen red against a reporter that does not resume (the first half lost).
4. **A new month starts at zero.** A publish in the next month writes only what was counted
   since the old month's last publish. The previous month's rollup is unchanged.
5. **Two processes with one node id are refused.** The second one's publish fails, and the
   first one's report is not overwritten.
6. **What it costs.** On an accounted store, with `n` nodes:
   - a publish costs 1 write after the first of the month;
   - the first publish of a month costs 1 write plus the shard's read and CAS;
   - a rollup costs exactly `64 + n` reads, in two rounds;
   - 0 LIST everywhere.
7. **The budget arithmetic.** At a target of 999,000 ppm with 10,000 answers, `allowed` is 10.
   With 10 failures, `remaining` is 0 and the budget is not exhausted. With 11 failures,
   `remaining` is −1 and it is exhausted. At 999,900 ppm, 10,000 answers allow exactly 1.
8. **Month arithmetic.** `Month::of` maps the epoch to 1970-01, 2024-02-29T23:59:59 to 2024-02,
   and 2024-03-01T00:00:00 to 2024-03. A key carries `yyyy-mm`.
9. **`slos.md`.** The row is split. The rollup is `enforced`, naming criterion 6's test. The
   target stays `blocked`, on a real fleet's failure rate. `scripts/check-slos.py` passes.
10. Gates: `scripts/gates.sh`, and `scripts/mutants.sh` over the diff.

## RA budget

A publish is 1 PUT, plus a GET and a CAS on the month's first publish. A rollup is `64 + n`
GETs in two rounds. Neither is on a request path, and neither scales with tenants.

## Risks

- **A crash between publishes loses what was counted since the last one.** That is bounded by
  the publish cadence, which is the caller's choice, and it is an undercount of answers *and*
  of failures alike.
- **A report holds every tenant the node served.** It is bytes, not requests, and a rollup's
  bytes scale with nodes × tenants per node.
- **Registration contends once a month.** About 156 nodes per shard at 10,000 nodes. A node
  whose registration stays contended returns the error and retries on its next publish. Its
  report is already written, but the index does not name it until a registration lands.

## Tasks

- **M109.1**: `Answer`, `Month`, `Reporter`, `rollup`, `Budget`, the tests, the `slos.md` rows,
  the ledger.
