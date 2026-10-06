# M54 — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ **Where this ran:** a Linux x86-64 cloud container. The server tests run real servers on
loopback sockets over one in-memory store; the timings in criterion 3 are relative to an
injected wait, never a latency of a real backend.

Commands:
- `cargo test -p pstore-query -p pstore-engine -p pstore-server`: every test passes.
- `cargo test -p pstore-server --test peers`: the 10 tests below.

⚠️ **The code came before its tests here.** The query-layer refactor was drafted while the
spec was in review, so these tests were written after the code they check. Each was then
**seen red under a hand mutation** of that code, named below, as the TDD rule requires of a
test written late.

1. **The answer is the single-server answer, through the API.**
   `a_split_query_answers_exactly_as_one_server_does`: three peered servers and an unpeered
   one share a store with at least 8 clustered segments and deleted rows. The fixture checks
   with `assign` that every server holds a segment. Every query below was asked of each of the
   three, and its results and epoch equal the unpeered server's:
   - dense; dense with a filter; dense with `exact`;
   - hybrid by RRF; hybrid by weighted RRF with a filter;
   - a multi-query.

   `pstore_peer_parts_sent` rose for each, so each was split.
   - Killed by hand: the peers' shares dropped; the peer ignoring the filter; the peer
     ignoring the shadow's size; the coordinator running a remote segment's legs too.
1b. **The answer is the single-server answer, in the engine.**
   `a_split_query_equals_the_unsplit_one` in `crates/pstore-engine/tests/split.rs` compares
   `query_split_as`, each share run by `Engine::part`, against `query_filtered`: hits, score
   and `$dist` bits, ids and attributes, all equal. The cases are dense; dense with a filter;
   sparse; two dense legs; and dense, sparse and text with a filter. The index has deletes
   folded into delete vectors, and unfolded upserts and deletes shadowing every segment. Each
   case sent exactly two parts.
   - Killed by hand by the same mutations as criterion 1. The peer ignoring the shadow's size
     is caught here and not through the API, which has no unfolded rows to shadow.
   - ⚠️ Amended from the spec's query-layer test, as the spec records: real folds build the
     clustered and sparse segments.
2. **Each segment is scanned once, by its assigned server.**
   `each_segment_is_scanned_once_by_its_server`, coordinated from each of the three servers in
   turn:
   - every segment's `.cen` is read exactly once, by the server `assign` names, and by no
     other;
   - the coordinator reads every segment's footer;
   - every server reads at least one table;
   - `pstore_peer_parts_sent` rose by 2 on the coordinator, and `pstore_peer_parts_served` by
     1 on each peer.

   Killed by hand: the served count not kept; `me` as 0. Added after the sweep:
   `a_split_filtered_query_masks_each_segment_on_its_server`, in which a filtered split query
   reads a remote segment holding no answer row once, its footer.
3. **Depth, end to end.** `a_split_query_keeps_the_round_trip_budget`: at 250 ms per request,
   with no cache, a split dense query finishes under 1,125 ms.
   - Killed by hand: the peers called only after the coordinator's own open, the five-round
     chain. The test then fails.
   - Added in code review: `failed_shares_are_run_here_together`. With a dead peer and a
     failing one at 400 ms per request, the query finishes under 2.9 s, six rounds. It failed
     at 2.02 s under a 1.75 s bound against failed shares run one after the other, and the
     wait was then widened to 400 ms, so the two shapes sit 0.8 s apart.
4. **A failed peer.** `a_failed_peer_costs_rounds_not_answers` covers a dead URL and a peer
   whose store fails every read. `a_peer_of_another_protocol_is_refused_and_run_here` covers a
   peer answering `409`, and checks that a real server refuses another protocol version with
   `409`.
   - Every answer equals the unpeered server's.
   - `pstore_peer_parts_failed` rises by exactly 2 per query, or 4 for the two-query
     multi-query.
   - Killed by hand: failures not counted; the version check removed.
4b. **A query error is the client's.** `a_query_error_on_a_peer_is_the_clients`: a query of
   the wrong dimension gets the unpeered server's status and code, after two parts were sent,
   and `pstore_peer_parts_failed` does not move. Killed by hand: `422` counted as a failure.
5. **Assignment.** `assignment_is_stable_balanced_and_moves_only_to_a_new_server` in
   `crates/pstore-engine/tests/split.rs`:
   - the same server whatever the list order, and with a trailing `/`;
   - 4 servers each hold 22–28% of 10,000 keys;
   - a fifth server takes 17–23%, and every moved key moved to it.
6. **The endpoint guards its keys and bounds.** `a_part_outside_its_tenant_is_refused`: eight
   parts are refused with `400`, and the server's store saw no read for any of them:
   - another tenant's key;
   - a `..` component, at two places;
   - a key not ending in `.seg`;
   - a key outside `idx/`;
   - a delete vector of another tenant;
   - a delete vector not ending in `.dv`, one of another segment, and one under a `..`;
   - `MAX_LEGS + 1` legs.

   An index named `a..b`, and the part with its own keys, are admitted. Killed by hand: the
   guard removed. The rule in [SPEC.md](SPEC.md)'s rule 6 that a coordinator does not split
   past 4,096 segments a share is `split_tests::a_query_is_split_only_where_rule_seven_allows`
   in `pstore-engine`, added in code review.
7. **What is not split, is not.** `queries_that_cannot_be_split_run_on_one_server`:
   `order_by`, an aggregation, a text-only query, `sum` fusion, `as_of` and an index of one
   segment each leave `pstore_peer_parts_sent` unchanged. The engine's unit test above adds
   `Sum` and one segment at the engine API.
8. **Configuration.** `peers_are_both_or_neither_and_include_self`: both set and consistent,
   trimmed, with the default and an explicit timeout, is accepted. Nine cases are refused:
   - either variable alone;
   - a self not in the list;
   - a URL named twice;
   - a zero timeout and a non-numeric one;
   - `https://`, a path, and no scheme (added in code review).

   The wire format round-trips bit for bit, and refuses a stray or repeated pair:
   `peers::tests::a_part_survives_the_wire_bit_for_bit`.
9. **Gates.**
   - `./scripts/gates.sh`: green by the pre-commit hook on this commit.
   - `cargo deny check`: licences, bans and sources pass with `reqwest` a direct dependency of
     `pstore-server`. It was already in `Cargo.lock` through the object-store client. ⚠️ Advisories fail
     on `paste` (RUSTSEC-2024-0436) through `foyer`, as at M53: not this change.
   - **Mutation**, as two incremental sweeps of the changed lines (`--no-config --profile
     mutants`, `-j 1`). `./scripts/mutants.sh`'s default runs every test of the workspace per
     mutant, about eight hours for these 149, so each sweep builds and runs the tests that
     reach its files (`-C=--lib -C=--test=…`):
     - `pstore-query/src/run.rs` and `pstore-engine/src/lib.rs`, against the libraries'
       tests, every `pstore-query` test file, and the 15 engine test files that query through
       `run` (`engine_query`, `filters`, `fresh_query`, `upsert`, `sparse`, `dict_skip`,
       `centroid_skip`, `distance`, `cold_depth`, `split`, `time_travel`, `analyzer`, `patch`,
       `invariants`, `failures`): **76 mutants, 4 missed**, 47 caught, 25 unviable;
     - `pstore-server/src/peers.rs` and `lib.rs`, against the server's unit tests and
       `--test peers`: **73 mutants, 5 missed**, 49 caught, 19 unviable.
   - **Each of the 9 misses now has a test that kills it, seen by hand:**
     - `assign`'s tie (`>` to `>=`): a server named twice ties, and goes to the earlier;
     - `shares`' segment count (`<` to `==`, `<=`): one and two segments that another server
       holds, split or not by the count alone;
     - the coordinator's mask computed for every segment: a filtered query reads a remote
       segment holding no answer row once, its footer
       (`a_split_filtered_query_masks_each_segment_on_its_server`);
     - the guard's leg and segment limits (`>` to `>=`, `==`): `MAX_LEGS` and 4,096 admitted,
       one more refused (`peers::tests::a_part_is_bounded_at_its_limits_exactly`);
     - the guard's delete-vector test (`||` to `&&`): another segment's vector, and a `..`
       under this one, each refused alone;
     - `rerank_of`'s `none`: every rerank round-trips by name.
   - Two earlier, abandoned passes over an older tree found 8 more, all fixed:
     - `QueryPeers::me` as 0. Every scan test coordinated from the first server, where `me` is
       0, so `each_segment_is_scanned_once_by_its_server` now coordinates from each server.
     - Six in `assign`'s finaliser. A weaker mix is still stable and balanced, so the hash is
       pinned against an independent model, below. Its last shift moves only bits the
       highest hash never looks at, so it was removed, not tested.
     - `assign`'s `>` as `<`: an argmin, killed by the same pin.
   - The model the pins came from, in Python:

     ```python
     M = (1 << 64) - 1
     def h(url, key):
         x = 0xcbf29ce484222325
         for b in url.rstrip('/').encode() + b'\0' + key.encode():
             x ^= b; x = (x * 0x100000001b3) & M
         x ^= x >> 30; x = (x * 0xbf58476d1ce4e5b9) & M
         x ^= x >> 27; x = (x * 0x94d049bb133111eb) & M
         return x
     def assign(key, servers):
         return max(range(len(servers)), key=lambda i: (h(servers[i], key), -i))
     servers = ["http://10.0.0.%d:8080" % i for i in range(4)]
     keys = ["0001/tnt/1/idx/docs/seg/L0/%020d-0000000000000001.seg" % i for i in range(16)]
     # [assign(k, servers) for k in keys] == [2,3,3,3,3,1,0,3,3,0,0,2,1,3,2,3]
     ```

**Not covered**, as the spec states:
- text legs are not split;
- the peer list is static;
- latency on real machines is not measured;
- `meta.cost` counts the coordinator's requests only;
- the peer call is a node-to-node round trip D-34 does not count.

These are backlog rows 57–59.
