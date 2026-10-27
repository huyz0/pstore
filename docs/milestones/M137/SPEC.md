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
  rows it would change were left.
- **Resume is sending the same write again.** A row already deleted no longer matches; a
  row already patched to the same values would not change, so it is not counted and not
  retained. Each pass changes min(bound, what remains) rows, so a client repeating until
  `rows_remaining` is absent makes progress every pass, and no row is changed twice by
  one pass.
- Which rows a pass takes, when more than the bound remain, is not specified: the scan
  retains the first it meets, and segments are read concurrently. Within what was
  retained and at its place in fold order, the operation takes rows in id order.
- `allow_partial` needs `durability: "applied"`, since only an applied answer can say
  `rows_remaining`; otherwise the write is refused with `bad_request`. Under an
  idempotency key a resend is a replay, so each pass needs its own key; the docs say so.

How it is carried:
- The operation carries `$partial: true`, a reserved attribute. A fold that predates it
  ignores it (`$` attributes are never merged into rows), so a rolled-back node applies
  the operation as M136 does.
- For a partial patch, the scan counts and retains only the rows the patch would change:
  the row, stamped with the index's text field as the fold stamps its base, merged with the
  patch, differs from it. A partial delete counts what it admits, as M136.
- Each commit's outcome object gains a fourth section, after the keys section: the
  requests with rows remaining. It is written only when one has, and forces the ids and
  keys sections, empty if need be. An M136 decoder refuses it, so `allow_partial` needs
  every node at M137, as M132's keys needed M132.
- `rows_remaining` in the summed outcome is true when any commit recorded it for the
  request.

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
3. A partial operation within the bound answers exactly as a plain one does, with
   `rows_remaining` absent; a plain one past the bound is still refused (M136's tests
   pass unchanged).
4. The outcome object round-trips the remaining section, and a body with it and no ids
   or keys entries decodes; a body without it decodes as before (`applied` unit tests).
5. The server: `allow_partial` without `durability: "applied"` is refused with
   `bad_request`; `allow_partial` without its by-filter operation is refused; with both,
   the response's `applied.rows_remaining` is `true` while rows remain, and absent after.
6. Every existing applied, idempotency, patch and by-filter test passes unchanged.

## Test plan
Engine: `crates/pstore-engine/tests/it/filter_rows.rs`, with `with_filter_rows(3)`.
Unit: `applied.rs`'s encode/decode tests. Server: a new
`crates/pstore-server/tests/it/allow_partial.rs`. Red before: 1, 2, 4 and 5 (no partial
operation, no section, no field); 3 is a guard. Mutations, each to be killed:
- a partial operation is refused past the bound like a plain one (1);
- `rows_remaining` never set, or set whenever the bound is reached rather than passed (1);
- a partial patch counts admissions, not changes (2);
- the base row is not stamped before the change test (a patch of a `body`-field index;
  covered in 2 if the test index declares a text field, else stated as not run);
- the remaining section is not written, or decoded as keys (4);
- the server drops the flag, or accepts it without `applied` (5).

## RA budget
No request is added: a pass is one ordinary write and one fold. The outcome object grows
by one entry per request with rows remaining. No LIST, nothing scaling with records.
Memory as M136: at most bound + 1 retained rows per operation.

## Risks
- A partial patch's change test runs per scanned row the filter admits, which clones it
  to stamp it: CPU per admitted row, as `by_filter` already spends in fold order.
- Concurrent writes that keep making rows match can keep `rows_remaining` true; each pass
  still changes rows.
- Rolling a node back to M136 while a partial write's outcome is listed makes that node's
  waiters fail to decode it until it falls off (32 tallying commits). Stated in
  `docs/api-migrations.md`.

## Tasks
- M137.1: engine: `$partial`, the change test in the scan, partial application and
  `rows_remaining` in the tally, the outcome section, the public `*_partial` calls.
- M137.2: server: the two request fields, their refusals, `applied.rows_remaining`, docs.
