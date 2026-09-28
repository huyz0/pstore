# M11 — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ **Where this ran:** a Linux x86-64 cloud container (4 cores, `cargo-mutants` 27.1.0).

⚠️ **The gate checks M11.1's criteria only.** It reads the first "Acceptance criteria"
section, as it did for M9i. M11.2's lines below restart at 1, so they are enumerated here
and not by the script.

## M11.1 — `session`

Every test below is in `cargo test -p pstore-server --test session`, unless another command
is named. **Observed red:** all eight tests written before the implementation failed, each
because no write returned an `x-pstore-session` header. The two added after spec review
(1b and 2b) were written against the implementation and seen red by hand mutation, as named.

1. **Read-your-writes across processes** —
   `a_session_reads_its_own_durable_write_through_another_process`:
   - through B, `503 not_folded` with `Retry-After`;
   - B's own fold tick then folds the requested tenant;
   - the same token is then served, with `meta.session` equal to the header.

   `eventual` through B answers 404 before the fold.
   - ⚠️ The draft expected an empty 200 there. To B the index does not exist yet, so the
     expectation was wrong and was corrected; the criterion is unchanged.
   - 1b: `each_of_many_concurrent_durable_writes_is_required_of_another_process`. Twelve
     concurrent durable writes over a store whose bundle PUTs take 30 ms, and each token is
     refused through B until a fold. With the entry minted from the request's own flush
     return instead of the engine's next sequence, it failed on 3 of 3 runs.
2. **Own process, no fold** — `a_session_reads_its_own_write_on_its_own_process_without_a_fold`.
   2b: `a_token_is_not_trusted_by_a_restarted_process_on_its_lane`. It is refused before the
   new process writes, refused after it resumes past the token, and served after a fold.
3. **Monotonic** — `a_session_never_goes_backwards_and_its_token_shrinks`:
   - the returned epoch is at least the sent one, and equals `meta.epoch`;
   - a folded token comes back 27 bytes long;
   - a token whose epoch is past HEAD is `400 bad_session`.
4. **Cost** — `a_covered_session_read_costs_what_eventual_does`: `meta.cost` is identical.
5. **Overflow** — `a_session_across_seventeen_lanes_overflows_into_strong`:
   - 17 writers chained into one token set overflow and drop every entry;
   - a reader on an 18th lane is refused, then served after a fold with overflow cleared.

   `the_seventeenth_lane_overflows_and_overflow_is_sticky` and
   `a_merge_takes_the_max_of_everything` (`cargo test -p pstore-server --lib session`) pin
   the merge.
6. **Batched writes** — `a_batched_write_is_not_required_of_another_process`:
   - the token has no entry, and its own process answers the write;
   - B answers as it would with no token: 404, not refused. The draft's empty 200 was
     corrected as in criterion 1.
7. **Refusals** — `a_bad_session_is_refused`. Each of these is `400 bad_session`: not
   base64url, version 2, one byte short, another tenant, 17 entries, and an epoch ahead of the
   store. `session` with `as_of` and `"Session"` are each 400.
   `a_token_round_trips_at_every_size_it_can_hold` covers 0 to 16 entries at extreme values.
8. **Multi-query** — `a_multi_query_checks_its_session_subqueries`: refused, then served with
   one merged 27-byte token.
   - ⚠️ The first implementation merged the request's token back into the sub-queries', which
     restored the entries a fold had covered. It was caught by this test and fixed before
     commit.
9. **Gates** — see the M11 commit: the sweep, reviews and `./scripts/gates.sh`.
   - ⚠️ Two existing tests changed, both in `crates/pstore-server/tests/consistency.rs`:
     - `what_consistency_cannot_mean_is_refused` listed `"session"` as meaningless, and M11.1
       gives it a meaning. `"Session"` takes its place, so the list refuses as many values.
     - `the_research_is_corrected_where_it_promised_otherwise` pinned the parity doc's
       "are **deferred** (BACKLOG)", and that is no longer true. It now pins "landed in
       [M11.1]".
