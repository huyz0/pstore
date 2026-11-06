# M193: sparse vectors in results and exports

## Serves
Workstream D/H of [the plan](../../comparisons/turbopuffer-next-plan.md): "return requested
stored vector fields ... so an HTTP export can reconstruct the index". [M190](../M190/SPEC.md)
refuses `sparse` in `include_vectors` as later; this milestone answers it. With
[M191](../M191/SPEC.md) every stored vector field is then projected.

## Why a new section
A segment stores a sparse field only as postings, an inverted index by key
([M177](../M177/SPEC.md)). One row's vector is spread over every key's list, so rebuilding it
reads the whole field and its dictionary: bytes in proportion to the segment, not to the
rows asked. A fold therefore also writes each row's pairs, forward, in a new section, and a
projection reads only the rows it answers. This is the layout of M191's varying dense
field, so it costs the same one round as M191's opt-in. A query projecting `sparse` alone
takes that round too, so it is an opt-in outside D-34 of its own: the owner chose it on
2026-11-04, asked "How should results return sparse vectors?" and answering "Per-row
sparse section, +1 round" over rebuilding rows from the postings.

## Delta
- **Format.** `Section::SparseRows` (17): an offsets table of `rows + 1` `u64`s indexed by
  **data** row (the row space hits resolve to, never `IndexRows`' index rows), then each
  row's pairs in key byte order, each a `u16` key length, the key's bytes and its weight as
  the stored F16's two bytes, the value the postings hold, so the bits are exact. A fold
  writes it for every segment with a sparse field, built by `sparse::build` beside the
  postings and attached the same way; a compaction, which folds its inputs' documents,
  writes it too. Additive: a reader before M193 skips it, and postings, scoring and `scan`
  do not change.
- **Request.** `include_vectors` admits `"sparse"`, once, with or without other names. It is
  still not a named field: `vectors.sparse` stays refused, as M178 has it. It raises the
  cap on names by one, to `2 + MAX_NAMED_FIELDS` (`vector`, `sparse` and 8 named fields);
  one more is `400` as today.
- **Answer.** A row holding the field gains `sparse: {key: weight}` beside `id`, spelled as a
  write's `sparse` is: keys in byte order, each weight a JSON number (a signed zero as
  `-0.0`). `vector_encoding` does not apply to it. A row without the field has no `sparse`
  key, never `{}`.
- **Reads.** As M191's varying fields, in the same rounds: the offsets in the row fetch's
  round (16 bytes a row, joined with `rows_at`), the pairs one round later for the resolved
  hits (or an order's page) only, in the same fan-out as M191's vectors. A query naming
  `sparse` and a varying dense field takes one round more, not two. An order naming it is
  5 rounds. Unfolded rows answer from memory.
- **Bound.** The pairs' bytes, as stored, count toward M191's `MAX_PROJECTED_BYTES` with the
  varying dense fields' bytes, checked before that round: past it, `422 bad_query` naming
  the bytes and the bound, as M191 refuses.
- **Segments folded before M193** have no `SparseRows`. A query projecting `sparse` whose
  hits (or page) include any row of a segment that has a sparse field (a `Fields` row of
  kind 1) and no `SparseRows` fails `422 bad_query`, naming the field and that the segment
  predates the section until a compaction or a reindex rewrites it. Whether that row holds
  the field is not asked: only the postings say, and they are never read for this. A
  segment with no sparse field answers its rows without `sparse`.
- **Unchanged:** sparse queries, postings, scores, `scan`, the dictionary, and every other
  projection.

## Acceptance criteria
1. Over 300 rows with `vector` (width 8), `sparse` (0 to 40 keys of 1 to 24 bytes drawn
   from 2,000, weights including zero and negatives) and `tokens` (width 4, 1 to 5
   vectors), folded in two segments, with 50 more unfolded, and restarted: a ranked query
   with `include_vectors: ["vector", "tokens", "sparse"]` answers each row's `sparse` as an
   object equal, key for key and bit for bit, to its written weights rounded to f16. A row
   without the field has no `sparse` key. Float and base64 answer the same `sparse`.
2. A `rank_by: ["id", "asc"]` export of `vector` and `sparse`, paged 64 rows at a time,
   written into a new index: 20 sparse queries and 20 exact `vector` queries answer
   bit-for-bit equal scores, in order, as the original, and the same ids within each score
   (a tie breaks by segment and row, which the new index folds differently). Every row's
   exported `sparse` equals the original's. A sparse-only index (M170, no `vector`) of 20
   rows exports its `sparse` the same way.
3. Refusals and the bound:
   - with the bound lowered so the hits' pairs and `tokens` together exceed it but either
     alone fits, the query fails `422` naming the bytes, and is one round shallower than
     admitted;
   - at the exact sum it is admitted;
   - over a segment folded without `SparseRows` (a hidden engine hook that omits it), a
     query projecting `sparse` whose hits include a row holding the field, and one whose
     only such hit holds none, both fail `422` naming the field and the rule; one not
     projecting it answers as before; after that segment is compacted with a new one by a
     build with the section, the same query answers every row's pairs;
   - `["sparse", "sparse"]` is `400`, as a name twice is. M190's
     `names_that_cannot_be_projected_are_refused_naming_the_rule` drops its `["sparse"]`
     case, because this milestone admits it.
4. On the depth-counting store, cold, at 1 and at 4 folded segments: a ranked query naming
   `sparse` is exactly one round deeper than one naming `vector`; naming `sparse` and
   `tokens` together is that same depth; a `rank_by` order naming `sparse` is exactly 5
   deep; the request counts are pinned in the test. A sparse query not projecting it keeps
   M178's pins.

## Test plan
Server tests in `crates/pstore-server/tests/it/vectors_sparse.rs`, and format tests of the
section in `crates/pstore-format/tests/it/`. Red first: on the parent `"sparse"` is `400`,
so every criterion fails.

Mutations, each to be killed:
- a row's pairs read for another row, or an offset off by one (1, 2);
- a weight written before rounding, or decoded as the wrong width (1, 2);
- keys out of byte order, or a key's length off by one (1);
- an empty row answered as `{}` (1);
- the pairs read for every candidate rather than the hits (3);
- the pairs' bytes left out of the bound, or `>` read as `>=` (3);
- a segment without the section answered as no field rather than refused, or refused only
  for rows its postings name (3);
- a compaction dropping the section (3);
- the pairs' round run apart from M191's, or the offsets awaited after `rows_at` (4).

## RA budget
- Without `sparse` projected: unchanged.
- With it: the M191 round, shared. Its requests are a coalesced run or more per segment
  holding a hit, beside the offsets' runs in the existing round. The offsets are read for
  every row the row fetch reads, as M191's are (the candidates, where an unfolded write
  makes the fetch read them before fusion); only the hits' pairs are read and bounded.
- A fold writes `8 × (rows + 1)` bytes of offsets plus `key + 4` bytes a pair, beside
  postings that hold a row id and an F16 a pair, and their keys once in the dictionary. A
  long key costs more here than in the postings; this is the price of a projection that
  does not read the field. The fold buffers the section while writing, about as much as
  the postings, uncounted as the postings are. One directory entry more in the meta region
  (12 bytes) is the width it adds.

## Risks
- Every segment folded before M193 refuses projection of `sparse` until it is rewritten.
  Exports of an old index need a reindex first, and the message says so.
- During a rolling deploy a build before M193 compacts without the section, and a later
  projection over that segment refuses again until the next rewrite. A branch reading its
  parent's old segments refuses the same way.
- A pair stores its key in full, so a vocabulary of long keys costs more than the postings.

## Tasks
- M193.1: `SparseRows` in the format and the fold; the offsets and pairs reads, the bound and
  the refusal; the request, the answer and the hidden hook; the tests; docs and VERIFIED.md.

## Review
Spec review round 1 (approve with changes, three majors, all taken): the old-segment refusal
stated from what the footer knows (a sparse field and no section), with a hit holding none
refused too; a compaction writing the section, checked; the export compared within tied
scores. Minors taken: the offsets' bytes, the data row space, the name cap raised by one,
the stored F16 kept as its bits, a signed zero, the meta region's width, a sparse-only index,
rolling deploys and branches. Not taken: a checked-in pre-M193 segment fixture; the hidden
hook writes the same bytes without the section, which is what such a build wrote. The
replicated-segment case is not tested: replication is off by default (`replicas: 0`).

Amended while the tests were written:
- M179's `vectors_out` refused `["sparse"]` too; both its and M190's case now ask
  `["sparse", "sparse"]`, refused as a name twice.
- The name cap is counted as M190 counts it over the dense names (`vector` and named
  fields, at most `1 + MAX_NAMED_FIELDS`), with `sparse` admitted once beside them: the
  same `2 + MAX_NAMED_FIELDS` in all, and ten dense names still `400`.
- An unfolded row answers its pairs from memory as a fold would store them: weights rounded
  to f16 and keys in byte order, so a row answers the same before and after its fold.
- The hidden hook is `Params::sparse_rows` (the index crate), set by
  `Engine::with_sparse_rows` and the server's `set_sparse_rows_for_test`.
- Format tests are `crates/pstore-format/tests/it/sparse_rows.rs`.
