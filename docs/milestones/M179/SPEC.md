# M179: stored vectors in results and exports

## Serves
Workstream D/H of [the plan](../../comparisons/turbopuffer-next-plan.md): "return requested
stored vector fields in the selected encoding so an HTTP export can reconstruct the index",
the `Returned vectors` and `vector_encoding` rows of
[the comparison](../../comparisons/turbopuffer-2026-10-25.md).

## Delta
Today no response carries a stored vector: M64 reads a ranked query's candidate vectors in
the row fetch for `diversify` only, and a `rank_by` order (the export path) returns ids
and attributes. After, a query names the vectors it wants and gets them.
- **Request**: `"include_vectors": ["vector"]` on a ranked query or a `rank_by` order. Only
  `vector`, the index's default dense field, is projected; any other name, an empty list or
  a non-list is 400 naming the rule. An attribute named `vector` is unaffected: it is
  `include_attributes`' to project, under `attributes`, as today.
- `"vector_encoding": "float"` (the default) answers a JSON array of numbers;
  `"base64"` answers the standard-alphabet, padded base64 of the vector's little-endian
  f32 bytes, the spelling a write already accepts. Another value, or `vector_encoding`
  without `include_vectors`, is 400.
- **Response**: each result row gains `"vector"` beside `id` (not under `attributes`); a
  row with no `vector` (M170's width-0 index, folded or unfolded) has none, never `null` or
  `[]`. Folded and unfolded rows answer alike. With `diversify`, the picked rows still answer
  theirs. A folded segment whose `vector` is not its `Vectors` section, unreachable through
  the API (M64, M170), answers no `vector`, as `diversify` reads it today.
- **Value**: the vector as stored, in client space (M64's `client_space`): under
  `dot_product` and `euclidean_squared` exactly the f32s written; under `cosine_distance`
  the written vector normalized, as the index stores it. Writing it back under the same
  metric reproduces the index's distances.
- **Refused** (400 naming the rule): `include_vectors` beside `aggregate_by`, beside
  `as_of`, inside a multi-query's `queries`, and above 1,000 rows (`MAX_VECTOR_ROWS`):
  a ranked query's `top_k + offset`, whose row fetch covers both, or a `rank_by` order's
  `top_k`, which reads only the page, so an export pages by `offset` as far as
  `MAX_ORDERED` (10,000) allows. Each is later, not never.
- **Cost**: a ranked query reads its vectors in the row round it already makes (M64), so
  its depth is unchanged: the row fetch's rows, which with `boost`, `diversify` or
  `limit_per` are the window's, and with something unfolded every leg's candidates (M64's
  `legs × window` bound). A `rank_by` order adds one round after selection: `select` returns
  each kept folded row's target and row number and keeps the segments its first round opened,
  so the round is one fan-out of `vector_rows` over those held segments, never a reopen or a
  lookup by id; unfolded page rows take theirs from memory through `client_space`, no round.
  Four rounds, within D-34 as M47 restates it (three to rank, one for the rows). A query
  without `include_vectors` makes exactly the requests it made before.

## Acceptance criteria
1. Over 300 rows of width 8 under each of the three metrics, folded once with 50 more
   rows unfolded, and restarted: a ranked query with `include_vectors` answers each row's
   vector bit for bit as written (dot product, euclidean) or as the f32 normalization
   `x / sqrt(sum x²)` of it within 1e-6 per component (cosine); `float` and `base64` decode
   to the same f32 bits.
2. A `rank_by: ["id", "asc"]` export with `include_vectors`, paged 64 rows at a time over
   that index, written into a new index of the same metric: 20 exact queries answer the
   same ids in the same order, `$dist` bit for bit (dot product, euclidean) or within 1e-5
   (cosine), and every attribute equal.
3. In a width-0 index, a folded and an unfolded row answer no `vector` key, and so does a
   row of a sparse-only index. With
   `diversify`, each picked row answers its vector. 400 naming the rule: `include_vectors`
   of `["other"]`, `[]`, `"vector"` and `true`; `vector_encoding` of `"f16"` and without
   `include_vectors`; `include_vectors` beside `aggregate_by`, `as_of`, in `queries`; a ranked
   query of `top_k` 1,001, or 1,000 and `offset` 1, and an order of `top_k` 1,001. A ranked
   `top_k` 1,000 and an order of `top_k` 1,000 with `offset` 1,000 are admitted.
4. On the depth-counting store, cold, at 1 and at 4 folded segments: a ranked query with
   `include_vectors` is exactly as deep as without it, with at most one more request a
   segment; a `rank_by` order with it is exactly 4 deep; the request counts pinned in the
   test. The same queries without `include_vectors` make the requests they made on the
   parent, pinned.

## Test plan
Server tests in `crates/pstore-server/tests/it/vectors_out.rs`. Red first: on the parent
`include_vectors` and `vector_encoding` are unknown fields (400), so 1, 2 and 4 fail and
3's asserted messages do not match. Mutations, each to be killed: the euclidean extra
component kept (1); cosine's vector returned unnormalized or another field's (1); base64
of big-endian bytes (1); the order's vectors read for the wrong rows or left out for
unfolded ones (2); a row with no vector answering `null` or `[]` (3); `diversify` taking the
picked rows' vectors (3); each refusal removed, and the row bound off by one (3); the
order's vector round run for a query that did not ask, awaited segment by segment, or
reopening the segments (4).

## RA budget
Ranked: unchanged rounds; the vector ranges coalesce into one more ranged read a segment
at most. Ordered: one more round, its requests one per segment holding a page row, bounded
by the page, never by the index.

## Risks
- At the 1,000-row bound and width 1,536 a page holds about 6 MB of f32s, 8 MB as `base64`
  and two to three times that as JSON numbers; `base64` is the compact spelling. The bound
  keeps a response's vectors to that (a windowed or unfolded ranked query still reads M64's
  `legs × window`), where `MAX_ORDERED` alone would allow ten times it.
- Cosine returns the normalized vector, not the bytes written: an export reconstructs the
  index's distances, not the client's input. Recorded in the migration note.
- Named fields, multi-vector and sparse projection are later milestones of D.

## Tasks
- M179.1: the request fields, the ranked path, the ordered path's vector round, the test
  module registered in `tests/it/main.rs`, docs.

## Review
Spec review round 1 (approve with changes, three majors, all taken): the ordered round
named over `select`'s held segments and rows; depth exact at 1 and 4 segments; a
1,000-row bound on projected vectors. Minors taken: vector-free rows folded and unfolded,
`diversify`'s picked rows, M64's window bound, the unreachable layout, the test module.
Round 2 (approve with changes, one major, taken): the unreachable layout answers no
`vector` as `diversify` does, rather than a 500 that would catch sparse-only and width-0
segments; minors taken: an order's bound is its page, a response's vectors.
