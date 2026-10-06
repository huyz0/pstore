# M55 — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ **Where this ran:** a Linux x86-64 cloud container. The server tests run real servers on
loopback sockets over one in-memory store; the timing in criterion 4 is relative to an
injected wait, never a latency of a real backend.

Commands:
- `cargo test -p pstore-query -p pstore-engine -p pstore-server`: every test passes.
- `cargo test -p pstore-engine --test split`: 6 tests; `cargo test -p pstore-server --test
  peers`: 16.

⚠️ **The code came before its tests here, as at M54.** Each test below was then **seen red
under a hand mutation** of the code it checks, named on its line.

1. **The answer is the single-server answer, in the engine.**
   `a_split_text_query_equals_the_unsplit_one` in `crates/pstore-engine/tests/split.rs`
   compares `query_split_as` against `query_filtered_as`: hits, score and `$dist` bits. The
   corpus has `alpha` in the first two segments only, deletes folded into delete vectors, and
   unfolded rows with `zeta`, a term only the fresh segment has. Every case asserts a phase-1
   and a phase-2 call per share carrying text legs, no one-exchange part, and that the
   coordinator read no `.tdict` of a segment `assign` gives another server. The cases: text;
   text with a filter; `max`; dense with text by RRF; dense, sparse and text with a filter; an
   absent term; an index whose only text is in the fresh segment; a share with no text (text
   in the first segment only, so at least one peer's share has none); text under `strong`.
   - Every phase 2 is sent `df` for the query's terms only: the recording peer counts any
     other term it receives, and the count must be 0.
   - Killed by hand: the peers scoring with default statistics; the peers' statistics not
     added to the sum; the coordinator's whole vocabulary sent in phase 2 (19 stray terms).
   - ⚠️ **Found in code review:** phase 2 carried every term of the coordinator's own segments,
     a body growing with records, past axum's 2 MB limit on a real index. Every text query
     would have failed every share and run it here. `shares` now restricts the sum to the
     query's terms (`pstore_query::only`) for every peer implementation.
   - ⚠️ The "share with no text" case was added at the ledger, after the others. It passes;
     no hand mutation was found that it catches and the fresh-only case does not.
2. **Through the API.** `a_split_query_answers_exactly_as_one_server_does` gains text alone and
   text with a filter; its hybrid case with weights and a filter, and its multi-query whose
   second sub-query has a text leg, now split their text legs too. Each was asked of each of
   the three servers, rose `pstore_peer_parts_sent`, left `pstore_peer_parts_failed` at 0, and
   equals the unpeered server. ⚠️ It does not assert the peers' `.tdict` reads, as the spec
   asked; criterion 3's test does, for a hybrid query, from each coordinator.
3. **Where the text legs ran.** `each_text_segment_is_scanned_once_by_its_server`: coordinated
   from each server in turn, every segment's `.tdict` is read exactly once, by the server
   `assign` names, and every server reads at least one.
   - Killed by hand: the coordinator scanning remote segments' text legs itself.
4. **Depth.** `a_split_text_query_keeps_the_round_trip_budget`: at 250 ms per request, with no
   cache, a split hybrid query finishes under 1,125 ms (4.5 × 250 ms).
   - Killed by hand: phase 2 opening its segments again, a fifth round.
5. **Failure in either phase.** `a_failed_text_share_costs_rounds_not_answers`, in the engine,
   over criterion 1's corpus: a share failing phase 1 (the coordinator opens and scans it), and
   one failing phase 2 (the coordinator runs it with the sum it has). Each answer equals the
   unsplit one. `a_share_whose_term_dictionary_will_not_read_is_never_short`: every peer's
   `.tdict` read fails, phase 1 fails whole, and the answer is still equal.
   - Killed by hand: phase 1 not strict (a share summing without its failed dictionary); the
     phase-2 fallback scoring with default statistics.
   - Through the API, `a_failed_text_peer_costs_rounds_not_answers` (added in code review): a
     dead peer fails phase 1; a peer that opens its part on a real server and answers its scan
     with `410` fails phase 2. For every text and hybrid query, the answer equals the unpeered
     server's and `pstore_peer_parts_failed` rises by exactly 2. Killed by hand: a phase-2
     failure not counted.
   - M54's `a_failed_peer_costs_rounds_not_answers` now runs text alone and text with a filter
     too, against a dead peer and one whose store fails every read: phase-1 failures.
   - ⚠️ Spec rule 1 names `pstore_query::summaries` as the one function both sides use. There
     is none: the strict check is in `open_part`, and the coordinator keeps M54's tolerant merge
     for its own segments and for a share it opens after phase 1 failed. That stays exact: a
     segment the tolerant merge drops has its text leg run here, and that leg then fails the
     query, as before M55.
6. **A held part is bounded and safe.** `peers::tests::held_parts_are_bounded_safe_and_dropped`
   on an injected clock: dropped once scanned; gone after 10 s and counted expired; the oldest
   dropped past 256 parts and past the byte budget; a part larger than the budget refused,
   evicting nothing; a part taken for its scan untouched by later admissions; another tenant
   or index answered as a missing part; 1,000 ids distinct.
   - Killed by hand: no tenant check; an oversize part admitted.
   - `a_part_never_scanned_expires_without_more_traffic` (added in code review): expiry was
     swept only on the next open or scan, so a peer receiving no more phased traffic held an
     abandoned part, uncounted, indefinitely. A `/metrics` scrape now sweeps too. On a paused
     clock: a part opened and never scanned reads as expired 11 s later. Killed by hand: the
     sweep on scrape removed. ⚠️ A peer that is neither scraped nor queried still holds its
     parts until one of those happens; the byte budget bounds them meanwhile.
7. **Protocol.** `a_vector_query_makes_one_exchange_and_an_old_peer_is_run_here`: a vector
   query makes one exchange per peer and a text query two; this build serves a protocol-1
   part; a peer of M54's build has its share run here, and the answer equals.
   - ⚠️ Amended from spec rule 10 (code review): an M54 server decodes a part before checking
     its version, and its legs have no `text` kind, so it answers a text part `400 malformed
     part`, not `409`. The fake peer now answers as the real one does. Any status but 200 and
     422 runs the share here, so the behaviour is as specified.
   The wire format of both phases round-trips bit for bit:
   `peers::tests::a_part_survives_the_wire_bit_for_bit`.
8. **Gates.**
   - `./scripts/gates.sh`: green by the pre-commit hook on this commit.
   - `cargo deny check`: licences, bans and sources pass; no dependency was added. ⚠️ Advisories
     fail on `paste` (RUSTSEC-2024-0436) through `foyer`, as at M53 and M54: not this change.
   - **Mutation**, as M54 ran it: two incremental sweeps of the changed lines (`--no-config
     --profile mutants`, `-j 1`), each building and running the tests that reach its files:
     - `pstore-query/src/run.rs` and `pstore-engine/src/lib.rs`, against the libraries' tests,
       every `pstore-query` test file and the 15 engine test files M54 named: **38 mutants,
       12 missed**, 20 caught, 6 unviable;
     - `pstore-server/src/peers.rs` and `lib.rs`, against the server's unit tests and `--test
       peers`: **73 mutants, 10 missed**, 49 caught, 14 unviable.
   - ⚠️ The sweeps ran over the tree before code review's fixes, so the lines those fixes
     changed were checked by hand mutation (criteria 1, 5 and 6), not swept.
   - **Each of the 22 misses now has a test that kills it, seen by hand:**
     - `OpenPart::bytes`, ten mutants of its arithmetic and its result: the per-segment
       estimate is now `held_bytes`, pinned exactly by
       `run::held_tests::a_held_segment_is_priced_by_what_it_keeps`, and every part held in
       criterion 1's test is checked to be at least 16 KiB a segment and under 1 MiB more;
     - `open_part`'s skip (`||` to `&&`, twice): a part with no text leg, and segments HEAD
       says have no dictionary, each sum nothing and require nothing
       (`a_share_sums_only_what_its_legs_and_head_ask_for`);
     - `HOLD_BYTES` (`*` to `+`, twice): the limits pinned to the spec's
       (`peers::tests::the_holder_keeps_what_the_spec_says`);
     - the counted-once test (`&&` to `||`): a hybrid query of the wrong dimension, whose `422`
       comes back in phase 2, counts nothing (`a_query_error_on_a_peer_is_the_clients`);
     - `Failure::message` (twice): a phase failing says why
       (`peers::tests::a_failed_phase_says_why`);
     - the phase guards (`== PHASED` to `true`, twice): protocol 1 naming a phase is `409`
       (`a_peer_of_another_protocol_is_refused_and_run_here`);
     - the holder's byte budget (`>` to `>=`, twice), and its `Debug`: a part of exactly the
       budget admitted, two halves held together, and what its log shows
       (`peers::tests::the_byte_budget_holds_exactly_its_size`).
   - M54's tests all pass. One narrowed, as the spec says:
     `queries_that_cannot_be_split_run_on_one_server` now asserts that a text-only query
     splits, while `sum`, `order_by`, an aggregation, `as_of` and one segment still send
     nothing. The engine's `split_tests::a_query_is_split_only_where_rule_seven_allows`
     narrows the same way: text is phased, dense is one exchange, `sum` over text is not split.

**Not covered**, as the spec states:
- every phase 1 must return before any phase 2: the slowest peer's open sets when the scans
  start;
- a held part is bounded memory on the peer, not zero;
- `sum` stays on one server (backlog row 60);
- latency on real machines is not measured (backlog row 59).
