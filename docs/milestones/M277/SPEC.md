# M277: what a write costs in PUTs, per tenant

## Serves
D-41 (cross-tenant cohort fan-in, never built), through lesson 1 of
[lessons-from-hsearch-opensearch.md](../../comparisons/lessons-from-hsearch-opensearch.md):
measure before choosing whether `wal_bundling = cohort` is worth building.

## Delta
1. **A measurement, pinned as exact counts**, in `crates/pstore-server/tests/it/puts_per_write.rs`,
   read off the per-tenant request counter through the handler:
   - sparse `durable` writes, 100 tenants: a first write, a later write, and one fold each;
   - sparse `batched` writes, 100 tenants, the same three steps;
   - 8 concurrent `durable` writes to one tenant (its in-flight cap), every request taking 20 ms;
   - 64 concurrent `durable` writes to 64 tenants, every request taking 20 ms.
2. **The decision**, from those numbers, in
   [lessons-decisions-2026-11-17.md](../../comparisons/lessons-decisions-2026-11-17.md).

Nothing on a request path changes. **No setting**: a test and a document.

## Acceptance criteria
1. A sparse durable write costs exactly 2 PUTs per tenant on its first write (bundle + lane
   registration), 1 per tenant thereafter, and a fold 2 per tenant (segment + HEAD CAS).
2. A sparse batched write costs 0 PUTs, and so does a fold tick, until the tenant's flush bound.
3. 8 concurrent durable writes to one tenant share bundles: at most 3 PUTs at 20 ms a request.
4. 64 concurrent durable writes to 64 tenants cost exactly 64 PUTs: one lane per tenant.
5. The decisions doc records the per-write and per-fold figures, what cohort bundling would
   save on which workloads, and what it would cost.

## Test plan
Each count is exact (or a bound for the latency-dependent share), so each red run is the
pinned number moved by one step and observed failing.

## RA budget
Unchanged. This milestone pins it.

## Risks
- **The memory store hides group commit**: with no latency nothing is in flight, so 8
  concurrent writes cost 8 PUTs. The share is measured on a store whose requests take 20 ms,
  which is closer to S3 and still `provisional`.

## Tasks
- **M277.1** The spec.
- **M277.2** The measurement and the decision.
