# M77 — A dense query over a named field

**Serves:** backlog row 65, its first half: *a dense leg over a named field would search
`vector`'s clustering (`VecIndex::search_field`); the API refuses it.*

## Why it was refused, and what is built instead

`VecIndex::search_field` checks that the field exists and then searches **`vector`'s** vectors and
centroids: pointed at a named field it would answer with another field's rows, silently. So the
leg is not routed through it. A dense leg over a named field is an **exact scan of that field's
own vectors** (`Segment::field_vectors`), scored by dot product. Every dense vector, named fields
included, is stored as the index metric's transform and the query is transformed the same way,
so the dot product is the metric's score and `$dist` is right. It reads the field whole, as the
`multi` leg does; **an approximate index per named field is not built** (that is the second half
of row 65, and a per-field clustering has the one-row-order problem M71 solved for the encoding).

## Delta

- `query`'s dense leg: a field other than `vector` is scanned exactly. A segment without the field
  answers nothing; a field of several vectors a document is still refused (`multi`'s job).
- The API's `field` accepts a named field (the same name rules as `multi`'s: no `$fde` suffix),
  with `vector`, beside `exact`.

## Acceptance criteria

1. `a_dense_leg_over_a_named_field_scans_that_field` (`pstore-query`): two fields with different
   vectors in one segment; a dense query over each returns that field's exact top 10 by dot
   product, **not** `vector`'s; a segment lacking the field answers nothing; a width mismatch is
   an error naming both widths; a multi-vector field is refused naming `multi`.
2. `the_api_searches_a_named_field` (`pstore-server`): after writes, a fold and a compaction,
   `{"vector": q, "field": "tag"}` ranks by the named field and not by `vector`; `field: "vector"`
   is unchanged; a `$fde` field name and a field no document carries are `400`.
3. `a_named_field_follows_the_index_metric`: under a cosine index the named-field scores equal the
   cosine of the stored vectors.
4. **Gates:** `./scripts/gates.sh` passes; the mutation sweep of the changed lines misses 0, or
   each miss is killed by a named test.

**Not covered:** an approximate (clustered) index per named field; splitting the leg across
servers (M54's parts carry `vector` only).
