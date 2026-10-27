# M140: order by up to eight attributes

## Serves
Workstream F of [the delivery plan](../../comparisons/turbopuffer-next-plan.md): "up to
eight sort keys handle ties/nulls/types and ascending/descending direction", and the
"Up to 8 attribute sort keys | One key only" row of [the
comparison](../../comparisons/turbopuffer-2026-10-25.md). Extends
[M9e](../M9e/SPEC.md)'s order by attribute. D-34's depth budget is unchanged.

## Delta
Today `rank_by` is `[attribute, "asc" | "desc"]`: one key, ties broken by the id.

After, `rank_by` may also be an array of 1 to 8 such pairs,
`[["a", "asc"], ["b", "desc"]]`. Rows are ordered by the first key; rows it ties are
ordered by the second; and so on; rows every key ties are ordered by id, ascending.
- Each key keeps M9e's rules for its own direction: the groups (bools, numbers,
  datetimes, strings, arrays) in order, reversed for `desc`; absent last in both
  directions; numbers one group by exact value; arrays tie among themselves; strings
  bytewise. The per-key comparison no longer breaks ties by id: the id breaks them once,
  after the last key.
- `id` may be a key, but only the last: ids are unique, so a key after it would never be
  read.
- Refused with `bad_request`: no keys, more than 8, an attribute named twice, a key after
  `id`, a pair that is not `[string, "asc" | "desc"]`. The one-pair form is unchanged, and so is every
  other refusal (`rank_by` beside a relevance ranking, `top_k + offset` past 10,000).

How: `pstore_query::OrderBy` stays one key. A new `pstore_query::Order` holds 1 to 8 of
them behind private fields, so every `Order` is valid: built from one (`From<OrderBy>`
and `From<&OrderBy>`), or checked from a list (`Order::new`, whose error is the refusal's
message). The `Selector::new` and the engine's `ordered`/`ordered_as` take
`impl Into<Order>`, so every existing caller, passing an `OrderBy` or a `&OrderBy`,
compiles unchanged. The selector compares a row against its worst kept row key by key,
borrowed, and owns a row's values only when it keeps it, as M119 made one key do. A kept
array value is stored as its group alone, since arrays never compare by content.

Unchanged: the rounds (three, no resolve round), the 10,000 bound, `offset`, results'
shape, and every one-key answer. Not added: a cursor for a multi-key order; it pages by
`offset` alone, within the 10,000 bound.

## Acceptance criteria
1. The selector, over pseudo-random rows mixing ints, floats, strings, bools, datetimes,
   arrays and absent values with many ties, for 1, 2 and 3 keys in every mix of
   directions and caps from 0 to past the row count, keeps exactly a reference sort's
   prefix (the reference: compare key by key with M9e's per-key rule, then id), and never
   holds more than its cap.
2. One key orders exactly as before: every existing test passes unedited, among them
   `pstore-query`'s order tests and the engine's and server's `order::` tests.
3. Engine: an order by two keys over two segments, a delete vector and unfolded rows
   returns the reference order, paged by `offset` and `limit`.
4. Server: `rank_by: [["a","asc"],["b","desc"]]` with `top_k` and `offset` returns the
   reference order's page; `[["a","asc"]]` answers as `["a","asc"]` does.
5. Server refusals, each `bad_request` before any blob request: `[]`, nine pairs,
   `[["a","asc"],["a","desc"]]`, `[["id","asc"],["a","asc"]]`, `[["a"]]`,
   `[["a","up"]]`, `[["a","asc"], "b"]`; and the list form beside `vector`, beside
   `exact`, and with `top_k + offset` past 10,000.

## Test plan
Unit tests in `crates/pstore-query/src/order.rs`; engine
`crates/pstore-engine/tests/it/order.rs`; server a new
`crates/pstore-server/tests/it/order_keys.rs`. Red before: 1 and 3 fail to compile
without `Order`, which counts as red; 4 fails (the list form is refused); 5's list-form
cases pass before, as the parent refuses every list, so their red is the mutations; 2 is
a guard. Mutations, each to be killed:
- only the first key compared (1, 3, 4); keys compared in reverse order (1);
- a later key's direction taken from the first (1);
- the id tie-break dropped or reversed (1);
- the bound of 8 off by one, a repeated attribute or a key after `id` accepted (5);
- the list form returning before the relevance and window checks (5).

## RA budget
Unchanged: HEAD, segments with delete vectors, blocks; no resolve round, no LIST. CPU per
row offered grows with the keys compared, up to 8.

## Risks
- Segment pruning by zone maps is by filter only, as today, so a multi-key order reads
  what a one-key order reads.

## Tasks
- M140.1: `Order`, the selector, the engine's signature, the server's parsing, docs.

## Review
Spec review round 1 (pass with minors; one major on wording, amended): `impl Into<Order>`
with `From<&OrderBy>`, so no existing test is edited; the id breaks ties once, after the
last key; a key after `id` is refused; arrays kept as their group; the list form's
other refusals tested; no cursor for multi-key orders, stated; `Order`'s invariant and
error stated; compile failure counted as red.
