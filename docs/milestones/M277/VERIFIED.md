# M277 verification

Run with `cargo test -p pstore-server --test it puts_per_write -- --nocapture`, three times,
identical output each time:

```
sparse durable, 100 tenants: Cost { first: 200, steady: 100, fold: 200 }
sparse batched, 100 tenants: Cost { first: 0, steady: 0, fold: 0 }
8 concurrent durable writes, one tenant, 20 ms a request: 2 PUTs
64 concurrent durable writes, 64 tenants, 20 ms a request: 64 PUTs
```

⚠️ `provisional`: an in-memory store with an injected 20 ms per request, not S3.

1. `a_sparse_durable_write_costs_one_put_per_tenant_and_a_fold_two` passes. Red: with the
   fold's figure moved from 2 to 1 per tenant it failed.
2. `a_sparse_batched_write_costs_nothing_until_its_fold` passes. OBSERVED-NOT red: its
   figures are zeros, and no edit of the test alone makes a zero count wrong without also
   changing the server.
3. `concurrent_durable_writes_to_one_tenant_share_bundles` passes (2 PUTs). Red: with the
   bound at 1 it failed. On the plain memory store the same 8 writes cost 8 PUTs, which is
   why the test injects latency.
4. `concurrent_durable_writes_to_many_tenants_cost_one_put_each` passes (64 PUTs). Red: with
   the figure at 1 it failed.
5. NOT-RUN as a test: the decision is prose, in
   `docs/comparisons/lessons-decisions-2026-11-17.md`, section "Lesson 1".
