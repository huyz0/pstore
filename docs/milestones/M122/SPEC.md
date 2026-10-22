# M122 — M121's deferred findings

**Serves:** the four findings [M121](../M121/FINDINGS.md) deferred (7, 8, 10, 12), at the
owner's request. No backlog row.

## What is true today

- **Every text query decodes every text segment's term dictionary**: a copy of the term blob,
  a UTF-8 check and a sort check per term, and two vectors the size of the vocabulary. A
  sparse leg walks its dictionary's dimensions the same way. The dictionary is an immutable
  object, and the pinned cache returns the same `Bytes` for it on every hit.
- **A clustered search over replicated rows sorts every candidate** to keep each data row's
  best, then ranks again; and allocates a bitmap the size of the segment.
- **A query copies every unfolded row** of its index out of the fresh cache -- documents,
  vectors and all -- though it only reads them.
- **The server parses some bodies more than it needs:** a branch write a second time, as a
  tree, to count its keys; a peer's part once more to read its protocol and phase.

## Delta

1. A decoded dictionary -- a term dictionary, a sparse one -- is kept, process-wide and
   bounded, **keyed by the buffer it was decoded from**: the entry holds that `Bytes`, so its
   memory cannot be reused while the key names it, and two stores' objects of the same name
   never meet. A different buffer of the same bytes decodes again. A refusal is not kept.
2. Replicated candidates are deduplicated in one pass, each data row keeping its best by the
   order the sort used; no segment-sized bitmap.
3. The fresh cache's rows are shared, `Arc`, with every view and every answer.
4. `parse_write` hands back the top-level key count it already has; `plain` records the
   top-level `protocol` and `phase` a part's dispatch reads, in its own pass.

**Not changed:** any answer, its order, any error a caller sees, any request count. A refused
single query keeps its third pass: the tree's parse is what words its refusal. A
multi-query keeps `plain` then the tree: its sub-queries are refused by the **presence** of
keys (`"aggregate_by": null` refuses), which a typed parse cannot see, so a typed multi-query
would need a probe per sub-query besides -- more passes than it saves.

## Acceptance criteria

1. **Kept, by buffer.** `a_dictionary_is_decoded_once_a_buffer`: the same `Bytes` twice is one
   decoded dictionary (`Arc::ptr_eq`); equal bytes in another buffer are another, answering
   the same; a refused dictionary is refused each time; the budget evicts.
2. **Dedupe.** `the_one_pass_dedupe_is_the_sorted_one`: over random candidates with ties and
   repeated data rows, the set kept equals the sort-and-retain's.
3. **Shared.** `two_queries_share_the_unfolded_rows`: two answers' `unfolded` are one
   allocation.
4. **Parses.** `the_fast_parse_is_the_tree_parse` compares the key count as well, over a
   corpus widened by branch writes; `the_fast_part_is_the_tree_part` passes with the probe
   gone, over a corpus widened by a nested and an escaped `protocol`.
5. M119's bench: every digest unchanged.
6. `./scripts/gates.sh` passes.

## Test plan

| # | Test | Fails first because | Mutation it catches |
|---|---|---|---|
| 1 | `a_dictionary_is_decoded_once_a_buffer` | it does not exist | keyed by length alone |
| 2 | `the_one_pass_dedupe_is_the_sorted_one` | it does not exist | a tie kept by the higher row |
| 3 | `two_queries_share_the_unfolded_rows` | the rows are copied | the view cloned |
| 4 | `the_fast_parse_is_the_tree_parse`, `the_fast_part_is_the_tree_part` | — | a key miscounted; the heading's integer not kept |

## RA budget

No request added or removed. Memory: each of the two dictionary caches is bounded at 32 MiB,
64 MiB in all, counting the buffers it keeps alive.

## Risks

- **A store that returns a fresh buffer per read** never hits: it decodes as today, and the
  cache holds up to its budget of dead buffers. The pinned cache, on every production path,
  returns one.

## Tasks

- **M122.1** — all of it, and the ledger.
