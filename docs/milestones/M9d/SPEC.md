# M9d — Distance metrics, exact kNN, `$dist`, base64 vectors

**Serves:** D-36 (base64 vectors) and the metric rows of
[`turbopuffer-api-parity.md`](../../research/11-design/turbopuffer-api-parity.md). Fourth of M9.
⚠️ Revised at spec review (block): `$dist` is joined from the dense leg, not read off a fused
score; the fold records a new schema's metric *before* its reject pass; and `exact` is the
full-vector scan, not an extra rung. The review also found clustering and the probe rank by
L2, not dot product, and proposed a dot-product probe -- ⚠️ **measured and not taken** (below).

## Delta

**Wire.**
- A write takes `distance_metric`: `cosine_distance`, `euclidean_squared`, or `dot_product`
  (pstore's metric until now, and the default, so every existing request means what it meant).
  Anything else is `400 bad_request`.
- A document's `vector`, and a query's, may be a JSON array or a **base64 string** of
  little-endian `f32`s. Not base64, a length not a multiple of 4, or a non-finite component:
  `400 bad_request`.
- A query takes `"exact": true` -- turbopuffer's `kNN`: every row scored at full precision.
- Each result carries `$dist` when the query has a dense leg and that leg scored the row:
  `cosine_distance` `1 − cos` (clamped to `[0, 2]`), `euclidean_squared` `‖q − v‖²` (clamped
  at 0), `dot_product` `−q·v` -- smaller is nearer under every metric. Without `exact` it is the
  rung's estimate (int8 by default), with `exact` the full-precision value. A hit only another
  leg scored carries none.

**Transforms.** The ranking rungs (RaBitQ, int8, full precision) maximise a dot product, so each
metric is a transform on the write and on the query that makes the dot product rank as the
metric does:
- `cosine_distance`: stored vector and query **normalized**; `cos = q̂·v̂`. A zero vector has no
  direction: refused at the door, as a document and as a query.
- `euclidean_squared`: stored `[v, −‖v‖²/2]`, query `[q, 1]`; `q'·v' = q·v − ‖v‖²/2`, which ranks
  as `−‖q − v‖²`, and `‖q − v‖² = ‖q‖² − 2·q'·v'`.
- `dot_product`: unchanged.

**The probe is unchanged, by measurement.** Clustering (k-means) and the probe rank by L2 over
the stored (transformed) vectors. Review argued the augmented component swamps an L2 probe and
proposed ranking lists by dot product with the transformed query. Built and measured on
criterion 2's corpus (600 rows, 12 clusters, norms spanning 10×, 20 queries, `p = 8`), recall@10
under `euclidean_squared`: L2 **0.955 / 0.805** at 20 / 10 rows a list, dot product
**0.915 / 0.725**, L2 over the unaugmented components **0.925 / 0.75**; under `cosine_distance`
0.99 for every probe. So the L2 probe every index has stays, and criterion 2 is what would
catch a corpus where it does not (`provisional`).

**Exact.** `exact` sends the dense leg down the **full-vector scan** a segment without a
centroid table already takes (open → `Vectors` section): exact at today's depth, no
quantized codes read. So it adds **no round trip**; bytes are the index's vectors.

**Where the metric lives.** HEAD's schema records it beside `dims` (a new optional trailing
section, index → metric; absent means `dot_product`, which every existing index is). A row
carries its metric from the write to the fold as the reserved attribute `$metric`; **a row
without it is `dot_product`**. It is stripped before any segment is sealed and before the fresh
view's rows are built, so no segment stores it and no query returns or filters on it. Attribute
names beginning `$` are refused at the write door. The metric is checked **exactly as the width
is**, at every rung: the cached schema and this process's unfolded rows at the door (`400`), and
per row in the fold's reject pass (rejected and counted, never re-scaled). ⚠️ **A fold creating
a schema records the metric (and width) first, then runs the reject pass against it** -- today
the pass is skipped for a new index, so two writers' first rows of different metrics would be
sealed together silently. The engine takes a query's metric from the HEAD it already read (an
index not yet folded: from its unfolded rows) and checks the query's width against the client
width before transforming.

**Client width.** `dims` records the stored width (`d + 1` under `euclidean_squared`); every
width a client sees -- `GET /v1/indexes/{index}`, a conflict or a mismatch message -- is `d`.
`Engine::search` (an L2 scan over stored vectors) is documented as `dot_product`-only.

**Does not change:** any `dot_product` request (the default) -- same probe, same requests; the
segment format; fusion (RRF over ranks).

## Acceptance criteria

1. Under each metric, over documents whose norms vary (≥ 10× spread), a dense `exact` query's
   `$dist` equals a brute-force computation to `1e-3` absolute + `1e-3` relative, and its ids
   are the brute-force top `k` (ties within the tolerance are interchangeable) -- unfolded and
   folded.
2. Without `exact`, a clustered segment (600 rows, norms spanning ≥ 10×, `exact_scan_threshold`
   below 600, the probe asserted non-exhaustive) returns at least 8 of the true top 10 under
   `cosine_distance` and under `euclidean_squared`, averaged over 20 queries -- `provisional`.
3. A base64 vector writes and queries identically to the same JSON array; malformed base64, a
   ragged length, a non-finite component, an unknown metric, a zero vector under cosine (write
   and query), and an attribute beginning `$` are each `400 bad_request`.
4. A metric contradicting the index is: refused `400` against the cached schema; refused `400`
   against this process's unfolded rows; rejected at the fold with `rejected_rows` +1 --
   including two engines' first rows of one new index; the index's rows unchanged in each.
   `GET /v1/indexes/{index}` reports the metric and the client width.
5. `exact` adds no round trip: equal depth with and without it, on a clustered segment and on an
   exact one; a `dot_product` query's requests are unchanged.
6. `./scripts/gates.sh` passes; `./scripts/mutants.sh` over the diff misses 0.

## Test plan

| # | Fails first | Mutation it catches |
|---|---|---|
| 1 | no `distance_metric` (ignored) | a transform skipped on one side; `$dist` sign or factor; normalization skipped (norms vary) |
| 2 | — (premise asserted: the probe is not exhaustive) | the augmentation's sign; normalization skipped |
| 3 | JSON arrays only | a ragged tail truncated rather than refused |
| 4 | no metric in the schema; first fold unchecked | a rung of the metric check skipped |
| 5 | no `exact` | `exact` through the rungs (+1 round) |

## RA budget

Unchanged. `exact` reads every vector of the index (bytes linear in it, as `filtering.md`
prices an exhaustive probe) at the default depth.

## Risks

- k-means over augmented vectors partitions partly by norm; criterion 2 measures recall on one
  corpus, `provisional`.
- `$metric` rides every unfolded row in memory and in bundles (a few bytes each).
