# M269: the turbopuffer compatibility facade, part 2: writes by filter and condition, affected ids, multi-query

## Serves
Workstream B of [the delivery plan](../../comparisons/turbopuffer-next-plan.md): part 2 of
about four of the facade [M268](../M268/SPEC.md) began. The pinned client is still the
vendor's MIT Python SDK, `turbopuffer` 3.1.0 (`types/namespace_write_params.py`,
`namespace_write_response.py`, `namespace_multi_query_params.py`,
`namespace_multi_query_response.py`). Later parts take `explain_query`, schema changes,
`hint_cache_warm`, `recall`, copy and branch with their operations, and the compatibility
manifest.

## Delta
Every facade request is still **one native call** and adds no blob request and no round trip.
M268's rules hold: identity, the error envelope, and a permanent 409 answered 400.

1. **Writes by filter and by condition.**
   - These pass through unchanged: `delete_by_filter`, `upsert_condition`,
     `patch_condition`, `delete_condition`, `delete_by_filter_allow_partial` and
     `patch_by_filter_allow_partial`.
   - `patch_by_filter: {"filters", "patch"}` becomes the native
     `{"filters", "attributes"}`. Any other key in it is refused, naming the key.
2. **Affected ids.** `return_affected_ids: true` becomes the native `affected_ids: true`.
3. **Durability.** A write whose answer depends on the rows it changed is **`applied`**, and
   every other write stays `durable` as in M268. Such a write carries a condition, a
   by-filter operation or `return_affected_ids`. The vendor's answer counts the rows the
   write changed, and only an applied write knows them.
   - If the server does not serve applied writes (`PSTORE_APPLIED_WRITES` unset), such a write
     gets the native `unsupported_durability` refusal, a 400.
4. **Write answers.** An applied write answers from the native outcome:
   - `rows_upserted`, `rows_patched` and `rows_deleted` are the rows changed, not the rows
     requested, and `rows_affected` is their sum.
   - `rows_remaining` is sent when the native outcome has it.
   - `upserted_ids`, `patched_ids` and `deleted_ids` are sent when asked for, and only when
     the list is not empty, as the SDK's docstrings say.

   An applied write whose outcome can no longer be read (`applied: null`) answers 500
   `outcome_unavailable`, never invented counts. Re-sending a conditional or by-filter
   write evaluates it against the rows as they are then.
5. **Multi-query.** A query body holding `queries` is a multi-query, as the SDK's `multi_query`
   sends it to the same path:
   - Each sub-query is translated as M268's Delta 4 translates a query.
   - The root's `consistency` and `vector_encoding` apply to every sub-query. A sub-query
     carrying its own `consistency` is refused, since the vendor sets it on the root only.
   - The answer is `{"results": [{"rows", "aggregations", "aggregation_groups"}],
     "performance"}`, with one result per sub-query, in order.
   - The root's `rerank_by`, `limit` and `offset` are refused, naming the key, for a later
     part. Their grammar is the vendor's, not the native M142 one.

**No setting.** Only facade translations change: nothing a native request sends or receives
changes.

## Acceptance criteria
1. **The SDK, against pstore** (`./scripts/facade.sh`; Python test names carry no code
   marks):
   - delete_by_filter removes the matching rows, and rows_deleted counts them;
   - patch_by_filter sets an attribute on the matching rows;
   - an upsert_condition that admits one of two rows writes only that row, with rows_upserted
     1 and upserted_ids holding its id;
   - delete_condition and patch_condition the same way;
   - return_affected_ids on a plain write gives the ids;
   - multi_query with two sub-queries, a vector and an order, under an eventual root
     consistency, answers two results that equal the two native queries' rows;
   - multi_query with rerank_by is refused BadRequestError, naming it.
2. **Translation, in process** (`crates/pstore-server/tests/it/facade.rs`):
   - `facade_filters_and_conditions_are_native_calls`: each of Delta 1–4's mappings against
     the native applied write's outcome, with the counts and ids equal;
   - `facade_multi_query_is_the_native_multi_query`: Delta 5's mapping, row by row, against
     the native `{"queries"}`;
   - `facade_adds_no_request` extended: a conditional write and a two-query multi-query cost
     the native call's requests and depth;
   - and in `facade.rs`'s own tests, `a_write_is_applied_only_when_its_answer_needs_it` (the
     durability of Delta 3, for each trigger and for none).
3. **Gates:** fmt, clippy, the workspace tests, `scripts/facade.sh`, and the named mutations
   observed red.

## Test plan
AC1 and AC2 are red against M268, which refuses each of these keys.

| AC | Mutation |
|---|---|
| 2 | `patch` not renamed `attributes`; a conditional write kept `durable`; requested counts answered instead of the outcome's; an empty id list sent; the root consistency not applied to a sub-query |

## RA budget
None: each facade request is one native call, and `facade_adds_no_request` holds it.

## Risks
- **`applied` costs what an applied write costs.** It waits for the folds that decide it, as
  M129 measured. The vendor's conditional writes are synchronous too, so the facade asks for
  nothing the vendor does not.

## Tasks
- **M269.1** The spec.
- **M269.2** `facade.rs`, the in-process tests, the SDK tests, docs.
