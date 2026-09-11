# M3c — Replication without row duplication

**Serves:** backlog item 14, which [M5g](../M5g/VERIFIED.md) opened when it clamped
`replicas: 0` in the engine, and whose premise `recall --replicas` corrected.

**Depends on** [M3](../M3/SPEC.md)'s `augment`, which is the mechanism, and
[M5g](../M5g/SPEC.md), which is why it is currently switched off.

## ⚠️ The failure: a replicated vector is a duplicated document

`augment` puts a boundary vector into a second posting list, and `assemble` writes rows in
**list order** — so the document is pushed to the segment **twice**. Measured while landing
M5g: 400 documents folded and merged came back as **431 rows**, and it compounds on every
merge because a compaction re-seals what it scanned. `Engine::scan` promises every row
"exactly once". So the engine clamps `replicas: 0`, and every caller that does not gets:

1. duplicate documents from `scan`, growing on each compaction;
2. ⚠️ **corrupted BM25 statistics** — `text::build` runs over the expanded rows, so a
   replicated document is counted twice in `doc_count` and in every one of its terms' `df`.
   Nothing catches this: the recall gate builds no text field, and the ndcg gate does not
   replicate;
3. duplicated sparse postings, the same way.

## ⚠️ What this buys, corrected — it is bytes, not recall

The backlog said the clamp cost r@10 p=2 **0.961 → 0.844**. That compared two different probe
widths, and `Query::default()` is **8**. Measured on both corpora
(`cargo run --release -p pstore-index --example recall -- --replicas`):

| | recall@10 | MB/query | index |
|---|---|---|---|
| replicas 0, p=8 (what the engine runs today) | **0.9810** | 0.769 | 1.00× |
| replicas 1×0.05, p=8 | 0.9810 | 0.841 | 1.64× |
| replicas 0, p=4 | 0.9680 | 0.429 | 1.00× |
| **replicas 1×0.05, p=2** | **0.9610** | **0.288** | 1.64× |

⚠️ **At the default probe width replication buys nothing and costs bytes.** What it buys is
**~33% fewer query bytes at equal-ish recall**, reachable only at small `p` — and `p` can only
be lowered once replication is available, which is the point. `cost-model.md` prices a node on
scan bytes; `Params::default()`'s own comment states the principle: *"storage is the cheap
resource and query bytes are the scarce one."*

## ⚠️ The shape: two row spaces, and only one of them is the document

The codes a probe reads may repeat a vector; the **document** may not. So:

- **Data blocks, sparse postings, text postings and fieldnorms** are built over the
  **primary** rows — each document once, in primary-list order.
- **`Vectors`, `RaBitQ` and `Sq8`** are written over the **index** rows — list order, replicas
  included — because that is what makes a probe one contiguous ranged read.
- ⚠️ A new section maps index row → data row, so a hit can name a document.

**Rejected: scattering the replicas.** Storing each document once and letting a list reference
rows non-contiguously breaks *"rows are written in LIST order, so a posting list is one
contiguous byte range and a probe is one ranged read rather than a scatter of thousands"* —
which is the layout argument the whole index rests on.

**Rejected: a replica bit and a filtering scan.** It keeps one row space, so `row_count()`
would count replicas while `scan` returned fewer — and `SegmentRef.rows`, the fold's own
bookkeeping, is that number.

## The numbers this milestone is pinned to

| Name | Value | Why this value |
|---|---|---|
| New section | **`IndexRows = 15`** | The next free id after `TextFields = 14`. A `u32` per index row: the data row it names. ⚠️ Additive — absent means "index rows *are* data rows", which is every segment written before this and every segment built with `replicas: 0`. |
| Size | **4 bytes × index rows** | At 1.64× and 20,000 documents that is ~131 KB against a `Vectors` section of 30 MB. It rides in the body, not the meta region: a probe already fetches ranges, and only the probed rows' mappings are needed. |
| Written when | **replicas actually duplicated a row** | `replicas: 0` writes no section and produces byte-identical segments to today's. |

## Delta

**`pstore-format`**
- `Section::IndexRows = 15`, and `Segment::index_rows() -> Option<&[u32]>`.
- ⚠️ `vector_row_len()` and every fixed-width code section derive their stride from the
  **index** row count when the section is present, not from `row_count()`. That divergence is
  the whole hazard of this milestone: a stride computed from the wrong count reads every row
  after the first at an offset, and returns vectors that decode without complaint.

**`pstore-index`**
- `assemble` writes blocks and the two sidecars over primaries, codes over index rows, and the
  mapping.
- ⚠️ `VecIndex::search` maps hits through it **and deduplicates by data row, keeping the best
  score** — without that, a replicated document appears twice in one top-k.

**`pstore-engine`**
- `seal` stops clamping `replicas: 0` — the clamp existed because replication was *wrong*, and
  it no longer is. ⚠️ **The engine's own default stays `replicas: 0`.** Spec review caught the
  first draft here: unclamping while `Params::default()` carries `replicas: 1` would give the
  engine 1.64× stored codes at `p=8` for **0.0000** recall, which is strictly worse than today.

## ⚠️ This milestone enables nothing, deliberately

The shipped configuration after it is **byte-identical** to before. What it changes is that
replication is **correct** rather than switched off because it is broken — so the p-vs-
replication trade above becomes a decision someone can take, instead of one the format forbids.

That is the honest value, and it is worth having on its own because the breakage is not only
the row duplication M5g found:
**`vec_index::build_all` corrupts BM25 statistics for any caller using the default `Params`
with a text field**, and nothing catches it — the recall gate builds no text field and the ndcg
gate does not replicate. That defect has been live since M3.

**Does not add** — **a lower default `p`.** The bytes this buys are only reachable at small
`p`, and changing `Query::default()` from 8 is a separate decision with its own measurement,
on a corpus that is not this one. **Replication for sparse or text retrieval** — this is the
dense index's mechanism. **Rebuilding existing segments**: absent section means index rows are
data rows, so every segment already written stays correct and unreplicated.

## Acceptance criteria

1. ⚠️ **A replicated segment holds each document exactly once.** With `replicas: 2,
   boundary: 0.9` over 300 documents, `row_count()` is 300, `scan` returns 300, and a
   compaction of two such segments returns their sum rather than more. This is the criterion
   M5g's clamp exists to satisfy, now met with replication **on**.
2. ⚠️ **The codes still carry the replicas.** The index row count exceeds the data row count,
   and the mapping is that long. Otherwise criterion 1 passes over a build that silently
   dropped replication and this milestone buys nothing.
3. ⚠️ **A replicated document appears once in a top-k.** Probing two lists that both hold it
   returns it once, at its best score. Without the dedupe it appears twice and displaces a
   real neighbour.
4. ⚠️ **BM25 statistics count each document once.** A replicated corpus with a text field has
   the same `doc_count` and the same per-term `df` as the unreplicated one, and the same text
   ranking. This is the defect that has been live since M3 and that nothing catches today.
5. **Recall improves at small `p` and the bytes fall**, measured by the harness: at `p=2` the
   replicated index is above the unreplicated one by more than 5 points and moves fewer bytes.
6. ⚠️ **The shipped configuration is byte-identical.** `replicas: 0` produces exactly the bytes
   M5g wrote, an existing segment opens and searches exactly as before, and the engine's
   answers do not move. This milestone makes replication *correct*; it turns nothing on.
7. ⚠️ **Strides are derived from the right count.** A segment whose index rows outnumber its
   data rows returns the correct vector for every row, asserted against the input vectors
   rather than against itself.
8. Region coverage ≥95% on the changed crates, mutation ≥80% on the changed modules, gates
   green, and `scripts/recall.sh` and `scripts/ndcg.sh` above their floors.

## Test plan

| # | Test | The mutation it kills |
|---|---|---|
| 1 | `a_replicated_segment_holds_each_document_once` | blocks written over index rows — the defect the clamp exists for |
| 2 | `the_codes_carry_the_replicas` | replication silently dropped, which makes criterion 1 pass for the wrong reason |
| 3 | `a_replicated_document_appears_once_in_a_top_k` | the dedupe removed, which displaces a real neighbour with a duplicate |
| 4 | `replication_does_not_change_the_bm25_statistics` | the sidecars built over index rows — live since M3, caught by nothing |
| 5 | `recall --replicas`, reported in the ledger | — |
| 6 | `an_unreplicated_segment_is_byte_identical` | the section written unconditionally, which changes every existing segment's bytes |
| 7 | `every_row_decodes_to_its_own_vector` | the stride taken from `row_count()`, which reads every row after the first at an offset and decodes without complaint |

⚠️ Criteria 1 and 2 must both hold: either alone is satisfied by a build that does the opposite
of what this milestone is for.

## RA budget

| Operation | W | Rseq depth | Rpar | List |
|---|---|---|---|---|
| Dense query | 0 | unchanged | unchanged — the mapping rides in the ranges a probe already fetches | 0 |
| Sealing | unchanged | unchanged | unchanged | 0 |

⚠️ Query **bytes** fall at small `p` and rise slightly at large `p` — the table above is the
measurement, and it is the reason for the milestone.

## Risks

- ⚠️ **Two row spaces in one segment is the sharpest edge in the format.** Every fixed-width
  section must agree about which count it strides by, and a disagreement decodes silently.
  Criterion 7 aims at it directly.
- ⚠️ **The trade is not taken here and may never be.** 1.64× stored codes buys ~33% query
  bytes only at small `p`, and lowering `p` needs its own measurement on a corpus that is not
  this one. If that decision goes the other way, this milestone will have fixed a real defect
  and enabled an option nobody exercises — which is why criterion 6 pins that the shipped
  bytes do not move.
- **The BM25 defect is fixed here rather than in its own milestone**, because it is caused by
  the same line and fixing one without the other leaves a corpus counted twice.

## Tasks

| Id | Commit |
|---|---|
| **M3c.1** | `Section::IndexRows`, and strides that know which count they mean |
| **M3c.2** | The builder separates the two row spaces, and search maps and dedupes |
| **M3c.3** | The engine stops clamping, and each document is still returned once |
