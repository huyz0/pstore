# M164: aggregations and facets inside a multi-query

## Serves
Workstream F of [the delivery plan](../../comparisons/turbopuffer-next-plan.md): "Unary
Count, attribute Sum, grouped counts ... work in multi", "All subqueries bind one snapshot,
including ranking, aggregations and facets", and "Rank-window limits do not accidentally
truncate aggregate input". D-34's depth budget is unchanged.

Not served here: named `ForEachUnique` groups and their rule that duplicate elements in
one array count once. They are a new grouping, the plan's "expanded grouping", and stay
open in DELIVERY.md for a later milestone.

## Delta
Today a sub-query of `{"queries": [...]}` (M9g) carrying `aggregate_by` or `facets` is
refused ("aggregate_by inside a multi-query is not offered", "facets inside a multi-query
is not offered").

After, a sub-query may be an aggregation (M12: `aggregate_by`, with `group_by`, `filters`,
`top_k`, as a query alone) or a ranked query with `facets` (M65). Every limit and refusal
is the one the same query gets alone, with the same message.

The response keeps its shape for a multi-query without either: `results`, one list per
sub-query, and `meta`, byte for byte as before. When at least one sub-query is an
aggregation, the root also carries, one entry per sub-query in request order:
- `aggregations`: the sub-query's `aggregations` object when it is an aggregation without
  `group_by`, else `null`;
- `aggregation_groups`: its groups when it is an aggregation with `group_by`, else `null`.

When at least one sub-query has `facets`, the root carries `facets`, the sub-query's
facets or `null`, and `meta` carries `facets_epochs`, the epoch its facets were counted at
or `null`. An aggregation sub-query's entry in `results` is `[]`, as a single
aggregation's is; its `meta.unfolded_hits` entry is the count of unfolded rows it counted,
as a single aggregation's is. Each entry equals what the same sub-query answers alone at
the same snapshot.

Snapshot (M131): a multi-query's aggregations and facets run inside the same pinned
snapshot as its ranked legs, so a fold that lands while they run is in none of them.
Legs at mixed levels or with `as_of` run apart, as M131 already states; even then a
sub-query's ranking and its own facets share one snapshot, as they do for that query alone
(M131): the pair is pinned on its own when the request as a whole is not.

Windows: an aggregation sub-query counts its whole filtered set. Another sub-query's
`top_k`, `offset`, boost or group window, and the outer `rerank_by`'s `limit` and
`offset` (M142), do not apply to it; its own `top_k` bounds groups, as M12's does. Facets
count their sub-query's filtered set, not its page (M65), and an outer fusion leaves them
per sub-query.

Refused with `bad_request`, before any blob request: an aggregation sub-query beside an
outer `rerank_by` (fusion ranks rows; an aggregation has none). Facets beside an outer
`rerank_by` are accepted: `results` is the one fused list, and `facets` keeps one entry per
sub-query.

How: `multi_query` stops refusing the two fields. The handler derives each sub-query's
facet query with `facets::derive`, then runs every sub-query, and each facet query beside
its own, concurrently inside the one closure M131 pins; when the request is not pinned, each
faceted sub-query's pair is pinned on its own, as `query_index` pins a faceted query.
Facet queries are derived, and every refusal found, before any blob request. The new root fields are `Option`
vectors skipped when `None`.

Unchanged: a query alone; a multi-query without aggregations or facets; M142's fusion.

## Acceptance criteria
1. Server, over a corpus of 40 rows with a number `n` and a string `tag`, a multi-query of
   a dense query (`top_k` 3, a filter), an aggregation `{"c": ["Count", "id"], "s":
   ["Sum", "n"]}` with a filter, an aggregation counting by `group_by: ["tag"]`, and a
   text query with `facets` grouped by `tag`: each entry of `results`, `aggregations`,
   `aggregation_groups` and `facets` equals the same sub-query's answer alone (`null`
   where the answer alone has no such field), and the counts and sums equal a reference
   computed from the rows written.
2. Windows: beside a ranked sub-query at `top_k` 2 and `offset` 1, an ungrouped
   aggregation at its own `top_k` 1 counts every row its filter keeps (30, and 40
   unfiltered), and a `group_by: ["tag"]` aggregation (4 tags) at `top_k` 4 returns all 4
   groups, 10 rows each. Facets grouped by `tag` beside an outer `rerank_by` with `limit`
   1 return all 4 groups and count every row their filter keeps; `results` is then one
   list of one row.
3. Snapshot: with a rival fold armed to land at the request's first HEAD read, a
   multi-query of a ranked query, an aggregation and a ranked query with facets answers
   every entry from epoch 1: the count is 2, the facets count 2, `meta.epochs` and the
   non-null `meta.facets_epochs` are all 1, and the folded row is in no list.
4. Shape: a multi-query without aggregations or facets has no `aggregations`,
   `aggregation_groups`, `facets` or `meta.facets_epochs`; with only an aggregation it has
   `aggregations` and `aggregation_groups` but no `facets`; with only facets it has
   `facets` and `meta.facets_epochs` but neither aggregation field. An aggregation
   sub-query's `results` entry is `[]`; its `unfolded_hits` entry equals its own alone
   (criterion 1 compares them).
5. Depth: on a cold server each, a multi-query of a ranked query, an aggregation and a
   faceted ranked query is no deeper than the deepest of its sub-queries run alone, and is
   measured (depth above 0).
6. Refusals, each `bad_request` with no blob request: an aggregation sub-query beside
   `rerank_by: ["RRF"]`; inside a sub-query, `group_by` without `aggregate_by`, `Sum` of
   `id`, `aggregate_by` beside `vector`, facets without a ranking leg, and facets beside
   `rank_by`, each naming what its single-query refusal names.
7. Every existing test passes unedited, except two cases that assert the refusal this
   milestone removes: `aggregate.rs`'s `{"queries": [count()]}` and `facets.rs`'s
   `{"queries": [{"vector": Q, "facets": ...}]}`. Each is removed from its refusal list and
   the acceptance it becomes is asserted by criteria 1 and 4; VERIFIED.md names both.
8. Mixed levels: with a rival fold armed to land at the first HEAD read, a multi-query
   that cannot be pinned as a whole (one sub-query reads `as_of`) answers its faceted
   sub-query's hits and facets from one epoch: `meta.facets_epochs[i]` equals
   `meta.epochs[i]`, and the facet count equals the rows that epoch holds.

## Test plan
Server tests in a new `crates/pstore-server/tests/it/multi_aggregate.rs`, and criteria 3
and 8 in `snapshot.rs` beside M131's. Red before: 1 to 5 and 8 fail (the parent refuses
both fields). Of 6's cases, the parent refuses each, but with its generic "inside a
multi-query is not offered" for `Sum` of `id`, `aggregate_by` beside `vector`, both facets
cases and the `rerank_by` case, so those are red by message; `group_by` without
`aggregate_by` already gets the single query's message and is a regression guard.
Mutations, each to be killed:
- an aggregation's `aggregations` put in `aggregation_groups`, or the reverse (1);
- a facet query run outside the pinned closure (3);
- facets run after the ranked legs rather than beside them (5);
- the outer limit applied to facets, or the aggregation run at another sub-query's
  `top_k` (2);
- the new fields serialized when `None` (4); the rerank refusal removed (6);
- the per-pair pin removed on the unpinned path (8);
- `facets::derive` called inside the run rather than before it (6, by the blob count).
If the depth mutation survives because the ranked leg warms every blob the facet query
reads, VERIFIED.md says so and criterion 5 stands as a guard only.

## RA budget
Unchanged depth: every sub-query and facet query runs concurrently, at one snapshot, so a
request's depth is its deepest leg's. Requests and CPU are the sum of its sub-queries', as
for any multi-query, and remain bounded by `MAX_QUERIES` (16) sub-queries, each at most one
facet query. Admission (M128) is unchanged: it admits a request, not its legs, as it
already does for a faceted query and for a multi-query; reserving per leg stays the plan's
open budget item.

## Risks
- A client that reads `results[i]` as rows gets `[]` for an aggregation sub-query; it must
  read `aggregations[i]` or `aggregation_groups[i]`. The migration notes say so.

## Tasks
- M164.1: the handler, the response fields, the refusal, tests, docs. DELIVERY.md's
  C/F snapshot row and F aggregate row name M164, with `ForEachUnique` still pending.

## Review
Code review round 1 (fail on one major, fixed: VERIFIED.md was missing). Minors taken:
criterion 8 asserts the epoch and count agree whichever leg takes the armed read;
criterion 8 moved after 7; the migration note names the one multi-only refusal.
Spec review round 1 (block on two majors, amended): an aggregation's `unfolded_hits` is
its own count, not 0; a `facets_epochs` mutation was equivalent under a pin, so the pair is
now pinned on its own on the unpinned path, with criterion 8. Minors taken: the red of
each criterion 6 case; criterion 2 names 4 groups at `top_k` 4; the depth mutation may be
equivalent; admission stated; DELIVERY rows named.
