# M190: named vector fields in results and exports

## Serves
Workstream D/H of [the plan](../../comparisons/turbopuffer-next-plan.md): "return requested
stored vector fields in the selected encoding so an HTTP export can reconstruct the index",
for the named dense fields (M77) and multi-vector fields (M61) that
[M179](../M179/SPEC.md) left unprojected; the `Returned vectors` row of
[the comparison](../../comparisons/turbopuffer-2026-10-25.md).

## Delta
M179 projects `vector` only and refuses any other name. After, `include_vectors` also names
a document's named fields, and each row answers them as a write spells them.
- **Request**: `include_vectors` lists `vector` and named fields in any combination
  (`["title"]` without `vector` is admitted), each at most once, at most
  `1 + MAX_NAMED_FIELDS` (9) names: a per-query bound, the most one document holds; an index
  whose documents hold more fields between them is exported by two queries. `vector` is
  accepted by name; every other name passes M61's `named_field` (empty, over 64 bytes, a
  NUL, or the derived-encoding suffix is 400 naming the rule), and `sparse` is 400 (its
  projection is later). A name listed twice, `[]` and a non-list are 400. A well-formed name
  the index holds no field of is admitted and answers nothing, as a row without the field
  does: knowing every field would take a read the query does not make.
- `vector_encoding` spells every projected vector, as in M179.
- **Response**: a row holding any projected named field gains `"vectors": {name: [v, ...]}`
  beside `id` and `vector`: the field's vectors in the order written, each spelled as
  `vector` is. A row holding none has no `vectors` key, never `{}`; a field the row does not
  hold has no entry, never `[]`; no name ending the derived-encoding suffix ever appears,
  though unfolded rows carry those fields in memory. Folded and unfolded rows answer alike;
  with `diversify`, the picked rows answer theirs. The `vector` key is M179's.
- **Value**: each vector in client space (M64's `client_space`), as M179's `vector`: the
  write transforms a named field's vectors by the index's metric as it does `vector`'s
  (`transform_stored`), so dot product and euclidean answer the f32s written, cosine their
  normalization. Writing a page back into an index of the same metric reproduces every
  exact dense and `multi` leg's distances.
- **Refused**, as M179 refuses `vector`: beside `aggregate_by` or `as_of`, inside a
  multi-query's `queries`, and above `MAX_VECTOR_ROWS` rows.
- **Layout.** A folded segment stores a field at one vector a row (`per_row` 1) only when
  **every** row of the segment holds exactly one vector of it (the writer has no other
  fixed count); one row without the field, or with two, stores it at a varying count
  (`per_row` 0), whose rows' offsets must be read before their vectors. So a folded
  multi-vector field is always of the second layout. This milestone projects the first,
  and every unfolded row's fields, multi-vector ones included, from memory. A query whose
  fetched rows lie in a segment storing a projected field at a varying count fails
  `422 bad_query` naming the rule, never answering nothing (with something unfolded a
  fetched row is any leg's candidate, so one never returned can fail it): M190.2 reads
  that layout, with one more round, if the owner admits that round outside D-34 (asked
  2026-10-30).
- **Cost**: the named fields' rows are read in the round that reads `vector`'s (M179), one
  `field_rows` call a projected field a segment, joined with `rows_at` and `vector_rows`,
  never awaited after them: a ranked query's row fetch (every `fetch_rows` caller: the
  shadow check, the `whole` path and `resolve_rows`), and an order's vector round. The
  depth is M179's. In the ranked path the fetched rows are the row fetch's, which with
  something unfolded are every leg's candidates (M64's `legs × window`), not only the page.
  A query naming no named field makes exactly M179's requests.

## Acceptance criteria
1. Over 300 rows, each with `vector` of width 8 and a named `title` of width 4 (one vector),
   folded in two segments, and 50 more unfolded that also hold a named `tokens` of width 4
   and 1 to 5 vectors, under each of the three metrics, and restarted: a ranked query
   with `include_vectors: ["vector", "title", "tokens"]` answers each row's `title` and
   `tokens` vectors, in order, bit for bit as written (dot product, euclidean) or as each
   one's f32 normalization within 1e-6 per component (cosine); `float` and `base64` decode
   to the same bits.
2. A `rank_by: ["id", "asc"]` export of all three fields, paged 64 rows at a time, written
   into a new index of the same metric: 20 exact `vector` queries, 20 dense queries over
   `title` and 20 exact `multi` queries over `tokens` answer the same ids in the same order,
   `$dist` bit for bit (dot product, euclidean) or within 1e-5 (cosine), and every attribute
   equal. (`fde` legs are approximate and excluded.)
3. Unfolded, a row with `title` alone answers `vectors` with `title` only and a row with no
   named field no `vectors` key; folded, a segment whose rows all lack named fields answers
   no `vectors` key; no response key ends the derived-encoding suffix, folded or unfolded. A
   name
   the index has no field of is admitted and answers no entry. With `diversify`, each picked
   row answers its `title`. A query whose fetched rows include a segment storing `title` at
   a varying count, or a folded `tokens`, fails 422 naming the rule. 400 naming the rule: `[""]`, a 65-byte name, a
   name with a NUL, a name ending the derived-encoding suffix, `["sparse"]`,
   `["title", "title"]`, `[1]`, `[]`, `"title"`, and 10 distinct names; 9 are admitted.
4. On the depth-counting store, cold, at 1 and at 4 folded segments, every row holding
   `title`: a ranked query naming `vector` and `title` is exactly as deep as one naming
   `vector` alone, and a `rank_by` order naming them exactly 4 deep, with exactly one more
   request a segment holding a fetched row than `vector` alone; the counts pinned in the
   test. M179's pinned counts for `vector` alone hold.

## Test plan
Server tests in `crates/pstore-server/tests/it/vectors_named.rs`. Red first: on the parent
any name but `vector` is 400, so 1, 2 and 4 fail and 3's asserted messages do not match.
Mutations, each to be killed: a named field's euclidean extra component kept, or cosine
unnormalized (1); an unfolded multi-vector field's vectors reordered or one dropped (1, 2); a field's
rows read from another field, through `vector_rows`, or for another row (2, 3); an empty
`vectors` object or field list answered (3); the derived field not filtered (3); a varying
layout answering nothing rather than failing (3); `diversify` dropping the picked rows'
named vectors (3); named fields dropped on the shadow-check or `whole` path (1, 3); each
refusal removed and the name bound off by one (3); the named reads awaited after `vector`'s
rather than joined (4).

## RA budget
Unchanged rounds; one more ranged-read request a projected field a segment holding a
fetched row, bounded by the fetched rows, never by the index.

## Risks
- A row holds at most 9 fixed-width vectors here, so the bytes are M179's times the fields
  named: 9 fields of width 1,536 over 1,000 rows is about 55 MB of f32s. M190.2's varying
  layout (up to 1,024 vectors a row) needs its own bound on vectors a response.
- An optional single-vector field folds at a varying count whenever a segment holds a row
  without it, so until M190.2 such a field fails to project. An alternative to M190.2's
  round: the fold records each row's offsets in its row block, keeping an order at depth 4
  (the ranked path would still need the round); or a presence section beside a fixed
  layout for at-most-one fields. Both are format changes, weighed in M190.2.
- Sparse projection needs a row-ordered sparse section at fold (postings are by
  dimension): a later milestone.

## Tasks
- M190.1: the request's names, `fetch_rows` and the order's vector round over fixed-width
  named fields, unfolded rows' fields from memory, the varying layout's refusal, the
  response's `vectors`, the test module registered in `tests/it/main.rs`, docs.

## Review
Spec review round 1 (approve with changes, five majors, all taken): the fixed layout is a
segment's rows' property, stated and tested; the bytes bound stated and the varying layout's
bound moved to M190.2; the derived fields never answered; the width-0 index; the varying
layout split out to M190.2 behind the owner's decision, refused rather than silent here.
Minors taken: `vector` accepted by name, the 9-name cap a per-query bound, every
`fetch_rows` caller, `fde` excluded, per-segment request counts, the format alternative.
While writing the tests: the writer's only fixed count is one, so a folded multi-vector
field is always of the varying layout; criterion 1's `tokens` moved to the unfolded rows.
And a named field needs a `vector` beside it (M61), so no width-0 index holds one: that
case left criterion 3.
