# M121 — a correctness and cost round over M119–M120

**Serves:** the cost rules in [AGENTS.md](../../../AGENTS.md) (requests scale with nodes and
bytes; depth is budgeted; concurrent misses collapse), and the findings of a second review of
M119–M120's changes, by crate. No backlog row. Every finding and its outcome: [FINDINGS.md](FINDINGS.md).

## What is true today

- **A scan of a segment that is not one fixed-width field reads the first field's vectors
  twice, in two rounds.** `Segment::scan` asks for each block's slice of `Vectors` with the
  blocks, throws them away, then reads every field whole -- the first field's span is
  `Vectors` -- after the blocks' round returns. Compaction and a fold read this way.
- **A pin check is two billed HEADs.** `head` (the length) and `get_tag` (the ETag) are each a
  HEAD on the object-store backend, for one object's metadata. An external column checks
  every one of its objects before every read, so a Lance column of `D` data files pays `2D`.
- **The cache copies every range it misses**, then drops the copies of a `Scan` read and of a
  range past its arena's quota, neither of which it admits: a compaction's whole segment
  copied once more, for nothing.
- **Batched misses do not collapse.** `fetch_once` holds concurrent misses of one range to one
  request; `get_ranges` -- how blocks, postings and vector rows are read -- sends each
  concurrent caller's misses to the store.
- **`rows_at` decodes every row of a block it reads** to return the few asked; a block grows
  past 64 rows when the block index would not fit its budget.

## Delta

1. `scan` asks for the per-block vector slices only when it decodes them, and reads every
   field's vectors in the same round as the blocks.
2. `BlobStore::stat` answers length and tag together: one HEAD on the object-store backend,
   by default the two calls it replaces. Every decorator on a production path forwards it.
   The external pin checks use it.
3. The cache copies a missed range only when it admits it.
4. `get_ranges` claims each miss as `fetch_once` does, fetches what it claimed, admits and
   releases, then waits on the claims others hold. A claim's holder that fails leaves its
   waiter to fetch the range itself.
5. `rows_at` builds only the rows asked; the others are skipped by `skip_value`.

**Not changed:** any answer, any error a caller sees, any wire format; a refusal still
refuses on any object of a column changing (`a_changed_fragment_is_noticed`).

## Acceptance criteria

1. **Scan.** `a_multi_field_scan_reads_each_fields_vectors_once`: every field's `Vectors` span
   is read exactly once. `a_multi_field_scan_reads_its_fields_together`: 1 round, not 2.
2. **One HEAD.** `a_stat_is_one_head`: the object-store backend's `stat` sends one HEAD.
   `a_column_check_is_one_request_an_object`: an external read of a three-object column sends
   three checks, not six.
3. **Copies.** `a_scan_is_served_from_bulk_and_admits_nothing` and `a_cached_range_owns_its_bytes` still pass; the copy is moved inside the admit,
   after its class and quota are checked, and is visible to no test -- named as such.
4. **Collapse.** `concurrent_batched_misses_issue_one_request_a_range`: eight concurrent
   `get_ranges` of the same ranges reach the store as one call alone does.
5. **Rows.** `a_row_asked_twice_comes_back_twice` and the `rows_at` suite pass, and
   `rows_at_builds_only_the_rows_asked` agrees with the full decode on every row of every
   block, for random row sets.
6. M119's bench: every digest unchanged.
7. `./scripts/gates.sh` passes.

## Test plan

| # | Test | Fails first because | Mutation it catches |
|---|---|---|---|
| 1 | `a_multi_field_scan_reads_each_fields_vectors_once`, `..._together` | 480 bytes of 240; 2 rounds | the slices asked again |
| 2 | `a_stat_is_one_head`, `a_column_check_is_one_request_an_object` | 2 HEADs; 6 requests | `stat` left to the default |
| 4 | `concurrent_batched_misses_issue_one_request_a_range` | 8 calls reach the store | no claim taken |
| 5 | `rows_at_builds_only_the_rows_asked` | it does not exist | a skipped row's bytes not consumed |

## RA budget

Requests: an external read `2 × objects` → `objects`; a multi-field scan's vectors of field
0 once, not twice. Depth: a multi-field scan `2` → `1`. A batched miss another call is
fetching costs no request. No LIST.

## Risks

- **A waiter whose holder fails fetches alone**: a second round, on a path that failed.
- **A decorator that does not forward `stat`** falls back to two calls: correct, and the
  cost it had.

## Tasks

- **M121.1** — all of it, and the ledger.
