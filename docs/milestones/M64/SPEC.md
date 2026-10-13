# M64 — MMR: a ranked window diversified by its candidates' vectors

**Serves:** backlog row 68, from the project owner's comparison of 2026-10-08. Qdrant's and
Weaviate's (2026, hybrid) maximal marginal relevance re-orders a ranking so near-duplicates do
not crowd the top. pstore returns the fused order only.

## What is true today

Read from the tree at M63 (`d5de2cf`):

- A boost (M62) widens every leg and the engine to a `window`, and the server cuts the window
  to `top_k`.
- **Rows are fetched in one of two places.** With nothing unfolded, `resolve_rows` fetches the
  final hits' rows, in D-34's fourth round. With something unfolded, `run` has already
  fetched the rows of **every leg's candidates** (`fetch_rows`, to check shadowing) before it
  fuses, and `resolve_rows` finds everything known.
- **Rows carry no vectors.** `Segment::rows_at` decodes blocks only. `Segment::vector_rows`
  reads named rows of `Section::Vectors` by ranged reads, coalesced.
- ⚠️ **`Section::Vectors` is the segment's first dense field, not necessarily `vector`.** The
  writer puts `vector` first when it is present (M61). Without it, the first named field
  takes the section, and if that field holds several vectors a row its bytes are an offset
  table and `vector_rows` returns misaligned floats without an error.
- Unfolded rows (`Answer::unfolded`) are whole documents whose vectors were **already
  transformed** by the write (`transform_stored`).
- Stored vectors are the metric's transform (M9d): normalized under cosine, `[v, −‖v‖²/2]`
  under euclidean, raw under dot product.

## Delta

**A ranked query may carry `diversify`. The engine ranks `window` candidates and reads their
`vector` in the same fetch that reads their rows. The server then picks `top_k` of them
greedily by MMR. Depth stays D-34's four.**

1. **The request.** `"diversify": {"lambda": 0.5, "window": 100}`.
   - `lambda`, a number from 0 to 1, is required: 1 is relevance alone, 0 diversity alone.
   - `window` is as a boost's (M62): from `top_k` to 1,000, defaulting to
     `min(1000, max(100, 2·top_k))` -- one function for both. `top_k` above 1,000 is refused.
   - **A window governs both** diversify and boost: if either names one, it is used by both;
     both naming different ones is refused.
2. **Vectors ride the row fetch** (`fetch_rows` gains `with_vectors`, so both places above
   carry them):
   - per segment, `rows_at` and `vector_rows` are joined, never awaited one after the other;
   - a segment's vectors are read only when `field_layout("vector")` is a dense, one-vector-
     a-row field in `Section::Vectors`. Otherwise its rows carry none (a segment whose first
     dense field is another, or whose `vector` holds several vectors a row);
   - each fetched document carries its `vector` under the default field.
   - **With something unfolded,** the vectors of **every leg's candidates** are read, not only
     the final hits': the rows are fetched before fusion, and the final hits are not yet
     known. Bytes are at most `legs × window` rows, and the ledger says so.
   - It is local to the server answering: `shares` run vector legs only, so no peer protocol
     changes.
3. **The engine.**
   - `pstore_query::query_rows_split` gains `with_vectors`; `query_rows_filtered` passes
     `false`.
   - `Engine::query_split_vectors` is `query_split_as` with it set, through one private path
     (the `bounded` retry included).
   - `Answer::vectors` is one `Option<Vec<f32>>` per hit in **client space**, and **empty**
     for every query without it. A hit with no vector, or whose segment's `vector` differs in
     dimension from the first candidate's, is `None`.
   - `Engine::resolve_rows_with_vectors` returns each resolved row beside its vector, so a
     hit whose row vanished cannot misalign them.
   - **Client space** is one function used for fetched and unfolded vectors alike: cosine's
     normalized vector, dot product's raw one, euclidean's stored vector less its extra
     component.
4. **The selection,** in `f64`, over the window's rows in the engine's order (a boost has
   already rescored and sorted them), on a blocking thread (`spawn_blocking`: it is
   `top_k × window × dims` multiply-adds):
   - **Relevance:** `rel(d) = (s(d) − s_min) / (s_max − s_min)` over the window's scores; 1
     for every candidate when `s_max == s_min` exactly, or when either is not finite.
   - **Similarity:** `sim(a, b)` is the cosine of their client-space vectors: 0 when either
     has no vector, a zero norm, or the dimensions differ. It is **not clamped**: a
     negative similarity (opposite vectors) is a bonus, as the cosine says.
     ⚠️ It is the cosine for every metric, *deliberately not the metric's own similarity*:
     MMR asks whether two results are alike, which is direction, while the ranking under
     dot product or euclidean also weighs magnitude.
   - **Greedy pick,** until `top_k` are picked or the window is spent: choose the unpicked
     `d` maximizing `λ·rel(d) − (1−λ)·M(d)`, where `M(d)` is the maximum of `sim(d, p)` over
     the picked `p`, kept as a running maximum updated as each pick lands, and is 0 while
     nothing is picked. **Strictly greater wins, so the earlier candidate keeps ties**: with
     `λ = 0` the first pick is the first candidate.
   - The window is a limit on the candidates, not on the picks: a window that shadows and
     deletes leave short returns fewer rows.
   - Each returned row keeps its fused (or boosted) `score` and its `$dist`. **The order is
     MMR's, so scores are no longer descending**, and the docs say so.
5. **With a boost,** the boost rescores the window first and MMR picks from the boosted
   window using the boosted scores, which the rows keep. With `highlight`, marks go on the
   final rows. `meta.unfolded_hits` counts the **picked** rows that are unfolded.
6. **Refused `400`,** each message naming `diversify`, and checked at the top of `plan`:
   - `diversify` not an object, or a field other than `lambda` and `window`;
   - `lambda` missing, not a number, or outside 0–1;
   - a `window` not a whole number from `top_k` to 1,000, or differing from a boost's;
   - `top_k` above 1,000;
   - `diversify` beside `rank_by`, `aggregate_by`, `offset` or `as_of` (the epoch's segments
     take a path of their own);
   - `diversify: null` is no diversify, as serde reads it.

**Not changed:** the format, HEAD, the ranking, and every query without `diversify`.

**Not covered, and the ledger says so:**
- `as_of`, a named field's vectors (`multi`), and sparse vectors: similarity is `vector`'s;
- returning the picked rows' vectors (`include_vectors`);
- a diversity over attributes (a result cap per brand);
- a row with no vector is unreachable through the API, which requires one: the case is the
  query layer's.

## Acceptance criteria

1. **MMR is MMR.** `mmr_picks_as_the_model_does`, in
   `crates/pstore-server/tests/diversify.rs`: under cosine, euclidean **and dot product**,
   over clustered near-duplicate vectors **of unequal norms**, unfolded, folded and mixed, a
   diversified query's ids equal the test's own greedy MMR (written in `f64` from the vectors
   it wrote and the unboosted window's scores) for `lambda` 0, 0.3, 0.7 and 1. The fixture
   asserts every choice's margin over the runner-up exceeds `1e-5`, except the `lambda` 0
   first pick, which is the first candidate. With `lambda` 1 the ids are the fused order. With
   `lambda` between, the second pick is not the first's cluster, and the response's scores
   are not descending.
2. **Ties and a missing vector.** `mmr_ties_keep_the_earlier_candidate`, in
   `crates/pstore-server/tests/diversify.rs`: identical vectors pick in the engine's order,
   by hand `d00, d03, d01, d02`. In `crates/pstore-server/src/diversify.rs`'s unit tests:
   `similarity_is_zero_without_a_usable_vector` (no vector, another dimension, a zero vector),
   `a_negative_similarity_is_a_bonus`,
   `a_range_that_is_flat_or_not_finite_gives_every_candidate_relevance_one`,
   `lambda_one_is_relevance_and_lambda_zero_starts_with_the_first`, and
   `ties_keep_the_earlier_candidate_and_a_short_window_returns_what_it_has`.
3. **With a boost.** `diversify_picks_from_the_boosted_window`: the boost's `final` scores
   and window are MMR's `s` and window, the response's scores are the boosted ones, a
   `window` named on one governs both, and different windows are refused.
4. **Depth and cost.** `vectors_ride_the_row_round`: a cold diversified query is four rounds
   deep at one segment and at four, folded, and with unfolded rows **whose shadow check
   fetches every candidate's vector**. Its bytes exceed the same query's at `top_k = window`
   and stay within them plus the corpus's whole vector sections. `Answer::vectors` is empty
   without it.
5. **Refusals.** `diversify_is_refused_at_the_door`: every `400` of rule 6, the limits
   accepted, `null` accepted.
6. **The query layer,** in `crates/pstore-query/tests/vectors.rs`.
   `vector_rows_ride_with_the_rows`: with `with_vectors`, every resolved document of a
   segment carries the vector the segment holds at its row; without it, none; with a
   nonempty shadow, the candidates carry theirs.
   `a_segment_whose_vectors_section_is_not_vector_resolves_without_one`: a segment whose
   only dense field is not `vector`, and one whose `vector` holds several vectors a row,
   resolve without one.
7. **The engine,** in `crates/pstore-engine/tests/vectors.rs`.
   `client_space_is_the_same_for_unfolded_and_folded_rows`: under each metric, the same
   document read from a segment and from the unfolded rows has the same client-space vector.
   `a_hit_whose_row_vanished_does_not_misalign_the_vectors`.
   `vectors_are_empty_unless_asked_for`.
8. **Gates:**
   - `./scripts/gates.sh` passes;
   - the mutation sweep of the changed lines misses 0, or each miss is killed by a test named
     in the ledger or is equivalent.

## Test plan

| AC | Seen red first under |
|---|---|
| 1, 3 | `diversify` absent (the field unknown, the query fused only) |
| 1 | similarity as a dot product; the euclidean extra component left in |
| 2 | the first-wins tie rule as last-wins |
| 3 | MMR over the unboosted scores; the boost window ignored |
| 4 | the vector read after the rows (a chain: depth five at one segment); vectors only for the final hits on the shadow path |
| 5 | each refusal removed; the limits tightened |
| 6 | `with_vectors` ignored; the layout check removed |
| 7 | the unfolded branch skipping the client-space conversion |

## RA budget

Depth unchanged: the vectors are read in the row fetch, concurrently with the rows.
- **Nothing unfolded:** `window` rows' vectors, one coalesced ranged read set per segment
  holding a hit, beside the row reads. Bytes at least `window × (dims + extra) × 4`; the
  coalescer may bridge holes.
- **Something unfolded:** up to `legs × window` rows' vectors.
- CPU: `top_k × window × dims`, on a blocking thread.

## Risks

- **Bytes.** 1,000 candidates at 1,536 dimensions is 6 MB a query, 18 MB for three legs with
  something unfolded. The window cap bounds it.
- **A diversified answer is not sorted by score.** Clients assuming it are wrong; the docs
  say so.

## Tasks

- **M64.1** `with_vectors` in the row fetch; `Answer::vectors`; `Engine::query_split_vectors`
  and `resolve_rows_with_vectors`; `diversify` parsed and applied in `run`; the docs; the
  tests: AC1–AC8.
