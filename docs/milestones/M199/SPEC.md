# M199: a document export at one epoch

Spec review round 1 failed on four majors: the export named no schema; AC 2 could not tell the
present's metric from the default; the RA budget missed a store without suffix reads; and AC 3
named no mutation. Each is addressed below, with the minors.

## Serves
Workstream H of [the delivery plan](../../comparisons/turbopuffer-next-plan.md): "Export
documents from a fixed retained snapshot, including IDs/types/schema, so a restore into a fresh
tenant produces the same exact-query/filter results and row digest ... Vendor live paged export
is not itself a stable-snapshot guarantee", the document-format export
[M196](../M196/SPEC.md) left pending. Partly: the schema comes from the index summary read that
picks the epoch, so this exports a live index. An index dropped since is restored by M196's
`as_of` copy instead.

## Delta
Today a client can page an index's documents with an order (`rank_by` an attribute) and
`include_vectors` (M179, M190, M191, M193). Each page reads the present, so pages taken while
the index is written do not describe one state. `as_of` with `include_vectors` is refused (`400`
"a past epoch's vectors are later").

After:
- **An order with `as_of` projects vectors.** `rank_by` an attribute with `as_of: E` and
  `include_vectors` answers the page from `E`'s segments, with each row's vectors as of `E`.
  Every page asked `as_of E` describes the same state, whatever was written since. This is the
  document export: page with `rank_by ["id", "asc"]`, `as_of E`, `filters ["id", "Gt", last]`
  and `include_vectors` naming every field.
- **Engine:** `Engine::ordered_projected_as_of(index, by, filter, offset, limit, epoch, vector,
  named, sparse)`, the existing order path (`ordered_at`) with the epoch it already takes.
- **The metric a past epoch's vectors are given back in** is the one the index had at `E`
  (`schema_recorded_at`, as a query `as_of` scores). Today an order `as_of` takes the present
  schema's metric, or the default one when the index has been dropped since. That turns a
  euclidean index's stored vectors back into client space with the wrong rule, and leaves the
  extra stored dimension in place. The same fix applies to an order `as_of` without vectors,
  which uses the metric only for vectors, so it answers the same.
- **Server:** the refusal moves from `vector_projection`, which runs before the plan is known,
  to the ranked arm of the plan.
- **Still refused:** a ranked query (a vector, text or sparse leg) with `as_of` and
  `include_vectors` (`400`): its candidates' vectors ride a row fetch that has no past-epoch
  path. Also refused, as for any `as_of`: `consistency: strong`, an epoch after the present, or
  one below the reap horizon (`400 time_travel_horizon`).
- **Docs:** `docs/api-migrations.md` gains the export loop, and how to choose `E` and the
  schema together: one `GET /v1/indexes/{index}` reports both (`epoch` and `schema`, from one
  HEAD read). `as_of E` holds only rows folded by `E`: a write acknowledged but not yet folded is not in it, so an operator folds first (`POST /v1/admin/fold`), or the export says it covers what was folded at `E` (amended after spec review round 2: the summary has no unfolded flag to poll). It also gains the restore (write the pages, with their
  vectors and attributes, into a new index under the same schema). The page's rows carry what the
  index stores: cosine vectors come back normalized, as M179 says. A restore reproduces the
  exact-query and filter answers and the row digest, but not the client's original bytes.

Not delivered: a server-side export job that writes files, a GC hold on `E` (an export that
outlives the reap horizon gets `time_travel_horizon` on its next page), and measured RPO/RTO.

Not changed: an order or query without `as_of`; a ranked query `as_of` without vectors; the
projection's bounds (1,000 rows a page, M191's bytes).

## Acceptance criteria
1. **Pages at one epoch** (engine): an index with a vector, a named field, a sparse field and
   attributes is folded at `E`. After `E` it takes a delete, a replacement, new rows and a
   compaction. Paging it `as_of E` 7 rows at a time with `id > last` returns
   exactly the rows, attributes and vectors an unpaged `ordered_projected` returned before the
   first change.
2. **The past metric** (engine): a `euclidean_squared` index is dropped after `E` and re-created
   under `dot_product`. Its rows `as_of E` carry each vector exactly as written, with no extra
   dimension. A second index is dropped twice after `E` (`euclidean_squared` at `E`, then
   `dot_product`), and its rows `as_of E` are the same: the first drop's schema, not the later's.
3. **A restore** (engine): AC 1's index under `dot_product` (so a vector written back is the
   vector stored). Its pages are written into a new index of a fresh tenant under the schema the
   summary reported. Its full digest equals the source's at `E`: every row by id, with its
   attributes and every vector field. So do its exact-vector and filter answers.
4. **Server:** an order with `as_of` and `include_vectors` answers `200`, with vectors equal to
   the same order's answer taken at `E` before later writes. A ranked query with both is `400`.
   So is a scored query (`rank_by` an expression) with both. An epoch after the present is
   `400 time_travel_horizon` (a regression check: existing behaviour).
5. **Gates:** fmt, clippy, suite, coverage 95/95, mutants.

## Test plan
| AC | Test | Mutation it catches |
|---|---|---|
| 1 | engine `an_order_pages_one_past_epoch_with_its_vectors` | the epoch not passed (the present paged); a field not projected |
| 2 | engine `a_past_epoch_gives_vectors_back_in_its_own_metric` | the present's metric used; the default used; the latest drop's schema, not the first after `E` |
| 3 | engine `a_restore_of_a_paged_export_answers_as_its_epoch` | none beyond AC 1's: it is an end-to-end check of the loop the docs give (amended after spec review round 2: the loop is the test's, not shipped code, and a repeated boundary row upserts the same id) |
| 4 | server `an_export_page_takes_as_of_and_vectors` | the refusal kept, or lifted for a ranked or scored query |

An order `as_of` without vectors reads the metric only for vectors, so a mutation of the metric
there is equivalent.

## RA budget
An order `as_of` with vectors costs what the same order costs at the present: its rounds, plus
M179's one round more for the vectors (M191's shared round for multi-vector and sparse). No
request is added, and nothing is added to a write. ⚠️ A segment that `Head::as_of` resurrects
has no recorded length, so on a store without suffix reads (Azure) opening it is a HEAD and
then a GET: one sequential round more than the same order at the present. It is the depth every
query `as_of` already pays there (M46).

## Risks
- A page whose rows sit in a segment folded before M193 (sparse) or M191 (multi-vector) refuses
  `422`, as at the present. A compaction rewrites it, but only for epochs after the compaction.
- A restore is checked under `dot_product` with full-width vectors only; a quantized field (M195 `f16`, `i8`) written back is quantized again, which no test here checks.
- A long export can cross the reap horizon. Its next page is refused, so it is never silently
  mixed with another epoch.

## Tasks
- M199.1: the engine call and the past metric.
- M199.2: the server's refusal narrowed, the docs, and the DELIVERY row.

## Review
Round 1 (a subagent given the spec and the code, read-only): four majors, all taken. The
schema to restore under comes from the same summary read as `E`; Serves is narrowed (a dropped
index goes to M196's copy); AC 2 tells the present's metric, the default and a later drop's
schema apart; the RA budget states the resurrected ref's extra round on a store without suffix
reads. The refusal moves to the ranked arm, with the scored query asserted. Round 2: PASS.
Minors taken: AC 3 names no mutation of its own, the docs say how to fold first, and the
quantized restore is a stated gap.
