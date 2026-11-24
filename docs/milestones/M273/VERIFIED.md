# M273 verification

The spec was not sent to a separate reviewer before code; the PR's code review covers it.
Tests: `cargo test -p pstore-testkit --lib ports` (4 passed).

Red runs, each a mutation applied by a script and reverted:
- The refused socket dropped at once (`drop(a)` for `held.push(a)`, the old loop):
  `a_port_just_freed_is_not_offered_again` and `udp_first_finds_a_pair_when_tcp_first_cannot`
  failed.
- The UDP-first half removed: `udp_first_finds_a_pair_when_tcp_first_cannot` and
  `no_pair_names_both_errors` failed.

1. `a_port_just_freed_is_not_offered_again`: green. Fake binders hand out the lowest free
   port, as Windows hands out the port just freed, and UDP refuses 1000: the pair is 1001, and
   1000 was offered once.
2. `udp_first_finds_a_pair_when_tcp_first_cannot`: green. UDP refuses every port TCP hands
   out: all 32 TCP-first tries are made on 32 distinct ports, then UDP first finds 5000.
   `no_pair_names_both_errors` checks the failure names both halves' last errors, and
   `the_real_search_finds_a_pair` runs the search over real loopback sockets.
3. `grep -rn "in 64 tries" crates` finds only `crates/pstore-testkit/src/ports.rs` (its own
   test of the message). The four searches (`peer_http/tests.rs`, `tests/it/peers.rs`,
   `tests/it/auth_peers.rs`, `tests/split_bench.rs`) call the shared helper.
4. `cargo test -p pstore-server --no-fail-fast`: every binary green but the lib's two
   `[::1]` tests (`ipv6_clients_refuse_malformed_replies_and_release_wire_capacity`,
   `serialization_failures_never_send_partial_requests_or_responses`), NOT-RUN in effect:
   this container has no IPv6 loopback, as in M267 to M272. `cargo clippy -p pstore-testkit
   -p pstore-server --all-targets -- -D warnings` passes. The Windows job is CI's; one green
   run there would prove little, since the flake was once in seven milestones.
