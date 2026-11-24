# M273: a TCP and UDP pair on one port, found on every OS

## Serves
The `portable` CI job (AGENTS.md's gates table): `peer_http`'s `same_port()` failed once on
`windows-latest` in #69 with `no port free for both TCP and UDP in 64 tries`, green on re-run.
Test infrastructure only; no product behaviour changes, and no decision is reopened.

## Delta
Four copies of one search (`peer_http/tests.rs`, `tests/it/peers.rs`, `tests/it/auth_peers.rs`,
and `tests/split_bench.rs`, which unwraps the UDP bind outright) bind TCP to port 0, try UDP on
the port it was given, and drop the TCP listener when UDP is refused. Windows hands the next
`bind(0)` the port just freed, so all 64 tries can name one port whose UDP half another test's
gossip or QUIC socket holds.

After, one search, `pstore_testkit::ports::same_port()`:
1. Each try's refused socket is **held** until the search ends, so no try is given a port an
   earlier try was refused on.
2. Half the tries bind UDP to port 0 first and TCP to its port, so a port range one protocol
   reserves (Windows' excluded port ranges are per protocol) does not refuse every try.
3. Every caller uses it; the failure message names the last error of each kind.

Unchanged: what any test asserts, and every product socket (`main.rs`'s peer bind).

## Acceptance criteria
1. Against binders that, like Windows, hand out the port just freed and refuse its UDP half,
   the search finds a pair (the old search does not), and never offers a refused port twice.
2. Against binders whose TCP-first tries all fail, the UDP-first half finds a pair.
3. No copy of the old loop remains: `grep -rn "in 64 tries" crates` finds only the helper.
4. The workspace suite passes.

## Test plan
In `crates/pstore-testkit/src/ports.rs`:
1. `a_port_just_freed_is_not_offered_again`: fake binders as criterion 1. Mutation: the
   refused socket dropped at once (the old loop), which retries one port until it gives up.
2. `udp_first_finds_a_pair_when_tcp_first_cannot`. Mutation: the UDP-first half removed.
3. `grep` (criterion 3), and `cargo test --workspace` (criterion 4).

## RA budget
Unchanged: tests only.

## Risks
- The flake is rare (once in seven milestones of CI), so a green Windows run proves little.
  The fake binders are the evidence; the real one is the same search over real sockets.

## Tasks
- M273.1 the spec, the helper and its tests, every caller moved to it.
