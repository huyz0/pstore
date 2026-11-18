# M280: tenant borrowing at admission, and a per-tenant cache share

## Serves
D-21 (class quotas) and M128's admission, through lesson 4 of
[lessons-from-hsearch-opensearch.md](../../comparisons/lessons-from-hsearch-opensearch.md).

## Delta
1. **Borrowing admission** (`RequestLimits::tenant_borrow_reserve`). Past its own cap
   (`tenant_active_requests`, 8), a tenant may take one more server slot while more than
   `reserve` slots would stay free after it. `None` is M128's fixed cap; a reserve of
   `active_requests` or more lets nobody borrow. Configured in `PSTORE_REQUEST_LIMITS_CONFIG`
   by `tenant_borrowing` (`false` wins) and `tenant_borrow_reserve` (which turns it on).
2. **A per-tenant cache share** (`CacheCore::with_tenant_share`,
   `PSTORE_CACHE_TENANT_SHARE_PERMILLE`): the most of each class's **memory** quota one
   tenant may hold. Past it, a tenant's admission evicts its own least recently used entries,
   never another's. A key's tenant is the third segment of `{shard}/tnt/{t}/...` or
   `{shard}/wal/{t}/...`; any other key is no tenant's and is never capped. The disk tier is
   not capped.
3. **Defaults from measurement** (criteria 1 and 2): borrowing **on** with a reserve of 8; the
   cache share **off**.

Not changed: the fixed cap's own number, the body-byte pool, class quotas.

**Settings, with a deviation recorded.** Both are cluster settings. Neither has a per-index
override: admission runs before the request's index is known or any HEAD is read, and the
cache sits beneath the engine and keys by object; reading an index's setting at either place
would be a request per request. A tenant-level override would need the same read. Recorded in
the [decisions doc](../../comparisons/lessons-decisions-2026-11-17.md).

## Acceptance criteria
1. **Admission, five workloads** (`borrowing_admission_measured_against_the_fixed_cap`,
   deterministic, exact): hot tenant alone 8 vs 56 slots; hot + one small tenant a step, none
   refused either way; hot + bursts of 8, none refused borrowing; hot + bursts of 24, none
   refused fixed and 160 of 240 borrowing; small tenants only, identical. Borrowing wins three,
   ties one, loses one: default on.
2. **Cache, four workloads** (`a_tenant_share_measured_against_one_shared_lru`, exact hit
   ratios at a quarter share): a cold scan beside 4 small tenants 0 vs 1000 permille; one
   tenant with a working set of half the quota 1000 vs 0; 8 small tenants identical; two
   tenants of 24 entries 1000 vs 0. The share wins one, ties one, loses two: default off.
3. **Both modes tested.** M128's cap tests pin `tenant_borrow_reserve(None)`:
   `a_permit_returns_everything_and_idle_tenants_leave_no_entry` and `tests/it/ingress.rs`'s
   `limits`; `the_fixed_cap_is_m128s_and_borrowing_keeps_its_reserve`,
   `a_reserve_the_server_cannot_leave_free_is_the_fixed_cap`,
   `borrowing_is_configured_off_on_or_by_its_reserve`;
   `no_share_is_one_shared_lru_and_a_share_of_1000_is_none`,
   `a_key_of_no_tenant_is_never_capped`, `the_cache_is_configured_by_name`,
   `a_bad_cache_setting_is_refused_by_name`.

## Test plan
Each measurement is a deterministic simulation whose numbers are asserted exactly: a change to
either mode moves a number and fails.

## RA budget
Unchanged: admission and the memory tier issue no request.

## Risks
- **Slots are not latency.** The simulation counts admitted slots. A hot tenant holding 56
  slots also competes for CPU with the small tenants it admits beside it; that is unmeasured,
  and is the first thing to measure on a real node (M0b).
- **No preemption**: a small tenant arriving to a full reserve waits for a slot to free.

## Tasks
- **M280.1** The spec.
- **M280.2** Borrowing, its simulation, its configuration.
- **M280.3** The cache share, its measurement, its configuration, docs.
