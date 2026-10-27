# M146: a hard per-group limit on a relevance ranking

## Serves
Workstream F of [the delivery plan](../../comparisons/turbopuffer-next-plan.md): "Hard
`limit.per` counts by the specified key; MMR cannot substitute for it", and the gap the
[comparison](../../comparisons/turbopuffer-2026-10-25.md) names: diversify "does not
enforce the hard group quota represented by `limit.per`". Builds on
[M62](../M62/SPEC.md)'s window and [M64](../M64/SPEC.md)'s diversify. D-34's depth budget
is unchanged.

## Delta
Today a relevance query returns its first `top_k` rows; nothing bounds how many share an
attribute value.

After, a relevance query (one with `vector`, `text`, `field` or `exact`) may carry
`limit_per: {"attributes": [a, ...], "limit": m}`, optionally with `"window": w`:
- A row's group is the tuple of its values for `attributes`, 1 to 8 distinct names. An
  absent attribute is a value of its own. Values compare as `group_by` compares them
  (M12): `1` and `1.0` are one value, strings bytewise, arrays element by element.
- The engine ranks `w` candidates, as it does for a boost (M62). Walking them in final
  order (after a boost's rescoring), a row is kept when fewer than `m` rows of its group
  were kept before it; the walk stops at `top_k` kept rows. So the answer is the first
  `top_k` rows of the window's ranking after every group past its `m` is cut, and may be
  shorter than `top_k` when the window runs out.
- `m` is an integer from 1 up. `w` defaults as a boost's window does (twice `top_k`,
  from 100 to 1,000), is from `top_k` to 1,000, and is the one window a boost and the
  quota share: a boost naming another window is refused, as a diversify's is (M64).
- Rows past the window are never seen: the quota is exact over the window, as a boost is.
  The vendor's own `limit.per` has the same depth question; this states its answer.
- A group reads the row's stored attributes from the row round, as a boost does, whatever
  `include_attributes` selects. A highlight marks the kept rows only. `unfolded_hits`
  counts the kept rows an unfolded row answers, as for a boost.
- Accepted with `as_of` (a boost is) and with any legs and fusion a relevance query
  takes; `offset` stays refused on a relevance query, as today. A `top_k` above 1,000
  cannot have a window, so it is refused with `limit_per`.
- In a multi-query each query carries its own; an outer `rerank_by` (M142) merges the
  cut answers, and the merged list is not itself bounded per group.
- Differences from the vendor, named in the migration notes: the field is a top-level
  `limit_per` beside `top_k` rather than `limit.per` inside `limit`, and `limit` 0 is
  refused (a group that may hold no row is a filter).

Refused with `bad_request`: `limit_per` that is not that object (an unknown key, missing
`attributes` or `limit`, no attribute or more than 8, a name twice, a non-string name,
`limit` that is not an integer of at least 1, `window` outside `top_k..=1000` or not an
integer); `limit_per` beside `rank_by`, `aggregate_by` or `diversify` (MMR would reorder
what the quota cut, and the plan says it cannot substitute for it); a boost naming
another window. Each before any blob request.

How: `pstore_query::Key` gains a public constructor from an optional value, the private
`Key::of` itself (floats normalized, absent as `Absent`, arrays element by element); the server parses
`limit_per` in `plan`, widens the legs to the window as for a boost, and cuts the final
rows in `run` with a pure function, `per_group`. Its `unfolded_hits` counts the kept rows,
as a boost's does.

Unchanged: every query without `limit_per`; multi-query, where each query carries its
own; ordered queries and aggregations, which refuse it.

## Acceptance criteria
1. `per_group`, over pseudo-random ranked rows with 1 to 3 group attributes mixing ints,
   floats equal to ints, strings, absent values and arrays, for every `m` from 1 to 4 and
   `top_k` from 1 past the row count, equals a reference written in the test: walk the
   rows, count per group by the reference's own normalization, keep while under `m`, stop
   at `top_k`.
2. Server: a dense query with `limit_per` over a corpus where the plain ranking's first
   `top_k` rows hold more than `m` of one group returns exactly the reference cut of the
   same query run plain with `top_k` equal to the window; no group appears more than `m`
   times; `1` and `1.0` are one group, and absent is a group.
3. With a boost, the cut is applied to the boosted order: equals the reference cut of the
   boosted plain query at the window. A hybrid query (vector and text, RRF) is cut the
   same way, and a query without `g` in `include_attributes` is cut by `g` all the same.
   With unfolded rows, `unfolded_hits` equals the kept rows whose ids were written
   unfolded.
4. Every refusal in the Delta answers `bad_request` and makes no blob request.
5. Depth: a query with `limit_per` is as deep as the same query with a boost of the same
   window, and makes no more blob requests, by `DepthCounting`.
6. A query without `limit_per` answers as before: the existing `boost::`, `diversify::` and
   `composition::` tests pass unedited.

## Test plan
Unit tests beside `per_group` in `crates/pstore-server/src/`; server tests in a new
`crates/pstore-server/tests/it/limit_per.rs`. Red before: 1 fails to compile; 2, 3 and 5
fail (M126 refuses the unknown field); 4's cases pass on the parent for that reason, so
their red is the mutations named below and the rest are guards; 6 is a guard. Mutations:
- the count compared with `<=` (1, 2); the walk not stopping at `top_k` (1, 2);
- groups by the first attribute only (1); `1.0` a group apart from `1` (1, 2);
- the cut applied before the boost (3);
- `limit` of 0 accepted; 9 attributes accepted; a repeated name accepted; `limit_per`
  beside `diversify` or `rank_by` accepted; a window below `top_k` accepted; a boost's
  other window accepted (4);
- groups read from the selected attributes (3); `unfolded_hits` taken from the engine's
  hits with `limit_per` alone (3).

## RA budget
Unchanged: the window is a boost's, so the depth and requests are a boost's (M62). CPU:
one key per window row.

## Risks
- A small window can return fewer than `top_k` rows when few groups dominate; the caller
  widens it, up to 1,000. Stated in the migration notes.

## Tasks
- M146.1: `Key`'s constructor, `per_group`, the parsing, docs.

## Review
Spec review round 1 (pass with minors, all taken): mutations for the two new window rules;
`as_of`, `offset`, `top_k` above 1,000, the attributes a group reads, highlight and
`unfolded_hits` stated, with tests and mutations for the last two; a hybrid case; the
multi-query merge is not bounded per group; the vendor differences named; `Key`'s
constructor described.
