# M191: folded multi-vector fields in results and exports

## Serves
Workstream D/H of [the plan](../../comparisons/turbopuffer-next-plan.md): "return requested
stored vector fields ... so an HTTP export can reconstruct the index". This milestone
covers the layout that [M190](../M190/SPEC.md) refused and called M190.2: a named field a
folded segment stores at a varying count of vectors a row.

## Owner's decision
2026-10-30: Huy admitted **one round past D-34** for this projection, as an opt-in like
`rerank: exact`. The thread's decision card asked "Let projecting multi-vector fields cost
one round past the D-34 budget?" and the answer was "Yes, as an opt-in like rerank".
AGENTS.md's budget line names the new opt-in.

## Delta
Today a query that projects such a field fails `422 bad_query` naming the field. That
covers every folded multi-vector field, and a single-vector field that some row of the
segment lacks. After this milestone the query answers the field, as M190 answers the
fixed layout.
- **Reads.** A varying field's rows are read in two steps: their offset pairs (16 bytes a
  row, `field_rows`'s first read, split out of it as `field_spans`), then the vectors'
  ranges.
  - **Ranked query:** the offsets are read in the row fetch's round, joined with `rows_at`,
    `vector_rows` and the fixed fields' reads, for every row that round fetches (with
    something unfolded, every leg's candidates: 16 bytes each). The vectors are read in one
    round more, **for the resolved hits only**: one fan-out over the segments holding them.
  - **Order:** the offsets in the vector round, for the page's rows; the vectors in one
    round more. 5 rounds in all.
  - The extra round is taken only when some hit (or page row) has a non-empty span of a
    varying field: a query naming only fixed-layout fields keeps M190's depth, and so does
    one whose varying rows hold no vectors of it.
  - A field may be fixed in one segment and varying in another: each segment reads it as
    its layout says, the fixed one in the row round.
  - A row with no vectors of a varying field (an empty span) has no entry, never `[]`.
- **Bound.** The vectors' bytes are known once their offsets are read: the sum of the hits'
  (or page rows') spans as stored, before coalescing, over every segment and varying field
  (under euclidean each vector is `(dims + 1) × 4` bytes). A query whose sum exceeds the
  bound fails `422 bad_query` naming the bytes and the bound, from a new
  `QueryError::ProjectionTooLarge` mapped as M190's refusal was. The check runs between the
  two rounds, so a refused query reads no vector bytes. The bound reaches `pstore-query` on
  `Projection` (`max_bytes`); the server sets it to `MAX_PROJECTED_BYTES`, 64 MiB, and a
  test lowers it through `Api::set_projection_bytes_for_test`, a hidden method as
  `set_index_params_for_test` is. One row always fits: 9 fields of 1,024 vectors of width
  1,537 is about 57 MiB, so an export can always page one row at a time.
- **Answer.** Unchanged from M190: `vectors: {name: [v, ...]}`, each vector in client space
  in the order written. Folded rows now answer alike whatever their segment's layout.
- **M190's refusal goes.** `QueryError::VaryingField` and its "later (M190.2)" message are
  removed; nothing fails for a layout any more.
- **Unchanged:** `vector`, fixed-layout fields, unfolded rows, and every refusal M179 and
  M190 make.

## Acceptance criteria
1. Over 300 rows with `vector` (width 8), `title` (width 4, one vector) and `tokens` (width
   4, 1 to 5 vectors), under each of the three metrics, folded in two segments -- the first
   with `title` on every row (fixed there), the second with `title` absent from every fifth
   row (varying there) -- with 50 more unfolded, and restarted: a ranked query with
   `include_vectors: ["vector", "title", "tokens"]` answers each row's `title` and
   `tokens`, in order. The vectors are bit for bit as written (dot product, euclidean), or
   each one's f32 normalization within 1e-6 per component (cosine). A row without `title`
   has no `title` entry, ranked and in an order. `float` and `base64` decode to the same
   bits.
2. A `rank_by: ["id", "asc"]` export of all three fields over that index, paged 64 rows at
   a time, is written into a new index of the same metric. On the new index, 20 exact
   `vector` queries, 20 dense queries over `title` and 20 exact `multi` queries over
   `tokens` answer the same ids in the same order as on the original. Each query's
   `$dist` or score is bit for bit equal (dot product, euclidean), or within 1e-5 (cosine),
   and every attribute is equal.
3. With the bound lowered to 1,000 bytes, a ranked query and an order whose hits' `tokens`
   cover more fail `422` naming the bytes and the bound, and each is exactly one round
   shallower than when admitted: no vector range is read. With the bound at 10,000 bytes,
   the same queries over 5 rows are admitted. With something unfolded and a bound the
   page fits but the candidates would not, a ranked query is admitted: the vectors are
   read for its hits only. M190's
   `a_field_folded_at_a_varying_count_fails_naming_the_rule` now asserts the field is
   answered, because this milestone changes that behaviour.
4. On the depth-counting store, cold, at 1 and at 4 folded segments, with every row holding
   `tokens` at a varying count:
   - a ranked query naming `tokens` is exactly one round deeper than one naming `vector`;
   - a `rank_by` order naming it is exactly 5 deep;
   - one naming `title` alone, where every row holds `title`, keeps M190's pins;
   - the request counts are pinned in the test.

## Test plan
Server tests in `crates/pstore-server/tests/it/vectors_multi.rs`. Red first: on the parent
a folded `tokens` fails 422, so criteria 1, 2 and 4 fail, and criterion 3's bound does not
exist.

Mutations, each to be killed:
- The varying vectors read for the wrong row, or with an offset off by one (1, 2).
- An empty span answered as `[]`, ranked or ordered (1).
- A fixed segment's field read in the extra round, or dropped when another segment varies
  (1, 4).
- The vectors read for every candidate rather than the hits (3).
- The offsets' round awaited after `rows_at` rather than joined with it (4).
- The vectors' round run per segment one after another (4).
- The bound checked after the vectors are read, or `>` read as `>=`, or the bound removed
  (3).
- The bytes summed over one segment or one field only (3).
- The extra round taken when no varying field is projected (4, M190's pins).

## RA budget
- Without a varying field: unchanged.
- With one: one more round. Its requests are a coalesced run or more per varying field per
  segment holding a hit (`get_ranges` merges near ranges), beside the offsets' runs in the
  existing round. Bytes are bounded by `MAX_PROJECTED_BYTES`.
- This is the opt-in outside D-34 that the owner admitted.

## Risks
- 64 MiB is a response of about 85 MiB as `base64` and several times that as JSON numbers.
  The bound is generous for exports by design, and a client pages by `offset`.
- A single-vector field that some rows lack now costs the extra round. A format change could
  keep such fields fixed (a presence section), and it is not taken here.

## Tasks
- M191.1: `pstore-format`'s split of `field_rows` into its offsets read and its vectors
  read; the offsets in `fetch_rows` and `vectors_at`, the vectors for the hits (page) with
  the bound between; `ProjectionTooLarge` replacing `VaryingField`; the server's bound and
  its test hook; AGENTS.md's budget line; the test module; docs and VERIFIED.md.

## Review
Spec review round 1 (approve with changes, three majors, all taken): the vectors read for
the hits only, not every candidate; a field fixed in one segment and varying in another;
the bound carried on `Projection` with a named error and a hidden test hook. Minors taken:
coalesced request counts, the bytes counted as stored, an empty span no entry (order path
too), an empty extra round, one row always fits, VERIFIED.md and the removed refusal.
