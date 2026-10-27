# M136: a by-filter operation is bounded, and refused whole above its bound

## Serves
Workstream C of [the delivery plan](../../comparisons/turbopuffer-next-plan.md): "partial
filter mutations expose progress and resume without skipped/duplicate effects". This is
the first slice. It bounds what one by-filter operation may change, which the progress
slice ([M137](#tasks), `allow_partial`) and ids for by-filter operations both need.
[M13.2](../M13/SPEC.md)'s Risks name the defect: a by-filter operation over a huge match
set holds that many rows, vectors included, in one fold. And D-56 in [memory
management](../../research/09-rust-stack/memory-management.md): memory a tenant's write can
make a node hold must be bounded.

## Delta
Today a `delete_by_filter` or `patch_by_filter` resolves inside one fold, and the fold
keeps every row its filter admits, with vectors, for the whole index. Memory scales with
the match set, without bound.

After: each by-filter operation has a row bound, `FILTER_ROWS`, 100,000 by default
(`Engine::with_filter_rows(n)` lowers or raises it; tests use a small one). An operation
is **refused whole** when its filter admits more than the bound, counted either:
- over the index's current versions as the fold read them (the scan), or
- at its own place in fold order (after this fold's earlier operations).

A refused operation changes no row, and its write's applied outcome counts it once in
`rejected`. Every other operation of the fold, of the same write too, applies as before.

How the counts are kept:
- The scan counts, per by-filter operation in the order the index's operations arrive,
  the **current** rows its filter admits: a row a delete vector buries is skipped before
  it is counted, as today, and supersede keeps one current version per id. A filter the
  fold cannot read counts nothing, and its operation stays `skipped`.
- Segments are read concurrently, so the counters are atomics. Which rows past the bound
  were retained varies between runs; it does not matter, because an operation past the
  bound is refused whatever was retained.
- The counters live inside one commit attempt, beside the scan, so a CAS retry counts
  afresh.
- The scan stops retaining a row for an operation once that operation's count passes the
  bound. A row is still retained when a per-id operation names it, or another operation
  under its bound admits it.
- `resolve` gets, per by-filter operation, whether the scan found it past the bound.
  `by_filter` refuses such an operation without applying it; otherwise it first collects
  the ids it admits in fold order, and refuses if they pass the bound.

The bound is **per operation**: whole rows a fold holds for by-filter operations are at
most `ops × (bound + 1)`, plus the rows per-id operations name. A fold merging many
writes can still hold many bounds' worth, and the scan still holds an id-only row for
every row of the index (`Prepared.rows`, for the supersede). Neither is closed here.

⚠️ **A behaviour change for existing callers**: a `delete_by_filter` or `patch_by_filter`
admitting more than 100,000 rows applied before and is refused after. Sent `durable`, the
refusal is told to nobody, and the fold moves past it. `docs/api-migrations.md` says so
plainly and says to split such a write by a narrower filter, or wait for M137's
`allow_partial`. There is no operator setting in this slice; the server uses the default.

Unchanged: per-id operations; a filter the fold cannot read is `skipped`, as today;
operation order; HEAD, bundle and outcome object formats; the server's request shape.
`rejected` already means "operations discarded" (M129), so no field is added. The
`Outcome::rejected` and `AppliedOut::rejected` rustdoc and `docs/api-migrations.md`'s
M129 entry name the bound as one more reason.

## Acceptance criteria
1. A `delete_by_filter` whose filter admits exactly the bound deletes every one of those
   rows, and its outcome is `deleted = bound`, `rejected = 0`.
2. One that admits bound + 1 rows deletes none of them. The write's outcome counts
   `rejected = 1`, and an upsert in the same write still applies. The same for
   `patch_by_filter`: no row changes.
3. Fold order counts: a `patch_by_filter` earlier in the same write makes a later
   `delete_by_filter` admit bound + 1 rows where the index admitted only bound. The delete
   is refused; the patch applies.
4. The scan counts too: a filter admitting bound + 1 rows of the index, of which an earlier
   `delete` in the same write removes one, is still refused. This pins the stated rule.
5. The scan's retention is bounded: a unit test of the retention counter shows that of N
   rows a filter admits, at most bound + 1 are retained for it, and every row a per-id
   operation names is retained whatever the counts.
6. The fold uses the bound end to end: with bound 3, a refused `delete_by_filter` whose
   filter admits 10 rows leaves the fold holding at most 4 whole rows, read through a
   `#[doc(hidden)]` test hook recording the most whole rows the last fold's scan kept
   (the `reapable_len_for_test` pattern). Today it holds 10.
7. Every existing by-filter, patch and applied test passes unchanged.

## Test plan
New `crates/pstore-engine/tests/it/filter_rows.rs` (a `mod` line in `tests/it/main.rs`),
with an engine built `with_filter_rows(3)` and applied writes (`applying`,
`await_applied`), reading rows back after. Criterion 5 is a unit test beside the counter
in `pstore-engine/src/filter_rows.rs`. Red runs: the builder lands first as a stub that
stores the bound and does nothing else, since every engine test is one binary and would
not compile without it. Against the stub, 1 is a guard, green; 2, 3, 4 and 6 are red;
5 does not compile until the counter exists, and its red is the "retention ignores the
count" mutation.
Mutations, each to be killed:
- the bound compares `>=` instead of `>` (kills 1);
- the fold-order count is skipped (3), the scan count is skipped (4);
- a refused operation counts `skipped` instead of `rejected` (2);
- retention ignores the count (5, 6); retention drops a row a per-id operation names (5).

## RA budget
No request is added or removed: the scan already reads every segment of the index. No
LIST, nothing scaling with records. Memory: by-filter retention goes from the match set to
at most `bound + 1` rows per operation.

## Risks
- The rule is stricter than "changes more than the bound": an operation that admits
  bound + 1 rows but would change fewer (a patch already applied to some) is refused. The
  rule counts what the fold must hold, which is admissions.
- Whether an operation is refused can depend on which other writes share its fold
  (criterion 3), as conditions already can. A client that must know uses
  `durability: "applied"` and reads `rejected`.
- A `durable` (not applied) write is told nothing of a refusal, as today for a schema
  reject. The docs say so.
- `rejected` does not say why. Splitting it is a format change, left to M137 with
  `rows_remaining`.

## Tasks
- M136.1: the bound, its builder, the retention counter, the refusal in `by_filter`, the
  test hook, the tests, the `rejected` rustdoc and `docs/api-migrations.md`.
- M137 (next, its own spec): `delete_by_filter_allow_partial` and
  `patch_by_filter_allow_partial`: apply the first bound rows in id order and report
  `rows_remaining`, so sending it again resumes.

## Review
Spec review round 1 (blocking on one major, amended): nothing tested that the fold uses
the counter, so criterion 6 reads what the fold kept through a test hook. Minors taken:
how the scan's counts reach `by_filter` and that they are per attempt; atomics, and
current rows only; the bound is per operation and `Prepared.rows` is unchanged; the
behaviour change for existing callers is stated; the red-run plan names the stub; D-56.
Minor not taken as a criterion: the docs wording, which no test can check; it is a task.
