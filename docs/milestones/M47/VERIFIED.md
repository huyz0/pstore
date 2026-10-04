# M47 — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

**This milestone changes no code.** It restates D-34 where it is written, by the project
owner's decision of 2026-10-04, given in answer to a direct question that offered building
ids beside codes, a per-index opt-in, leaving the row open, or this. ⚠️ It weakens the written
bound, by decision: no check was made to pass, and no test changed.

1. **Regression guard: every bound holds, unchanged.** The seven tests in the spec's table run
   in `./scripts/gates.sh` (`cargo test --workspace`), which was green by the pre-commit hook on
   this commit. Among them, `a_query_costs_four_round_trips_however_many_segments_it_has`
   still asserts exactly 4 at 1, 4 and 8 segments, and
   `a_cold_query_from_head_costs_three_round_trips` asserts 3. No test file's assertion was
   touched; `depth.rs`, `invariants.rs` and `end_to_end.rs` changed comments only.
2. **Layer 0 names both halves and the exception.** `grep -n "three sequential round trips to rank" AGENTS.md`
   matches line 114, `one more to fetch` matches line 114, and `rerank: exact` matches line 115.
3. **Every corpus place carries the banner.** `grep -L "D-34 restated by M47"` over the eight
   files the spec lists prints nothing. `roadmap.md` has it in two places: below the M1 exit
   line, and in the cross-cutting row.
4. **No stale statement survives in code.** `grep -rn` over `crates/` for each of the six
   phrases the spec lists matches 0 lines. Before the change they matched 2, 1, 2, 1, 1 and 1.
5. **The objectives gate holds.** `./scripts/gates.sh` runs `check-slos.py`, `check-links.sh`
   and `build-index.py --check`, green on this commit. `slos.md`'s rows now read "ranking
   depth ≤ 3" and "ranked and fetched: D-34 as restated, 4".

**Left as written, because "three" there means the ranking, which is unchanged:**
- `architecture.md`'s "Reads are budgeted at three sequential blob round trips, which is what
  forces a clustered vector index". It is the index-selection argument.
- INDEX.md's conclusion 2, "~3 sequential fetches … disqualifies graph ANN indexes". The same
  argument.
- `full-text-search.md`'s C-11, "a cold multi-term query stays inside three sequential round
  trips". That is a text ranking.
- `docs/deploy.md:71`, which is about warm depth.

**The latency arithmetic is exceeded, and recorded:** four cold rounds at ~30 ms is ~120 ms,
one round over the ~100 ms the corpus derived three from, and inside `query-path.md`'s own
"~100–150 ms cold".

**Residue (the spec's list):** no HEAD-to-response depth test bounds text, sparse or hybrid
queries through the API, `as_of` relevance queries through the API, `rerank: exact` through
the API (which would be five), or a ranked-and-fetched query over an exact-scan segment. Each
is bounded in pieces, or at the engine, as the spec's table says.
