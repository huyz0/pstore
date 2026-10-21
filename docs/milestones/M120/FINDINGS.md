# M119–M120 — every review finding, and what became of it

The five read-only reviews M119 ran (one a crate group), each finding by its number in its
review, with the commit that closed it and the evidence. **Fixed** means landed and covered
by the gate run on that commit; a row naming a test names one that passes, and the ledgers
record the mutation each went red under first, where one was run. **Rejected** gives the reason. The bench digests
([M119](../M119/VERIFIED.md) criterion 2) held across every commit below.

Commits: M119.2 `7a94c49`, M120 `17e96bb`, M120.2 `4893a5e`, M120.3 (this one).

## `pstore-index`

| # | Finding | Outcome |
|---|---|---|
| 1 | branchy, bounds-checked 1-bit kernel | Fixed, M119.2: `signed_sum`; `the_branchless_kernel_is_the_branchy_one` |
| 2 | centroid table cloned and fully sorted a query; re-decoded a query | Fixed: no clone and top-k probe in M119.2; read in place in M120 (`the_table_in_place_is_the_decoded_one`) |
| 3 | a `Vec` a candidate in `read_code` | Fixed, M119.2: `estimate_residual_raw`, `estimate_prepared_raw` |
| 4 | full sorts for top-k at seven sites | Fixed, M119.2: `top::top_k_by` at each; `top_k_matches_a_full_sort` |
| 5 | SipHash maps over every probed row | Fixed, M119.2: mappings read in place, a bitmap dedupe |
| 6 | SipHash BM25 and sparse accumulators | Fixed, M119.2: `top::Accumulator`; `the_dense_accumulator_is_the_map` |
| 7 | clustering sorts every centroid a row, 12 times | Fixed, M119.2: argmin first, one buffer, `augment` by selection |
| 8 | centroid means column by column | Fixed: `cluster` in M119.2, `lire` in M120 |
| 9 | a build copies every document twice | Fixed, M119.2: rows moved into the writer, fitted vectors reused |
| 10 | a `BTreeMap` accumulator in the multi-vector search | Fixed, M119.2: a dense vector, capped as review asked |
| — | `exq`, `lire` carry 1 and 8 | Fixed, M120 |

## `pstore-format`

| # | Finding | Outcome |
|---|---|---|
| 1 | a `String` a term in `TermDict::decode` | Fixed, M119.2: one blob and spans |
| 2 | FDE signs drawn per bucket; planes drawn per document | Fixed: signs once a repetition in M119.2; both kept a thread in M120.3 (`the_draws_kept_are_the_draws_made`) |
| 3 | quadratic trigram dedupe, a `String` a hash; trigrams per block | Fixed: set and stack bytes in M119.2; a fuzzy needle carries its own in M120 |
| 4 | the block-size fit re-encodes every row; body grown by doubling | Fixed, M119.2: zones measured alone, rows sealed once; capacity reserved |
| 5 | a zone key cloned a row | Fixed, M119.2 |
| 6 | a posting key cloned a row | Fixed, M119.2 |
| 7 | external rows as `Vec`s, removed one by one | Fixed, M119.2: `Bytes` slices, walked |
| 8 | the sparse dictionary copied a query | Fixed, M119.2: `decode_bytes` |
| 9 | `rows_at` scans blocks linearly, files every row, clones hits | Fixed: bisection and asked rows only in M119.2; moved, not cloned, in M120.3 (`a_row_asked_twice_comes_back_twice`) |
| 10 | a scan copies field vectors, reads fields one after another | Fixed: moved in M119.2; read together in M120 (`a_multi_field_scan_reads_its_fields_together`). **Rejected:** `first_field.clone()` a row -- the document's map owns its key, so a row needs its own |

## `pstore-query`

| # | Finding | Outcome |
|---|---|---|
| 1 | every segment's whole vocabulary merged a text query | Fixed, M119.2: `summary_for` the query's terms |
| 2 | each term dictionary decoded once a text leg and again for statistics | Fixed, M119.2: once at open |
| 3 | `fuse` sorts every row | Fixed, M119.2: selection over a hash map |
| 4 | exact scans sort every row | Fixed, M119.2 |
| 5 | the filter clones a value a row a leaf | Fixed, M119.2: borrowed |
| 6 | `Segment` deep-cloned a segment a query | Fixed, M119.2: `Arc` |
| 7 | the order-by selector builds a rank before rejecting | Fixed, M119.2 |
| 8 | the aggregator searches twice and clones an `Arc` a row | Fixed, M119.2 |
| 9 | SipHash mask and delete checks a candidate | Fixed, M119.2: a bitmap, a sorted vector |
| — | the mask decodes every row whole (found by the bench) | Fixed, M119.2: projected decode; both `a_projection_*` tests |

## `pstore-engine`, `pstore-blob`, `pstore-cache`

| # | Finding | Outcome |
|---|---|---|
| 1 | one fresh segment a tenant | Fixed, M119.2: one an index; `each_index_keeps_its_own_fresh_segment` |
| 2 | every unfolded row cloned a query, hit or miss | Fixed, M119.2: copied on a miss only; shadow shared |
| 3 | a bounded hit clones HEAD under the mutex | Fixed: `Arc` in M119.2; `scope` borrows in M120 |
| 4 | a coalesced slice pins its whole fetch in the cache | Fixed, M119.2: admitted as a copy |
| 5 | every write rescans every held row | Fixed, M119.2: only for a named field |
| 6 | schemas cloned a query | Fixed, M119.2: compared first |
| 7 | misses placed in O(misses × ranges); a lock a range; a key `String` a range | Fixed: by index in M119.2; one lock a batch in M120.3. **Rejected:** the key -- `HashMap<Id, _>` is looked up by an owned `Id`, and stable `std` has no borrowed-key lookup for a composite key |
| 8 | a tokio mutex for a synchronous section | Fixed, M119.2: `std`, poison recovered |
| 9 | `scoped` allocates a `String` a byte | Fixed, M119.2 |

## `pstore-server`, `pstore-node`, `pstore-catalog`, `pstore-gossip`, `pstore-meter`

| # | Finding | Outcome |
|---|---|---|
| 1 | a write body parsed into a tree, then the type | Fixed, M120: once where the tree agrees; `the_fast_parse_is_the_tree_parse` |
| 2 | a peer part the same | Fixed, M120 and M120.2: `the_fast_part_is_the_tree_part` |
| 3 | a query the same; a multi-query clones each sub-query | Fixed: moved in M119.2; once in M120; a multi-query told apart first in M120.2 |
| 4 | an untagged `VectorIn` buffers every vector | Fixed, M119.2: by hand; `the_hand_written_vector_is_the_derived_one` |
| 5 | a patch through two JSON copies; rows and cells cloned | Fixed: built directly in M119.2; moved out of the request in M120.3 |
| 6 | `to_document` copies every vector | Fixed, M119.2: by value |
| 7 | an eviction pass locks once an engine | Fixed, M119.2 |
| 8 | the external list copied a request | Fixed, M119.2 |
| 9 | the meter copies every range | Fixed, M119.2: sliced |
| — | `swim::dest_for` scans members per datagram | **Rejected by the review itself:** about 50 µs at 10,000 members, a few times a tick |

The two independent reviews' findings are in [M119's](../M119/VERIFIED.md) and
[this milestone's](VERIFIED.md) ledgers: the first's two majors are M120's Deltas 1 and 2,
its minors fixed in M119.2; the second's minors are M120.2.
