# M65 — Facets: an aggregation beside a ranked query

**Serves:** backlog row 69, from the project owner's comparison of 2026-10-08. Vespa's
grouping, Elasticsearch's aggregations and Weaviate's `aggregate` next to a search return the
hits **and** the counts over the same filter in one call. pstore has both halves (M12 counts
and sums; the ranked path) but M12 refuses `aggregate_by` beside `vector`, `text` or `multi`,
so a faceted result page is two calls with two chances to see different epochs.

## What is true today

- `aggregate_by` / `group_by` (M12) is its own plan: no `vector`, `multi`, `text`, `rank_by`,
  `offset`, `fusion`, `include_attributes` or `exclude_attributes`. Depth 3, bounded memory,
  every consistency level and `as_of`.
- A ranked query runs `Plan::Ranked`: depth four (D-34).
- `Engine::aggregate_as` and the ranked path each read HEAD themselves.

## Delta

**A ranked query may carry `facets`: the M12 aggregation over the same `filters`, run
concurrently with the ranking and returned beside the hits.**

```json
{"vector": [...], "filters": [...], "top_k": 10,
 "facets": {"aggregate_by": {"n": ["Count", "id"]}, "group_by": ["color"], "top_k": 20}}
```

1. **The request.** `facets` is an object with `aggregate_by` (required), `group_by` and
   `top_k` (optional, defaulting to 10, and the bound of groups, **not** the query's `top_k`).
   Its parsing and limits are M12's, one function for both: 1 to 16 labels, 1 to 8 `group_by`
   attributes, `top_k` to 10,000, `Sum` of `id` refused, a label equal to a `group_by`
   attribute refused.
2. **What is faceted.** The rows the query's `filters` admit -- **the whole filtered set**,
   not the ranked window, the candidates, the unboosted or undiversified rows, or the page.
   `top_k`, `offset`, `boost`, `diversify` and `highlight` do not change it. Facets are over
   the filter, not over what the ranking matched: the docs say so.
3. **The response.** `facets` beside `results`, one of `{"aggregations": {...}}` (no
   `group_by`) or `{"aggregation_groups": [...]}`, exactly M12's shapes. `meta.facets_epoch`
   is the epoch the facets were read at. It equals `meta.epoch` unless a fold landed between
   the two concurrent reads (they are separate reads of HEAD), and the docs say so.
4. **Execution.** The ranked query and `Engine::aggregate_as` (same index, filter, `as_of` and
   consistency) are awaited together (`futures::join`). Depth is the greater of the two (four),
   never their sum. Either failing fails the request. The session token is checked and
   advanced on the ranked read's epoch as today; the aggregation is served at the same level
   and so is refused where M12's would be (`strong` with another process's unfolded write).
5. **Cost.** Blob requests are the ranked query's plus the aggregation's (HEAD, open, admitted
   blocks), which scale with segments and bytes, never records. `meta.cost` sums both.
6. **Refused `400`,** each message naming `facets`:
   - `facets` not an object, a field other than `aggregate_by`, `group_by`, `top_k`;
   - any M12 refusal of its content (message names the rule);
   - `facets` without a ranking leg (`vector`, `multi` or `text`) -- use `aggregate_by`;
   - `facets` beside `rank_by` or `aggregate_by`;
   - `facets` inside a multi-query's sub-query;
   - `facets: null` is no facets.

**Not changed:** M12's own plan, the format, HEAD, and every query without `facets`.

**Not covered:** facets over the ranked window ("counts of what matched"), several
independent facet filters (a facet excluding its own filter), numeric histograms and
distinct counts.

## Acceptance criteria

1. **Facets are the aggregation.** `facets_equal_the_aggregation_of_the_same_filter`, in
   `crates/pstore-server/tests/facets.rs`: over folded rows plus unfolded writes including an
   upsert and a delete of folded ids, grouped and ungrouped, with and without a filter, and at
   `as_of`, the `facets` equal the response of the M12 query with the same `aggregate_by`,
   `group_by`, `filters`, `as_of`.
2. **The ranking is unchanged.** `facets_do_not_change_the_hits`: the `results` equal those of
   the same query without `facets`, for dense, text and hybrid, with a `boost`, a `diversify`
   and a `highlight`.
3. **Whole filtered set.** `facets_count_the_filtered_set_not_the_page`: with `top_k = 3`, a
   boost and a diversify, `Count id` equals the number of rows the filter admits.
4. **Depth.** `facets_ride_beside_the_ranking`: a cold ranked query with facets is four
   rounds deep at one segment and at four, folded and with unfolded rows, and its blob
   requests equal the ranked query's plus the same aggregation's, each measured separately.
5. **Consistency.** `facets_honour_the_consistency_level`: `strong` and `session` refuse as
   the aggregation alone does; `bounded` reports staleness.
6. **Refusals.** `facets_are_refused_at_the_door`: every `400` of rule 6, `null` accepted,
   `top_k` 10,000 accepted and 10,001 refused.
7. **Gates:** `./scripts/gates.sh` passes; the mutation sweep of the changed lines misses 0,
   or each miss is killed by a named test or is equivalent.

## Test plan

| AC | Seen red first under |
|---|---|
| 1-3, 6 | `facets` absent (the field unknown) |
| 4 | facets awaited after the ranking (depth five) |
| 5 | the aggregation served at a fixed level |
| 3 | facets computed over the ranked rows |

## RA budget

Depth four, unchanged. Requests: the ranked query's plus one aggregation's -- HEAD, open
round, admitted blocks. CPU: the aggregation's, on the same visitor.

## Tasks

- **M65.1** parse `facets` through M12's parser; run the aggregation concurrently; the
  response and `meta.facets_epoch`; the docs; AC1-AC7.
