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
9. **Gates**
   - `./scripts/mutants.sh --check . --in-diff <the M11.1 source diff>`: **85 tested in 2h, 70
     caught, 12 unviable, 3 missed.**
     - Two misses were in `unfolded_next` (it returned `None`, and its `>` was flipped). Code
       review had named that gap. `clearing_overflow_keeps_the_readers_own_unfolded_writes` was
       added for it while the sweep ran, on a tree the sweep had already copied; the re-check is
       under M11.2's gates.
     - The third was `|` for `^` in the token decoder's byte folding. It is equivalent, because
       the shifted accumulator's low byte is zero. The decoder now uses `from_le_bytes`, which
       has no such operator.
   - Spec review: two rounds.
     - Round 1 blocked on a durable write whose rows a concurrent flush had written. Its token
       got no entry.
     - Round 2 passed.
   - Code review: one round, pass. It raised one major and four minors.
     - The major was the untested own-lane entry on overflow; it was fixed with the test above.
     - Two minors were fixed: the multi-query test now puts `session` second and asserts the
       merged epoch, and `tenant_of`'s misplaced warning was moved back.
     - The "ahead" refusal is now stated as `session`-only in the spec.
     - Minor, not taken: the decoder accepts unknown flag bits, and an overflowed token that
       still carries entries.
   - `bad_session` was added to the counted refusal codes.
   - ⚠️ Two existing tests changed, both in `crates/pstore-server/tests/consistency.rs`:
     - `what_consistency_cannot_mean_is_refused` listed `"session"` as meaningless, and M11.1
       gives it a meaning. `"Session"` takes its place, so the list refuses as many values.
       M11.2 did the same with `"bounded"` and `"Bounded"`.
     - `the_research_is_corrected_where_it_promised_otherwise` pinned the parity doc's
       "are **deferred** (BACKLOG)", and that is no longer true. It now pins "landed in
       [M11.1]".

## M11.2 — `bounded`

Every test below is in `cargo test -p pstore-server --test bounded`, unless another command
is named. **Observed red:** all seven failed against the M11.1 source, with M11.2's engine and
server changes stashed. Six failed because `bounded` was refused. The refusal test failed
because `max_staleness_ms` without `bounded` was accepted.

1. **A hit skips HEAD** — `a_hit_skips_head_and_says_how_stale_it_is`. For a relevance query
   and a rank_by order:
   - exactly 1 read fewer than the same eventual query;
   - lower depth under `DepthCounting`;
   - `staleness_ms` of 1000 after a paused advance of 1000 ms;
   - the same ids.
2. **A miss reads it** — `a_miss_past_the_bound_reads_head`: past the bound, eventual's reads
   and `staleness_ms` 0. The miss refills the cache, and the next read within the bound is a
   hit.
3. **Stale, and says so** — `a_hit_is_stale_and_says_so`: another process's folded write is
   missing from a hit, which reports the cached epoch; past the bound, it is answered.
4. **Own writes stay visible** — `a_hit_still_answers_this_processs_own_writes`: this process's
   batched write, then its durable write and fold, each within the bound.
   - 4b: `its_own_commit_ends_a_hit`. A drop through the same process makes the next bounded
     read 404. With the commit check removed by hand, it failed.
   - 4c: `a_hit_on_a_reaped_segment_falls_back_to_a_fresh_read`, which covers both query
     shapes. Another process drops and runs gc with retention 0, and the bounded read answers
     404. With either fallback removed by hand, it answered `500 internal` ("no such key …
     .seg").
5. **Refusals** — `what_bounded_cannot_mean_is_refused`: without a bound, a bound without
   bounded, -1, "5", 1.5, null, 3,600,001, and with as_of.
6. **Gates** — see the M11 commit.
