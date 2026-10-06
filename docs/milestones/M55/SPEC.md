# M55 — Text legs split too

**Serves:** [BACKLOG](../BACKLOG.md) row 57, which the project owner asked on 2026-10-06 to
close with the rest of the backlog.

## What is true today

Read from the tree at M54 (`09fb7c3`), nothing measured:

- **[M54](../M54/SPEC.md) splits only dense and sparse legs.** A text leg runs on the
  coordinator over every segment. The coordinator therefore opens every segment's term
  dictionary and reads every segment's postings for the query's terms, wherever the vector
  legs ran.
- **Why:** BM25 scores against statistics summed over every segment the query reads
  (`Stats::merge` in `pstore_query::run`, D-30):
  - `doc_count`;
  - `total_tokens`, for `avgdl`;
  - each query term's `df`.

  A server holding some of the segments sees only theirs. Its IDF would differ, its per-segment
  cut would keep different rows, and the answer would change.
- **The statistics come from the term dictionaries the open round already fetches** (one
  summary per segment, added together). They cost no blob round, but they exist only after the
  open round, and a peer would need them before its leg round.
- **The fresh segment** (unfolded rows, sealed in memory at query time) contributes to them
  too, and only the coordinator has it.
- ⚠️ **A term dictionary that fails to read is dropped from the sum silently** (`maybe()`
  returns `None`; the merge skips the segment). Today that is safe only because the same
  segment's text leg then fails, and the query with it (spec review).
- **Text-only queries and `max` fusion run on one server** (M54 rule 7).

## Delta

**A query with a text leg is split too, in two exchanges with each peer instead of one. First
each peer opens its segments and returns their statistics for the query's terms. Then the
coordinator returns the global sum, and each peer runs every leg over its segments. The
answer is the single-server answer, and the depth is unchanged: the exchange between the two
is a round trip between servers, not a blob round.**

1. **Phase 1, `open`** (`POST /v1/internal/part`, protocol 2):
   - **Sent:** M54's part (segments, legs, now including text legs, the raw filter, the
     full-text schema, `|shadow|`) and every text leg's terms, analysed with the index's
     analyzer.
   - **Done:** the peer opens its segments with the sidecars its legs need, one blob round.
   - **Returned:** their summed statistics (`doc_count`, `total_tokens` and the `df` of the sent
     terms only), and a part id. The peer holds the opened segments under that id (rule 5).
   - **When:** beside the coordinator's own open round, as M54's single exchange is.
   - ⚠️ **Whole or nothing** (spec review): a segment HEAD says has a term dictionary, and whose
     footer has a text field, must yield a summary. Otherwise phase 1 fails and never returns a
     sum one segment short. `pstore_query::summaries` is the one function both sides use for
     this.
2. **Phase 2, `scan`.**
   - The coordinator adds its own segments' statistics (by the same function), the fresh
     segment's, and every peer's, and sends each peer the sum with its part id.
   - The peer runs every leg over its held segments with that sum, exactly as `run` does:
     widened or exhaustive, masked, rid of deleted rows, cut. It returns the hits as M54 does.
3. **The coordinator, for a segment whose legs run elsewhere, opens its footer and delete
   vector only:** no term dictionary and no postings. The shadow check and the row fetch need
   nothing more.
4. **Exact.** The statistics every leg scores with are the sum over every segment the query
   reads, as before. Restricting `df` to the union of every text leg's analysed terms changes
   no score: `search_with` reads only those terms' `df`.
5. **Depth.** HEAD, the open round, the leg round, the row fetch: four blob rounds. The
   statistics exchange between them is a server-to-server round trip, like M54's part call.
   `PSTORE_PEER_TIMEOUT_MS` applies to each phase.
6. **A held part is ephemeral, and safe:**
   - **Holder:** held by the server, not by a tenant's engine, so the engine registry's
     eviction can neither drop a part nor keep an engine alive.
   - **Id:** 128 random bits.
   - **Scan:** must name the same tenant and index the open did. Anything else gets the same
     `410` as a part that is not there, so the status reveals nothing.
   - **Lifetime:** held at most 10 s, and taken out of the holder when its scan starts, so
     eviction can never drop a part being scanned.
   - **Budget:** at most 256 parts and 256 MiB of opened bytes per server, the oldest dropped
     first. A single part larger than the budget is refused: phase 1 fails and the
     coordinator runs that share, rather than the part evicting every other.
   - A peer whose phase 1 fails partway holds nothing.
   - **Counted:** a part dropped unscanned is counted in `pstore_peer_parts_expired`.
   - Nothing about a part outlives its query, and no server owns a segment. A `bounded` retry
     from a fresh HEAD leaves its first try's parts to expire: intended.
7. **A failure costs rounds, never answers**, and every share the coordinator runs itself is
   scored with the global statistics and the index's schema, never the defaults:
   - **A share that fails phase 1:** the coordinator opens those segments itself, one more
     round, so the sum stays whole. It then scans them itself.
   - **A share that fails phase 2:** the coordinator runs it itself, with the sum it already
     has: open and legs, two more rounds.
   - Failures are counted in `pstore_peer_parts_failed` once per part. A phase-2 `422` is the
     client's and is not counted, as in M54.
   - ⚠️ Both are exceptions to D-34, on failure only. The worst case is two timeouts and two
     rounds.
8. **Split now:** a query with a text leg, alone or beside vector legs; text-only queries; `max`
   fusion.
9. **Still not split:**
   - `sum` fusion. Its legs are whole, so a peer's reply would be every matching row of every
     segment. A per-segment cut that keeps it exact exists, and is left to the backlog.
   - `order_by`, aggregations, `as_of`.
   - An index of one segment, and a share past 4,096 segments.
10. **A vector-only query keeps M54's single exchange at protocol 1**, which a server of either
    build accepts. Only a query with a text leg needs protocol 2, and a peer of M54's build
    refuses it with `409`, so the coordinator runs that share itself.
11. **Counting:** `pstore_peer_parts_sent` and `pstore_peer_parts_served` count once per part,
    whether it took one exchange or two.

**Not changed:** the format, HEAD, the write path, vector-only queries, the API, and every
single-server answer.

**Not covered, and the ledger says so:**
- **Every peer's phase 1 must return before any phase 2, the coordinator's own text legs
  included.** The slowest peer's open sets when every scan starts. A vector-only query has no
  such barrier.
- A held part is memory on the peer: bounded, but not zero.
- `sum` stays on one server (rule 9, backlog).
- A filter with a hybrid query no longer computes its mask twice (M54's "not covered" item):
  every leg of a segment now runs on one server.

## Acceptance criteria

1. **The answer is the single-server answer, in the engine, and the text legs ran on the
   peers.** `query_split_as` against `query_filtered_as`, hit for hit in score and `$dist` bits.
   - Each share runs through a test `Peers` that records its phases. Every case asserts:
     - a phase-1 and a phase-2 call per share, carrying the text legs;
     - the coordinator's store read no `.tdict` of a segment assigned elsewhere.
   - The corpus has terms whose `df` differs between shares, and unfolded writes that add terms
     only the fresh segment has, so a peer scoring with any other statistics answers
     differently.
   - The cases:
     - text alone;
     - text with a filter;
     - `max`;
     - dense with text by RRF;
     - dense, sparse and text with a filter;
     - a term absent from every segment;
     - an index whose only text is in the fresh segment;
     - a share none of whose segments has text;
     - text under `strong` consistency.

   Test: `a_split_text_query_equals_the_unsplit_one`, in `crates/pstore-engine/tests/split.rs`.
2. **Through the API.** `a_split_query_answers_exactly_as_one_server_does` gains:
   - text only;
   - hybrid with weights and a filter;
   - a multi-query with a text sub-query.

   Each is split, and each equals the unpeered server. On every peer the `.tdict` reads of its
   own segments rise.
3. **Where the text legs ran.** For a split text query, each segment's `.tdict` is read by
   exactly the server `assign` names, coordinated from each server in turn. Test:
   `each_text_segment_is_scanned_once_by_its_server`.
4. **Depth.** At 250 ms a request, with no cache, a split hybrid query finishes under
   4.5 × 250 ms. The test is seen to fail with phase 2 opening again. Test:
   `a_split_text_query_keeps_the_round_trip_budget`.
5. **Failure in either phase, over AC1's corpus**, so a fallback scoring with the wrong
   statistics answers differently. Each answer equals the single-server one, and each failure
   is counted once:
   - a share failing phase 1;
   - a share failing phase 2;
   - a peer whose first `.tdict` read fails: an equal answer, never another ranking.

   Test: `a_failed_text_share_costs_rounds_not_answers`, in the engine. Through the API, a dead
   peer and a failing peer: `a_failed_text_peer_costs_rounds_not_answers`.
6. **A held part is bounded and safe.** A unit test of the server's holder, on an injected
   clock:
   - a part is dropped once scanned;
   - one older than 10 s is gone, and counted as expired;
   - past 256 parts or past the byte budget, the oldest is dropped;
   - a part larger than the budget is refused, and evicts nothing;
   - a part taken for its scan is not evicted by parts admitted while it runs;
   - a scan naming another tenant or index gets the same answer as a missing part;
   - ids differ across 1,000 parts.

   Test: `held_parts_are_bounded_safe_and_dropped`.
7. **Protocol.**
   - A vector-only query sends protocol 1 and makes one exchange per peer, and a server of
     this build accepts a protocol-1 part.
   - A text query against a peer that answers protocol 2 with `409` is run by the coordinator
     and answers equal.
   - Test: `a_vector_query_makes_one_exchange_and_an_old_peer_is_run_here`.
8. **Gates:**
   - `./scripts/gates.sh` passes;
   - the incremental mutation sweep of the changed lines misses 0, run as M54 ran it, over
     the tests that reach each file;
   - every M54 test still passes, or the change to it is named in the ledger. M54's
     `queries_that_cannot_be_split_run_on_one_server` narrows: a text-only query now splits,
     while `sum`, `order_by`, aggregations, `as_of` and one segment still send nothing.
