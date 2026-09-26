# M9e — Order by attribute, `offset`, and paging by id

**Serves:** the `rank_by: [attr, "asc"|"desc"]` row of
[`turbopuffer-api-parity.md`](../../research/11-design/turbopuffer-api-parity.md) -- and the
export path, as turbopuffer made it: every document of an index, a page at a time, by id.
Fifth of M9. ⚠️ Revised at spec review (block): the depth is **three** (HEAD first), and the
bound is on what the query **holds**, not only on what it returns -- rows are selected, not
sorted.

## Delta

**Wire.** `POST /v1/indexes/{index}/query` takes `rank_by: [attr, "asc" | "desc"]` (`attr` an
attribute name, or `id`) and `offset` (default 0). Refused with `400 bad_request`: `rank_by` with
`vector`, `text`, `field` or `exact: true` (an ordered answer and a relevance answer are different
questions); `offset` without `rank_by`; a direction other than `asc`/`desc`; a `rank_by` not a
two-element array of strings; `top_k + offset > 10000`. A `null` `rank_by` or `offset` is the
field omitted. Results carry `id` and `attributes` as
any query's do; **`score` and `$dist` are absent** (`score` becomes optional on the wire, still
serialised for every relevance query).

**Order**, a total order on `(value, id)`, byte-wise for every string, the id included:
- `asc`: present integers ascending, then present strings ascending, then documents without
  the attribute.
- `desc`: present strings descending, then present integers descending, then documents without
  the attribute -- the present values reversed, the absent still last.
- Ties, and the absent group, by id **ascending** in both directions.
`id` is always present and a string. An attribute no document has orders everything by id.

**Engine.** `Engine::ordered(index, attr, desc, filter, offset, limit, as_of)` returns the rows
and how many came from unfolded rows (`meta.unfolded_hits`). Per segment, one open round -- the
segment's suffix and its delete vector together -- then one block round: the blocks M9b's zone
maps admit (all of them without a filter), each decoded **in turn** and its rows admitted by
the predicate, minus the rows the delete vector names and the ids with unfolded operations
(M9c), fed to a **bounded selector** holding at most `offset + limit` rows. The unfolded rows
the filter admits join the same selector; segments merge by the same comparator. Peak memory:
the fetched block bytes of one query -- undecoded, and unfiltered that is the index's data
bytes -- plus `segments × (offset + limit)` rows; never the decoded index. `as_of`: that epoch's manifest and delete vectors, no unfolded rows.

**Depth: three** -- HEAD, open, blocks -- D-34's budget, and one fewer than a relevance query,
because the blocks carry ids and attributes and there is no resolve round.

**Paging.** `rank_by: ["id", "asc"]` with `filters: ["id", "Gt", <last id>]` visits every
document once. Why: each page is answered from **one** HEAD, and every object it reads through
that HEAD is immutable -- segments, and delete vectors keyed by epoch and lane -- so a fold or
compaction between its rounds cannot change it (a GC that reaped an input fails the page; it
never answers wrongly). Within a page no id appears twice (delete vectors, the shadow, the
newest operation per id). The cursor rises strictly between pages. Limits, stated: unfolded
operations are visible only on the process holding them, so pages served by different
processes see different unfolded states (still once per id); rows not yet flushed are exported
and are lost if that process dies before its flush. `offset` pages by position instead, and is
exact only against an index that does not change between pages.

**Does not change:** any query without `rank_by`; the segment format; the write path. The
reader gains an API, not a format: `Segment::visit_rows_where`, which fetches the admitted
blocks in `rows_where`'s one coalesced round and hands each block's rows to a callback as it
decodes them, instead of collecting every decoded row.

## Acceptance criteria

1. `rank_by` over integers, strings, both in one attribute, and a partly absent attribute
   returns exactly the order above, `asc` and `desc`, ties by id -- unfolded, folded, both, and
   with `as_of`. The comparator is checked against a brute-force sort in a unit test.
2. A filtered `rank_by` returns only admitted documents; `offset` skips exactly `offset` of
   them **across segments and the unfolded rows** (an offset spanning two segments and the
   fresh one).
3. Paging with `["id", "Gt", last]` and `top_k = 7` visits every one of 50 documents exactly
   once while, between pages, a new id beyond the cursor is written, an id beyond the cursor is
   deleted, one beyond it re-upserted, and the index folded and compacted: the export returns
   the new id, the upserted version, and not the deleted id.
4. Deleted and superseded rows never appear, folded or unfolded.
5. `DepthCounting` over the engine's store: a `rank_by` query is **3** deep with 1 segment and
   with 8, with and without unfolded rows, and with `as_of`; an unfiltered page issues at most
   2 requests per segment in the open round and 1 per segment in the block round (a store
   whose `coalesce_gap` is above zero, as every real backend's is).
6. The bounded selector never holds more than `offset + limit` rows and equals a full sort
   truncated, on a random input (a unit test).
7. Each refusal in **Wire** is `400 bad_request`; a relevance query still serialises `score`,
   and a `rank_by` result has neither `score` nor `$dist`.
8. `./scripts/gates.sh` passes; `./scripts/mutants.sh` over the diff misses 0.

## Test plan

| # | Fails first | Mutation it catches |
|---|---|---|
| 1 | no `rank_by` (a query needs a vector or text) | `desc` as `asc` reversed (absent first); `Value`'s order used for `desc`; ties by id descending |
| 2 | as 1 | the filter after `offset`; `offset` per segment |
| 3 | as 1 | the cursor as `Gte`; the delete vectors ignored |
| 4 | as 1 | the shadow ignored |
| 5 | — | a round per segment; the delete vector fetched after the open round |
| 6 | no selector | an unbounded buffer; an off-by-one at `offset + limit` |
| 7 | accepted or ignored today | a refusal skipped |

## RA budget

Three sequential round trips, all fan-outs. Requests: open, 1 per segment plus 1 per delete
vector; blocks, 1 coalesced plan per segment -- one GET unfiltered, at most the admitted runs
when filtered (grows with blocks, never with rows). Bytes: the admitted blocks -- the **whole
index** unfiltered, **for every page**: an id cursor cannot prune, because a block has no id
zone. So exporting `N` documents in pages of `top_k` reads the index `N / top_k` times; at the
10,000 page bound, a 1M-document index is read 100 times. That is the stated price of this
milestone's export, and it is fit for indexes up to roughly 10^5 documents (10 full reads at
the bound). A per-block id range in the block meta, which would make a page read `top_k` rows'
blocks, is a format change and the follow-up.

## Risks

- The export's cost above; the follow-up is named, not scheduled.
- `offset` pages overlap or skip across changes, by design.
