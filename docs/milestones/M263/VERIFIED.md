# M263 verification

The spec was not sent to a separate reviewer before code; the PR's code review covers it.
One change from the first draft of the spec, made before any test was green and stated in
the spec: the extremes of strings, bools, datetimes and uuids were first a change to plain
`Min` and `Max`. M211's own test pins `null` for an attribute holding only strings, and a
client doing arithmetic on `Min` would get a string, so it became an opt-in per aggregate,
`{"non_numeric": true}`, and every existing answer and assertion is unchanged.

How to run the tests: `cargo test -p pstore-server --test it group_expanded::`.

Red runs. Against the parent, AC1 to AC5 are red by construction: it refuses a
three-element aggregate as malformed. Each test was also seen red by the mutations below,
applied by a script, the tests run, and the mutation reverted. No `cargo mutants` run is
claimed for them.

1. `min_max_over_each_scalar_kind`: green over unfolded rows, one segment, a segment beside
   unfolded rows, and two segments holding different extremes. Killed: `MinAny` answering
   the largest other (N1); the merge dropping the other side's others (N2); the option
   ignored by the parser (N5); the smallest kept as the first seen (N8).
2. `min_max_numbers_win_over_other_kinds`: green. Killed: an other preferred over a number
   (N3).
3. `min_max_of_strings_per_group`: green. Killed by N1, N2, N5 and N8.
4. `min_max_ignore_bytes_and_arrays`: green. Killed: bytes admitted (N4).
5. `min_max_options_are_refused_when_malformed`: green, five refusals and no blob request.
   Killed: an options object with extra keys accepted (N9).
6. `min_max_and_avg_equal_a_reference_through_folded_and_unfolded_rows` (M211), assertions
   unchanged: green. Plain `Min` folding the others (N7) and plain `Min` answering them (N6)
   each survived alone, as they must: either alone answers what the parent does. Applied
   together they were killed by this test (`lo` answered `"cheap"`), by AC1's `*_plain`
   assertions and by AC5's `false` case.
7. Gates: `cargo fmt --check` and `cargo clippy --workspace --all-targets --all-features --
   -D warnings` pass. The workspace suite over this
   commit, which is the PR's last for M260 to M263: `cargo test --workspace --no-fail-fast
   -j 3`, 56 test binaries, 2,889 passed, 0 failed, 15 ignored. The two IPv6 tests
   (`ipv6_clients_refuse_malformed_replies_and_release_wire_capacity`,
   `serialization_failures_never_send_partial_requests_or_responses`) are NOT-RUN: this
   container has no IPv6 loopback. `scripts/check-links.sh` and
   `scripts/gen-clients.py --check` pass. `cargo deny check`, coverage and the mutation
   sweep are NOT-RUN here; they are CI's on the PR.
