# M137: `allow_partial` by-filter writes apply up to the bound and say what remains

## Serves
Workstream C of [the delivery plan](../../comparisons/turbopuffer-next-plan.md): "partial
filter mutations expose progress and resume without skipped/duplicate effects", and the
`delete_by_filter_allow_partial` and `patch_by_filter_allow_partial` rows of [the
comparison](../../comparisons/turbopuffer-2026-10-25.md). Builds on
[M136](../M136/SPEC.md)'s bound, under D-56 in [memory
management](../../research/09-rust-stack/memory-management.md).

## Delta
Today a by-filter operation past the bound is refused whole (M136), and a client with more
rows to change than the bound has to split its filter by hand.

After: a write may set `delete_by_filter_allow_partial: true` or
`patch_by_filter_allow_partial: true` beside its by-filter operation.
- Such an operation is never refused for its size. It applies to at most the bound's rows
  it **would change**, and its write's applied outcome says `rows_remaining: true` when
  rows it would change were left: when the scan counted more than the bound (M136's scan
  count, of rows it would change), or when, at its place in fold order, more than the
  bound would change. The scan count can overstate, when an earlier operation of the same
  fold deleted a counted row; a spurious `true` costs the client one pass that changes
  less.
- In fold order it takes, in id order, only rows that **would change**: a row the filter
  admits that the operation would leave as it is, retained because a per-id operation
  names it, is passed over, not counted against the bound.
- `rows_remaining` is per request, not per operation: a write with a partial patch and a
  partial delete gets one flag, true when either left rows.
- **Resume is sending the same write again.** A row already deleted no longer matches; a
  row already patched to the same values would not change, so it is not counted and not
  retained. No row is changed twice by one pass.
  - Progress is promised only when the write's other operations do not make rows match
    the filter or change rows it already changed. A write that, each pass, upserts more
    than the bound of rows the filter admits can re-change the same rows forever; that
    write is the client's to split. Under that condition each pass changes
    min(bound, what remains) rows.
  - On `outcome_unavailable` or `idempotency_partial`, whether rows remain is unknown; the
    answer is to send it again (with a new key), which is safe for the same reason.
- Which rows a pass takes, when more than the bound remain, is not specified: the scan
  retains the first it meets, and segments are read concurrently. Within what was
  retained and at its place in fold order, the operation takes rows in id order.
- `allow_partial` needs `durability: "applied"`, since only an applied answer can say
  `rows_remaining`; otherwise the write is refused with `bad_request`. Set without its
  by-filter operation, it is refused with `bad_request` too.
- It needs a new operator level, `PSTORE_APPLIED_WRITES=4`, set once every node runs
  M137; below it the write is refused with `unsupported_durability`, as M130's and M132's
  levels refuse theirs.
- Under an idempotency key a resend is a replay, carrying the winner's `rows_remaining`
  (summed as the counts are), so each pass needs its own key; the docs say so.

How it is carried:
- The operation carries `$partial: true`, a reserved attribute. A fold that predates it
  ignores it (`$` attributes are never merged into rows), so a rolled-back node applies
  the operation as M136 does.
- For a partial patch, the scan counts and retains only the rows the patch would change:
  `merged(row, patch)` differs from the row by `same_row`, the test `by_filter` already
  applies in fold order. (`same_row` ignores the `$text` stamp the fold adds to its base
  after the scan, so the scan's rows need no stamp.) A partial delete counts what it
  admits, as M136.
- Each commit's outcome object gains a fourth section, after the keys section: the
  requests with rows remaining. It is written only when one has, and forces the ids and
  keys sections, empty if need be. An M136 decoder refuses it, so `allow_partial` needs
  every node at M137, as M132's keys needed M132.
- `rows_remaining` in the summed outcome is true when any commit recorded it for the
  request. It is a new field of `Outcome`.

Unchanged: operations without `allow_partial`, M136's refusal among them; the order of
operations; HEAD and bundle formats.

## Acceptance criteria
1. With bound 3 and 7 rows a partial `delete_by_filter` admits, three applied writes in a
   row answer `deleted` 3, 3, 1 and `rows_remaining` true, true, absent; afterwards none
   of the 7 rows exist and every other row does.
2. With bound 3 and 7 rows a partial `patch_by_filter` admits, of which 2 already hold
   the patched value: two writes answer `patched` 3, 2 with `rows_remaining` true, then
   absent; afterwards every admitted row holds the value. Without counting only rows it
   would change, the passes could stall on already-patched rows.
3. A partial operation admitting exactly the bound answers as a plain one does, with
   `rows_remaining` absent; a plain one past the bound is still refused (M136's tests
   pass).
4. Fold order counts: with bound 3, an index whose filter admits 3 rows, and an earlier
   `patch_by_filter` in the same write making a fourth match, a partial
   `delete_by_filter` deletes 3 and answers `rows_remaining` true.
5. Fold order takes only rows that would change: with bound 3, 4 admitted rows of which
   one already holds the patched value and is named by a per-id patch of another
   attribute in the same write, a partial `patch_by_filter` patches the other 3 and
   answers `rows_remaining` absent.
6. The outcome object round-trips the remaining section, and a body with it and no ids
   or keys entries decodes; a body without it decodes as before (`applied` unit tests).
7. The server: `allow_partial` without `durability: "applied"`, or without its by-filter
   operation, is refused with `bad_request`; below `PSTORE_APPLIED_WRITES=4` with
   `unsupported_durability`; with all three, the response's `applied.rows_remaining` is
   `true` while rows remain, and absent after.
8. Every existing applied, idempotency, patch and by-filter test passes, the only edit
   being the new `rows_remaining: false` in literal `Outcome` values.

## Test plan
Engine: `crates/pstore-engine/tests/it/filter_rows.rs`, with `with_filter_rows(3)`.
Unit: `applied.rs`'s encode/decode tests. Server: a new
`crates/pstore-server/tests/it/allow_partial.rs`. Red before: 1, 2, 4, 5, 6 and 7 (no
partial operation, no section, no field); 3 is a guard. Mutations, each to be killed:
- a partial operation is refused past the bound like a plain one (1);
- `rows_remaining` never set (1), or set when the bound is reached rather than passed (3);
- the fold-order count does not set `rows_remaining` (4);
- a partial patch counts admissions, not changes, in the scan (2) or in fold order (5);
- the remaining section is not written, or decoded as keys (6);
- the server drops the flag, accepts it without `applied`, or below level 4 (7).

## RA budget
No request is added: a pass is one ordinary write and one fold. The outcome object grows
by one entry per request with rows remaining. No LIST, nothing scaling with records.
Memory as M136: at most bound + 1 retained rows per operation.

## Risks
- A partial patch's change test runs per scanned row the filter admits, which clones it
  to stamp it: CPU per admitted row, as `by_filter` already spends in fold order.
- A partial patch whose merged rows the width pass sets aside (M61) counts them
  `rejected`, does not set `rows_remaining` for them, and takes them again next pass. A
  pass can then change nothing yet answer `rows_remaining`; the docs say to stop on a pass
  that changes nothing and rejects rows (code review).
- Concurrent writes that keep making rows match can keep `rows_remaining` true; each pass
  still changes rows.
- Rolling a node back to M136 while an outcome with the remaining section is listed makes
  that node fail to decode it: its waiters, and **its whole fold** whenever the fold holds
  a keyed attempt, since `decide_keys` reads every listed outcome. That lasts until the
  object falls off (32 tallying commits). Level 4 is what keeps the section from being
  written before every node reads it; `docs/api-migrations.md` says to lower the level and
  wait 32 commits before rolling back.

## Tasks
- M137.1: engine: `$partial`, the change test in the scan, partial application and
  `rows_remaining` in the tally, the outcome section, the public `*_partial` calls.
- M137.2: server: the two request fields, their refusals, `applied.rows_remaining`, docs.

## Review
Spec review round 1 (blocking on two majors, amended): `allow_partial` now has an
operator level (4) and the rollback risk names the fold, not only waiters; and
`rows_remaining` is defined by both counts, with the progress claim narrowed to writes
whose other operations do not make rows match, and a fold-order criterion (4). Minors
taken: the `$text` stamp dropped (`same_row` ignores it); exactly the bound in
criterion 3; fold order takes only rows that would change (criterion 5); replay, unknown
outcomes and the without-operation refusal stated; the flag is per request.

Code review round 1: pass with minors. A width set-aside can make a pass change nothing
and still answer `rows_remaining`; stated in Risks and the migration notes. Not tested:
summing `rows_remaining` over two commits of one request (`|=` against `=`), which one
partial operation per request never needs.
