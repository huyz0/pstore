# M133: `$ref_new` in write conditions

## Serves
Workstream C of [the delivery plan](../../comparisons/turbopuffer-next-plan.md): "conditional
reads use the specified old row and `$ref_new` values; absent/null rules, repeated IDs, and
conflicts are pinned. A 100-writer adversarial schedule never reports two mutually
exclusive conditions as successful. Successful responses survive restart."

## Delta
Today a write condition ([M13](../M13/SPEC.md)) compares the current row with constants
only. A "newer version wins" write cannot be stated.

After: in `upsert_condition` and `patch_condition`, the value of a comparison may be
`{"$ref_new": "<attribute>"}`, meaning that attribute's value in this operation's new row.

- **Where it may appear.** Only as the value of `Eq`, `NotEq`, `Lt`, `Lte`, `Gt` or `Gte`,
  at any depth under `And`, `Or` and `Not`. Every other place is `400 bad_request`:
  - any other operator;
  - `delete_condition`, which has no new row;
  - a by-filter operation, and a query filter. Both already refuse an object value.
  - The object must have exactly the key `$ref_new`, and its value must be a string. A name
    that is empty or begins `$` is refused, as a filter attribute is (M40).
- **What it resolves to.** It is resolved at the door, once per operation, after the
  document is converted, so a dated attribute resolves to a datetime:
  - upsert: the document's attribute;
  - patch: the value the patch sets. An attribute the patch unsets or does not name is
    absent. The current row is not consulted, so the door reads nothing.
  - `id` resolves to the operation's id.
- **Absent and null.** A missing attribute and `null` are the same thing (M13). Against an
  absent reference:
  - `Eq` means the current row's attribute is absent;
  - `NotEq` means it is present;
  - `Lt`, `Lte`, `Gt` and `Gte` are false.
  - A present reference compares as the constant would, so a type mismatch is false.
- **Engine.** `write_conditioned(index, docs, conds, metric, declared)` and
  `patch_conditioned(index, patches, conds)` take one predicate per operation. Their length
  must match the operations' (else a `Format` error, and nothing is buffered). Each
  operation's `$cond` is its own predicate.
  - The bundle bytes and the fold are unchanged. `$cond` is already carried per operation,
    and the fold already evaluates each operation against the version its order puts
    before it.
  - A condition with no reference keeps today's path and its bytes.
- **Repeated ids.** Operations on one id in one request resolve in request order, each
  against the version the one before it left (M13). Each carries its own resolved
  condition.

Unchanged: conditions without `$ref_new`; `delete_condition`; by-filter operations; and the
bundle, HEAD and outcome formats. Conditions stay durable-only (M13).

## Acceptance criteria
1. Upsert, "newer wins": with `["v", "Lt", {"$ref_new": "v"}]` over a row at `v = 5`, an
   upsert at 7 applies and one at 3 does not. Rows are read back, and the applied outcome
   (M129) is `upserted 1` and `skipped 1` respectively.
2. Patch: with `patch_condition ["v", "Lt", {"$ref_new": "v"}]`, a patch setting `v` applies
   when larger and not when smaller. A patch that does not set `v` never applies (`Lt` is
   false against an absent reference).
3. Absent and null: an upsert whose document lacks `v`, under `["v", "Eq", {"$ref_new":
   "v"}]`, applies only over a row that lacks `v`. `NotEq` applies only over a row that has
   `v`. `Lt`, `Lte`, `Gt` and `Gte` never apply over an existing row. An upsert of a
   missing id applies, as M13 says.
4. `{"$ref_new": "id"}` resolves to the operation's id. A dated attribute compares as a
   datetime.
5. Repeated ids: one request upserting `x` at `v = 2` then `v = 1`, under "newer wins" over
   `v = 0`, leaves `v = 2`, with outcome `upserted 1`, `skipped 1`. At `1` then `2`, it
   leaves `v = 2` with `upserted 2`.
6. Adversarial: 100 applied writers across 4 servers on one store.
   - **Exclusive claim:** each upserts `lock` with `{"owner": i}` under
     `["owner", "Eq", null]`. Exactly one response reports `upserted 1`, and the row's owner
     is that writer's.
   - **Newer wins:** each upserts `x` at `v = i` under `["v", "Lt", {"$ref_new": "v"}]`.
     The final `v` is 99, the writer of 99 reports `upserted 1`, and every response is
     `upserted 1` or `skipped 1`.
   - **Restart:** a server started afterwards on the same store reads both rows exactly as
     the responses imply.
7. Refusals (400, nothing buffered, the index not made): `$ref_new` in `delete_condition`,
   under `In`, `Contains` or `Glob`, with a non-string or reserved name, an object with a
   second key, and in `patch_by_filter`'s filter and a query filter.
8. The engine refuses a length mismatch between operations and conditions, and buffers
   nothing.
9. A condition with no reference writes the same `$cond` bytes as before (an existing M13
   test passes unchanged).

## Test plan
Each test is run red first: against the parent, where `$ref_new` is refused as a malformed
value, or where the engine methods do not exist.
- 1 to 5: `crates/pstore-server/tests/ref_new.rs`, over HTTP at `durability: "applied"`
  (`PSTORE_APPLIED_WRITES=1`). Mutations:
  - "resolve against the current row instead of the new one" (1 and 2 flip);
  - "absent reference means Eq null for every operator" (3);
  - "the patch reference reads the unset list as present" (2);
  - "one condition for the whole request, from the first document" (5);
  - "`id` reference resolves to the attribute named id" (4).
- 6: `crates/pstore-server/tests/ref_new.rs`, `adversarial_writers_never_both_win`. Mutation:
  "the fold evaluates conditions against the folded version only, ignoring this fold's
  earlier operations". Any second claim then reports success.
- 7: `ref_new_refusals`. Mutations: each refusal deleted, one at a time.
- 8: `crates/pstore-engine/tests/ref_new.rs`, `lengths_must_match`. Mutation: zip without
  the length check.
- 9: the existing `crates/pstore-server/tests/patch.rs` suite passes unchanged.

## RA budget
Unchanged: zero new blob requests on the write path and the fold, and no LIST. The door
resolves from the request alone. The bundle carries one `$cond` per conditional
operation, as it already did; only its value may now differ per operation.

## Risks
- **The reference resolves at the door, not at the fold.** That is correct for an upsert,
  whose new row is the request. For a patch the "new row" could be read as the merged row,
  which only the fold knows. This spec pins it as the patch's own values; the merged-row
  reading would need fold-side evaluation. Revealed by criterion 2.
- **Per-operation conditions grow the bundle** when every operation's condition differs.
  The size is the same as today's per-operation copy, so this is not a new cost.

## Tasks
- M133.1: engine `write_conditioned` and `patch_conditioned`, with criterion 8.
- M133.2: server template parse and resolution, criteria 1 to 7 and 9, and
  `docs/api-migrations.md`.
