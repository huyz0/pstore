# M154: splitting a query that scores a further text field

⚠️ **Not ready to start.** This is the scope [M152](../M152/SPEC.md) split out after its
second spec review. It names the change and the criteria it must meet. It is written in full,
and reviewed, before any code.

## Serves
Workstream E of [the delivery plan](../../comparisons/turbopuffer-next-plan.md) ("multiple
string/string-array BM25 fields"). M152 answers a query over a further text field correctly
by never splitting it. This milestone lets such a query be split, so a further field costs
what the primary costs in distribution.

## Delta (to be written in full)
- A share's `Part` carries two `FullText`s: the filter's (always the index's `fts`, which binds
  token predicates) and the scoring one (the queried field's).
- That is a wire change for phased and summed parts alike. Each gets a new protocol number
  beside `PROTOCOL`, `PHASED` and `SUMMED` in `crates/pstore-server/src/peers.rs`, and both
  `serve_open` and `serve_scan` accept the new numbers.
- A peer that does not know a number answers 409, and the coordinator runs that part itself,
  as today.
- A query over the primary keeps today's protocol numbers and bytes.

## Acceptance criteria (to be made checkable)
1. A query over a further field, through phased shares, is split and answers exactly as on an
   engine with no peers.
2. The same with a token filter over the primary.
3. The same through summed shares.
4. Against a peer that answers the new protocol with 409, the query still answers exactly, run
   by the coordinator.
5. A query over the primary sends today's protocol numbers and bytes.
