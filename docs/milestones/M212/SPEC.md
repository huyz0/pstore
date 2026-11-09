# M212: a query reserves its plan's candidates before it runs

## Serves
D-55 (a query reserves from a pool, sized from its plan) in
[memory management](../../research/09-rust-stack/memory-management.md), and workstream A and
F of [the delivery plan](../../comparisons/turbopuffer-next-plan.md): "Admission reserves for
total legs/windows, not only the outer top-k". D-34 unchanged.

## Delta
Today admission (M128) counts a query as one active request whatever it plans: a multi-query
of 16 hybrid sub-queries at `top_k` 10,000 is one permit, as a `top_k` 1 lookup is. M143
bounds each leg's `top_k`, not their sum.

After, every public query reserves **candidate rows** for its whole plan, after it is parsed
and planned (every refusal found) and before any blob request, and holds them until its
response exists. A query's reservation is the sum, over the request's queries and each
query's facets:
- a relevance ranking: each leg's `limit` (a `multi` FDE leg's `rerank` when larger), plus
  its window (`top_k + offset`, or the boost, `diversify` or `limit_per` window);
- an order or a `rank_by` expression: `top_k + offset`;
- an aggregation: its `top_k`.

Two version-1 limits per `Api`, defaults and configuration as M128's (lower only, validated
before store, cache or bind), new fields of `PSTORE_REQUEST_LIMITS_CONFIG`:
- `query_rows`, default 4,194,304: what every query of this server holds at once;
- `tenant_query_rows`, default 1,048,576, at most `query_rows`: what one tenant's hold.

A reservation above `tenant_query_rows` can never fit: `413 request_too_large`, naming
`tenant_query_rows`. One that fits but finds the tenant's or the server's pool short gets
`429 overloaded` with `Retry-After: 1`, as M128's. Both never wait, and make no blob request.
The reservation is a drop guard, returned on success, refusal, failure and cancellation.
`Api::reserved_query_rows()` reports what is held (beside `ingress_counts`, whose shape is unchanged).

Not served: bytes. A candidate is a row a leg or window holds, whose bytes vary with its
attributes; D-55's byte estimate stays open, and this is the count it multiplies. A peer's
share of a split query (M45) is reserved by the public server that admitted it, not again by
the peer. Writes are unchanged (M141 bounds them).

## Acceptance criteria
1. Boundary: with `tenant_query_rows` R, a hybrid (dense and text) query whose reservation is
   R (two legs at window k plus the window, 3k = R) succeeds, and at `top_k` k + 1 gets 413
   `request_too_large` naming `tenant_query_rows`, with no blob request. A multi-query of
   four such sub-queries summing to R succeeds; one row more is 413.
2. Every term counts, each at exactly R and refused at R + 1: a dense query with a boost
   window W at `top_k` 1 (W + W); a ranked query with `offset`; an order (`top_k + offset`);
   an aggregation (`top_k`); a faceted query (its legs, window and the facets' `top_k`).
3. Contention: with H rows held for tenant T, T's query of R - H succeeds and one of
   R - H + 1 gets 429 `overloaded`, `Retry-After: 1`, no blob request; tenant U's query of R
   succeeds meanwhile; once the hold is dropped, the refused query succeeds. With
   `query_rows` R as well, R held by T refuses U's query of 1 row with 429.
4. Release: after a success, a 404 found while running (no such index), a 413 and a 429,
   `reserved_query_rows` is 0; while a query is held mid-run it equals its reservation.
5. Configuration: zero, above the default, and `tenant_query_rows` above `query_rows` are
   refused, by name, as M128's limits are; the fields read from the config JSON.
6. Every existing test passes unedited.

## Test plan
Server tests in a new `crates/pstore-server/tests/it/query_reservation.rs`; criterion 5 in
`ingress.rs`'s unit tests beside M128's. A test-only `Api::hold_query_rows_for_test` holds a
reservation for criterion 3, and a store that parks one read holds a query mid-run for 4.
Red before: 1 to 4 fail (nothing is reserved, so every boundary query succeeds); 5 fails to
compile. Mutations, each killed:
- legs not counted, only the window (1); the window not counted (2, the order);
- facets not counted (2); sub-queries after the first not counted (1);
- `>` for `>=` at the bound (1: exactly R succeeds);
- the guard released before the run rather than after (4: held mid-run);
- the tenant pool checked but the process pool not (3);
- 429 for a reservation that can never fit (1 asks for 413).

## RA budget
Unchanged: the reservation is local arithmetic under one mutex; no blob request, no round.

## Risks
- A legitimate request above 1,048,576 candidate rows (16 sub-queries of 8 named dense legs at
  `top_k` 10,000) is now 413. It was memory nothing bounded; the migration notes say how to
  split it.
- Rows are not bytes: a tenant of wide rows can still hold more memory per row. The plan's
  byte item stays open in DELIVERY.md.

## Tasks
- M212.1: limits, reservation, handler, tests, docs. DELIVERY.md's A admission row names M212.
