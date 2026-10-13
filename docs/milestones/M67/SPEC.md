# M67 — Multi-vector candidates from a fixed-dimensional encoding (MUVERA)

**Serves:** backlog row 63, from M61's plan, piece 2: *a `multi` leg reads every float of its
field in every segment* (51 KB a document at 100 vectors of 128 dimensions). The project
owner's comparison of 2026-10-08 (row 71) names MUVERA, which reduces multi-vector retrieval
to single-vector retrieval.

⚠️ **This changes the mechanism row 63 proposed** (per-vector 1-bit codes, a format change):
a fixed-dimensional encoding (FDE) needs **no format change** -- it is one more ordinary dense
field per document, written by the server. **Row 65 (a dense leg over a named field, with a
clustering of its own) stays open:** the FDE leg here is an exact scan of the FDE field, as
the `multi` leg is an exact scan of its own.

## What is true today

- A `multi` leg (M61) reads the field whole per segment (`Segment::field_vectors`, one ranged
  read) and scores every row by exact MaxSim: the sum over query vectors of the best dot
  product with the document's.
- A named field's section is variable-width: `rows + 1` offsets of 8 bytes, then the vectors.
- Named fields are written through `DocumentIn.vectors`; `named_field` bounds their names.
- Compaction keeps every field (M60).
- `rerank: exact` is, by the project's depth rule, an opt-in outside the three-round budget.

## The encoding (Dhulipala et al., MUVERA, 2024)

For a set of `d`-dimensional vectors, `R` repetitions each do:
1. **Partition** by SimHash: `k` random Gaussian vectors `g_1..g_k`; a vector `p` falls in
   bucket `b(p) = Σ 2^i · [g_i · p > 0]`, of `B = 2^k`.
2. **Aggregate** per bucket: the **query** encoding *sums* its vectors in a bucket; the
   **document** encoding takes their *mean*, and a bucket the document leaves empty takes the
   vector of the document whose bucket id is nearest by Hamming distance (the earliest on a
   tie).
3. **Project** each bucket's vector to `d_proj` dimensions by a random sign matrix
   (`±1/√d_proj`); when `d <= d_proj` it is kept as is.
4. **Concatenate** buckets, then repetitions: `R · B · d_proj` floats.

The dot product of a query's and a document's encodings approximates the Chamfer similarity
(MaxSim). Randomness is from a **fixed seed**: the same input is the same encoding on every
node and across restarts, which a stored encoding requires.

**Fixed parameters,** not configurable (a configurable one would be a schema to record and a
migration to run): `R = 4`, `k = 3` (8 buckets), `d_proj = 16`: **512 floats, 2 KB a
document**, whatever the vectors' count or `d`. The seed derives from `(rep, d)`.

## Delta

1. **Write.** For every named field `name` a document carries, the server also writes the
   encoding as a derived single-vector field **`name$fde`** (dense, 512 floats).
   - A field name ending in `$fde` is refused `400` (it is the derived field's), and the
     derived fields count against no limit of the document's: a document may hold 8 fields of
     its own and 8 derived.
   - The derived field is **never returned**: no response shows a field ending in `$fde`.
   - Folded, compacted and branched exactly as any named field is (M60).
2. **Query.** The `multi` leg gains `mode` and `rerank`:
   `{"field": "late", "vectors": [...], "mode": "fde", "rerank": "exact"}`.
   - `mode`: `"exact"` (the default, M61's leg) or `"fde"`. In `fde` mode the leg encodes the
     query vectors (summing per bucket) and scores every row of `name$fde` by dot product:
     one ranged read of 2 KB a row, **not** the field's whole vectors.
   - `rerank`: absent or `"exact"`, only with `mode: "fde"`. With `"exact"`, each segment takes
     the leg's top `rerank_window` (default `4 × limit`, at most 1,000) by FDE and re-scores
     them by exact MaxSim, reading the offsets table and then the candidates' vector ranges.
     ⚠️ **That is two reads in sequence after the leg's, outside the D-34 budget by the
     project's rule that `rerank: exact` is an opt-in**; the docs say so, and `meta` is
     unchanged. Without `rerank` the leg's scores are the FDE's: approximate.
   - A segment without `name$fde` (written before M67) answers nothing in `fde` mode, as a
     segment without the field answers nothing for `multi`; the docs say **documents written
     before M67 are invisible to `fde` mode** until they are rewritten.
   - Refused `400`: an unknown `mode`; `rerank` without `mode: "fde"`; any value of `rerank`
     but `"exact"`; `rerank_window` without `rerank` or outside `limit..=1000`... (see AC).
3. **One implementation,** `pstore_format::fde` (`encode_doc`, `encode_query`, and the
   parameters), used by the server's write and the query layer's leg, so the two cannot
   disagree about a bucket.
4. **Reading rows.** `Segment::field_rows(store, key, name, rows)` reads the named field's
   offsets table and then only the named rows' ranges (coalesced), for the rerank.

**Not changed:** the format, `vector`, the exact leg, and every query without `mode`.

**Not covered:** a configurable `(R, k, d_proj)`; product-quantizing the FDE; an
approximate-nearest-neighbor index over the FDE field (row 65: it is a flat scan); splitting
the leg across peers (row 64); rebuilding old documents' encodings.

## Acceptance criteria

1. **The encoding is the algorithm.** In `crates/pstore-format/tests/fde.rs`:
   - `fde_is_deterministic_and_fixed_width`: the same vectors encode to the same 512 floats
     twice and in a different process-independent order of calls; the width is 512 for `d` of
     8, 16 and 128 and for 1 and 1,000 vectors; a query and a document of the same vectors
     differ only as sum versus mean.
   - `fde_with_one_bucket_is_sum_and_mean`: with parameters `k = 0`, `d_proj >= d` the
     document encoding is the mean of its vectors, the query's their sum, and their dot
     product is `Σ_q q · mean(d)`, by hand on a small example. (Parameters are an argument
     of the functions; the leg uses the fixed ones.)
   - `fde_fills_an_empty_bucket_from_the_nearest`: a document with vectors in one bucket only
     has every other bucket equal to the nearest-by-Hamming bucket's vector.
2. **It finds what exact MaxSim finds.** `fde_candidates_recover_the_exact_top`, in
   `crates/pstore-format/tests/fde.rs`: on a seeded clustered corpus (300 documents of 8 to
   24 vectors, `d = 32`, 20 queries of 4 to 8 vectors) the exact top 10 by MaxSim are, on
   average, within the FDE's top 100 with recall **at or above a floor measured on the
   first run and written in the test** -- provisional, deterministic data -- and a control
   ranker (the FDE of a different corpus's seed) falls **below** it.
3. **Through the API.** In `crates/pstore-server/tests/multi_fde.rs`:
   - `fde_mode_ranks_and_exact_mode_is_unchanged`: after writes, a fold and a compaction, an
     `fde` query's top results share the exact query's leading document; the exact query's
     response is byte-identical to before this milestone's (the existing `multi` tests).
   - `fde_reads_less_than_the_exact_leg`: at 64 vectors of 128 dimensions a document, the
     `fde` leg's `bytes_read` is **under a quarter** of the exact leg's, over a cold start,
     and its depth equals the exact leg's.
   - `rerank_exact_orders_by_maxsim`: with `rerank: "exact"` and a window covering the corpus,
     the order equals the exact leg's, scores included; its depth is recorded, and is at most
     the leg's plus 2.
   - `the_derived_field_is_invisible`: no response (query with `include_attributes`, fetch,
     export if any) names a `$fde` field, and a write naming one is `400`.
   - `old_documents_are_invisible_to_fde_mode`: a segment written with the encoding off (a
     fixture through the format's writer) answers nothing in `fde` mode and fully in exact.
4. **Refusals.** `fde_options_are_refused_at_the_door`: every `400` of rule 2.
5. **The reader.** `field_rows_reads_only_the_named_rows`, in
   `crates/pstore-format/tests/fde.rs`: the rows returned equal `field_vectors`' at those
   rows, and the bytes read are the table plus the rows' ranges, not the field.
6. **Gates:** `./scripts/gates.sh` passes; the mutation sweep of the changed lines misses 0,
   or each miss is killed by a named test or is equivalent.

## Test plan

| AC | Seen red first under |
|---|---|
| 1 | `fde` absent (compile) |
| 1 | mean for sum in the query; the empty-bucket rule removed |
| 2 | the projection dropped; one repetition; a bucket from the wrong sign |
| 3 | the derived field not written; `mode` ignored (exact scores: bytes not lower) |
| 3 | rerank reading the field whole (bytes) |
| 5 | `field_rows` reading the whole section |

## RA budget

- Write: +2 KB a document per named field, one dense field in the segment.
- `fde` leg: one ranged read of `rows × (512 × 4 + 8)` bytes a segment, in the leg round,
  concurrent with the other legs. Depth unchanged. CPU: `rows × 512` multiply-adds.
- `rerank: exact`: +2 sequential reads a segment (the offsets table, then the candidates),
  outside the budget as a rerank is.

## Tasks

- **M67.1** `pstore_format::fde`; the server's derived write; the `fde` leg; AC1-AC4.
- **M67.2** `Segment::field_rows`; `rerank: "exact"`; AC3 (rerank), AC5; the docs.
