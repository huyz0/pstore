# M271: the turbopuffer compatibility facade, part 4: copy, branch and the async copy

## Serves
Workstream B of [the delivery plan](../../comparisons/turbopuffer-next-plan.md): part 4 of
the facade [M268](../M268/SPEC.md) to [M270](../M270/SPEC.md) built. The pinned client is
still the vendor's MIT Python SDK, `turbopuffer` 3.1.0 (`resources/namespaces.py`:
`branch_from`, `copy_from`, `start_copy_from`, `poll_copy_from`;
`types/namespace_write_params.py`, `copy_from_namespace_operation.py`,
`copy_from_namespace_operation_result.py`, `operation_error.py`). M272 takes the
compatibility manifest.

## Delta
M268's rules hold: identity from the credential, the vendor's error envelope, and a permanent
409 answered 400. Each request is **one native call**.

1. **Branch and copy, synchronous.** The SDK's `branch_from`, `copy_from` and
   `write(copy_from_namespace=...)` each send a write to `POST /v2/namespaces/{ns}` holding
   `branch_from_namespace` or `copy_from_namespace`. The value is a name or
   `{"source_namespace"}`. The `?stainless_overload=` query the SDK adds is ignored: the body
   says what is asked.

   Each becomes the native write `{"branch_from_namespace": source}` (M16). That is a
   copy-on-write branch made in one commit: the destination shares the source's segments
   and takes writes of its own. A vendor copy is a full copy. Readers cannot tell the two
   apart, and the branch costs no row writes.
   - The answer is M268's write answer from the native one. `rows_affected` is 0, because
     the branch writes no rows. No copied-row count is invented.
   - `source_api_key`, `source_region` and `dest_encryption` in the source are refused,
     naming the key. They name another organization, region or key, which pstore's
     identity does not reach.
   - Both keys at once, a source without `source_namespace`, or a source holding anything
     else is refused, naming what is wrong.
   - Anything beside the source in the write is translated and sent, and refused natively
     (M122: a branch comes alone).
2. **The async copy.** `start_copy_from` is `POST /v2/namespaces/{ns}/async` with
   `copy_from_namespace`, as in item 1. It becomes the native copy (`POST /v1/indexes/{ns}/copy` with
   `{"source": {"index"}}`, M166), and the answer is `{"token"}`, the native copy's `run`
   as a string. The same three keys are refused as in item 1.
3. **Polling.** `poll_copy_from` is `GET /v1/namespaces/{ns}/operations/{token}`. It becomes
   the native copy status (`GET /v1/indexes/{ns}/copy`).
   - A `running` or `paused` copy answers `{"status": "running", "start_time", "progress"}`.
     `progress` names the segments done of the total when the native status has them.
   - A `succeeded` copy answers `{"status": "finished", "start_time", "finish_time",
     "result": {"success": {"status": "OK", "message", "rows_affected": 0}}}`.
   - A `failed` or `cancelled` copy answers the same with `"result": {"error":
     {"status_code", "detail": {"status": "error", "error"}}}`. `status_code` is 404 for
     `source_not_found` and 409 for the rest, and `error` is the native code and last
     error.
   - A token that is not the native copy's `run` is 404 `operation_not_found`. A
     destination with no copy is the native 404.

**No setting.** Only facade translations change: nothing a native request sends or receives
changes.

## Acceptance criteria
1. **The SDK, against pstore** (`./scripts/facade.sh`; Python test names carry no code
   marks):
   - branch_from makes a namespace whose rows a query reads as the source's;
   - copy_from does the same, and a write to the copy leaves the source unchanged;
   - write(copy_from_namespace=...) does the same;
   - start_copy_from answers a token, and poll_copy_from reaches finished with a success
     result, within 60 s, after which the copy reads the source's rows;
   - poll_copy_from with another token is refused NotFoundError;
   - copy_from with source_region is refused BadRequestError, naming it.
2. **Translation, in process** (`crates/pstore-server/tests/it/facade.rs`):
   - `facade_branch_and_copy_are_the_native_branch`: each of item 1's forms against the
     native branch, rows compared, with the refusals;
   - `facade_async_copy_is_the_native_copy`: items 2 and 3, the token being the native
     `run`, a running status and a finished one after the replication worker runs, each
     against the native status, and a missing source refused 404 as the native copy refuses
     it before starting;
   - `an_operation_is_the_native_copys_state` in `facade.rs`'s own tests: each native state
     (running, paused, succeeded, failed, cancelled) as the vendor's operation, with the
     `status_code` each failure gets and a token mismatch. A failed run cannot be reached in
     process, because the native copy checks its source before it starts;
   - `facade_adds_no_request` extended: a branch, a start and a poll each cost the native
     call's requests.
   - M268's refusal case (`facade_errors_are_the_vendors`) and the SDK's refusal test refuse
     `sharding` instead of `copy_from_namespace`, which is now served. A vendor key pstore
     cannot honour is still refused, naming it.
3. **Gates:** fmt, clippy, the workspace tests, `scripts/facade.sh`, and the named mutations
   observed red.

## Test plan
AC1 and AC2 are red against M270: it refuses `copy_from_namespace` and
`branch_from_namespace`, and has no async or operations route.

| AC | Mutation |
|---|---|
| 2 | a copy sent as a durable write rather than a branch (`durability` kept beside it); `source_region` passed rather than refused; the token not the native `run`; a token mismatch answered as the copy; `failed` answered `running`; `status_code` 409 for `source_not_found` |

## RA budget
None. Each route is the native call it names, and `facade_adds_no_request` holds them.

## Risks
- **The finished copy's result is untyped in the SDK.** The SDK's `WriteResult` requires
  `billing`, and the delivery plan forbids inventing vendor billing. So the success carries
  none, and the SDK hands `result` back as a dict rather than its `Success` model. A caller
  reading `result.success` gets an `AttributeError`, and `result["success"]` works.
- **The SDK's request shapes were read from its traffic.** The generated resource methods
  name a top-level `source_namespace`, but the namespace wrapper the SDK hands callers sends
  `copy_from_namespace` or `branch_from_namespace` for every form, async included. The
  facade serves what is sent. That was captured from the pinned SDK's own requests, and the
  SDK suite runs each form.
- **A copy is a branch.** Writes to the destination are its own, as the vendor's are. GC
  keeps the shared segments while either index names them (M16). A vendor caller who
  expects copied bytes to be billed sees no row writes.

## Tasks
- **M271.1** The spec.
- **M271.2** `facade.rs`, the routes, the in-process tests, the SDK tests, docs.
