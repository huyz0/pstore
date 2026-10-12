# M62 — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ **Where this ran:** a Linux x86-64 cloud container with 4 cores. No latency is claimed;
depth, requests and bytes are exact.

Commands:
- `cargo test -p pstore-server --test boost`: 6 tests.
- `cargo test -p pstore-server --test peers a_split_boosted_query_answers_as_one_server_does`
- `cargo test -p pstore-server`: every test passes.

1. **Boosts rescore the window.** `boosts_rescore_the_window`: a text query and a hybrid one,
   each checked against the test's own model (the query unboosted at `top_k = window`,
   rescored in `f64` in term order), ids and scores bit for bit:
   - unfolded, folded, and with folded and unfolded rows mixed;
   - the boost lifts a document from below the top `top_k`;
   - `meta.unfolded_hits` counts the returned unfolded rows, and differs from the unboosted
     query's (code review);
   - the same with `include_attributes` naming none of the boosted attributes, or false;
   - per sub-query in a multi-query, one unboosted;
   - under `as_of` the first fold's epoch.
   Seen red with `boost` absent (an unknown field ignored); with the engine asked for
   `top_k`; with the legs left at `top_k`; with the `as_of` path not widened; and, for
   `unfolded_hits`, with the boosted branch's segment test inverted.
2. **Each function.** `each_boost_function_scores_as_specified`, against hand-written values:
   - `linear` on an int and a float;
   - `saturate`, including a negative `x`;
   - `decay` `exp`, `gauss` and `linear` on an int, a float and a datetime;
   - `eq` on a string, a bool, a float, an int against a float value, an array, and a
     datetime against a string;
   - zero for a missing attribute and for every unread type;
   - an overflowing term contributing 0, and finals clamped to ±`f32::MAX`.
3. **The window bounds the lift.** `the_window_bounds_what_a_boost_can_lift`: a text leg and a
   dense leg alone each lift their sixth document into the top 3 with a window of 10, and not
   with a window of 3. `the_default_window_is_twice_top_k_and_at_least_100` (code review):
   the defaults at `top_k` 10 and 60. Seen red with the legs left at `top_k`.
4. **Refusals.** `boosts_are_refused_at_the_door`: every `400` of rule 6, 29 cases, and the
   limits accepted.
5. **Cost.** `a_boost_costs_its_window`: four rounds, and blob reads and bytes equal to the
   unboosted query at `top_k = window`, at one segment and four, with unfolded rows and
   without. Seen red with the engine asked for `top_k`.
6. **Split.** `a_split_boosted_query_answers_as_one_server_does`: dense, text and hybrid
   boosted queries split on M54's fixture and answer exactly as the unpeered server. Seen red
   with a split part's dense leg cut at 10 on the wire. ⚠️ A fault in `run` itself cannot
   turn it red, because the peered and unpeered servers share `run`; criteria 1, 3 and 5 own
   that.
7. **Gates.**
   - `./scripts/gates.sh`: green by the pre-commit hook on this commit.
   - **Mutation:** `cargo mutants --in-diff` over the changed source lines, `--no-config`,
     against `pstore-server`'s `boost` tests and library tests: **79 mutants, 1 missed**, the
     rest caught or unviable. The sweep was resumed twice after container restarts and a
     time limit (`--iterate`).
   - **The miss:** `pstore-server/src/lib.rs`, the unboosted branch's `h.segment ==
     unfolded_at` as `!=`, in `meta.unfolded_hits`. Code this change only moved. Killed by
     `attributes_come_back_from_unfolded_rows`, `attributes_survive_a_fold_and_time_travel`
     and `distinct_integer_names_do_not_brick_the_fold`, in `tests/attributes.rs`, which the
     sweep did not run. Seen red by hand.

**Code review**, two rounds:
- Round 1, **major, fixed:** `meta.unfolded_hits` of a boosted answer was rebuilt by id and
  tested by nothing. `boosts_rescore_the_window` now mixes folded and unfolded rows and
  asserts the count, which the boost changes.
- Round 1 minors, fixed: the default window pinned by its own test; the missing decay
  shapes on a datetime and a float; a refusal case mislabelled; `boost: null` documented.
- Round 2: pass. Its minor, that a count stuck at 0 might pass, is now asserted away.
- Spec review, before any code, found the legs needed widening too (blocking), non-finite
  scores, `top_k` above the window's cap, an untrue request-count claim, numeric `eq`, the
  sort key, and decay precision: all in the spec as it stands.

**Not covered**, as the spec states:
- a term over a leg's own score, `product` composition, an origin of "now", geo decay (M66);
- ⚠️ a term over an attribute a document lacks or holds as another type adds 0 silently, by
  design: one index's documents need not share attributes.
