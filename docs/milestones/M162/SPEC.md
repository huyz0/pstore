# M162: `offset` on a relevance ranking

## Serves
Workstream F of [the delivery plan](../../comparisons/turbopuffer-next-plan.md):
"relevance offset", and "cursor pagination and offset have explicit bounds". The vendor's
`Query.offset` applies to every ranking; pstore takes it only beside `rank_by`
([M9e](../M9e/SPEC.md)). D-34's depth budget is unchanged.

## Delta
Today `offset` without `rank_by` is refused ("offset applies only to a rank_by order").

After, a relevance query (one with `vector`, `multi`, `text`, `field` or `exact`) may also
carry `offset: o`, an integer of at least 0. Its answer is rows `o` to `o + top_k` of the
answer the same query would give with `top_k` set to `o + top_k` and no `offset`:
- The engine ranks `o + top_k` rows (each leg cut at that depth, or at a boost's or a
  group limit's window, as today), and the server returns the rows after the first `o`.
  That definition holds for an approximate ranking too: a page is a slice of one ranking
  at the page's own depth. Two pages are slices of one ranking when `exact: true`, or when
  both depths reach every candidate; otherwise an approximate ranking at another depth
  may order rows differently, which the migration notes say.
- With a boost (M62) or `limit_per` (M146): their window must reach `o + top_k` (it is
  from `o + top_k` to 1,000; the default is twice `o + top_k`, from 100 to 1,000), and the
  boost rescoring and the group cut run over the window as today, stopping at `o + top_k`
  kept rows before the first `o` are skipped. So a page of a group-limited ranking is a
  slice of the group-limited ranking.
- `o + top_k` may be at most 10,000, as for `rank_by` (the bound is the same constant).
- Highlights mark the returned rows; `unfolded_hits` counts the returned rows an unfolded
  row answers; `score` and `$dist` are the rows' own.
- In a multi-query, each query's `offset` is its own; an outer `rerank_by` (M142) fuses
  the returned pages, ranks counted from 1 in each page, as M142 already states.

Refused with `bad_request`, before any blob request: `offset` beside `diversify` (MMR's
picks depend on how many it picks), beside `aggregate_by` (unchanged), not an integer of
at least 0 (unchanged, by the type); `o + top_k` past 10,000; a boost's or group limit's
window below `o + top_k`.

How: `plan` computes the page end `o + top_k` and passes it, not `top_k`, to the legs'
depth, the boost and group-limit windows, and the engine; `run` cuts the final rows at
the page end and then skips `o`. `unfolded_hits` takes the counted-by-id path whenever an
offset is given.

Unchanged: every query without `offset`; `rank_by`'s `offset`; facets, whose own query
has none.

## Acceptance criteria
1. Server, for a dense query, a text query, a hybrid RRF query and an `exact` query, and
   offsets 0, 1, 3 and 7 with `top_k` 4: the page equals rows `o..o+4` of the same query
   at `top_k = o + 4` without `offset`, ids, scores and `$dist` alike.
2. With `exact: true`, pages 0, 4 and 8 of `top_k` 4 concatenate to the first 12 rows of
   `top_k = 12`.
3. With a boost, and with `limit_per`, a page equals the slice of the same query at
   `top_k = o + top_k` without `offset`, at the same named window.
4. With unfolded rows in the index, `unfolded_hits` equals the returned rows whose ids
   were written unfolded, on a page where that differs from the unpaged count.
5. Refusals, each `bad_request` with no blob request: `offset` beside `diversify`; beside
   `aggregate_by`; `top_k + offset` of 10,001; a boost window of 10 with `top_k` 5 and
   `offset` 6; the same for `limit_per`.
6. Every existing test passes unedited, except the one case of
   `order::rank_by_refuses_what_it_cannot_mean` that asserts `{"vector": ..., "offset": 1}`
   is refused: this milestone changes exactly that answer, so that case moves to a test
   that asserts the new answer, and the change is named in VERIFIED.md.

## Test plan
Server tests in a new `crates/pstore-server/tests/it/relevance_offset.rs`. Red before:
1 to 4 fail (the parent refuses `offset` without `rank_by`); 5's cases pass on the parent,
which refuses every relevance offset, so their red is the mutations; 6 is a guard.
Mutations, each to be killed:
- the engine ranked at `top_k`, not the page end (1, 2);
- the skip done before the cut, or not done (1);
- the boost or group window checked against `top_k` (5); its default from `top_k` (3);
- `unfolded_hits` from the engine's hits with an offset (4);
- the 10,000 bound off by one (5); `offset` accepted beside `diversify` (5).

## RA budget
Unchanged depth: one ranking, at a deeper limit. Bytes and CPU grow with `o + top_k`, as
`top_k` did; the 10,000 bound caps them as it caps `rank_by`.

## Risks
- Paging an approximate ranking with growing offsets is not a cursor: rows can repeat or
  be skipped between pages at different depths. Documented; `exact` or `rank_by` are the
  stable choices.

## Tasks
- M162.1: the page end in `plan` and `run`, the refusals, docs.
