# M131 verification

Spec reviewed in two rounds (round 1: one blocking, the per-leg bounded retry under a pin;
three majors; round 2 pass, its minors folded into the test plan). Code reviewed in two
rounds (round 1: one major, no test read inside a pin at another level or index; round 2
pass). Engine tests:
`cargo test -p pstore-engine --test snapshot`; server tests:
`cargo test -p pstore-server --test snapshot`. Red runs: the engine tests ran against a stub
`Engine::pinned` that only ran `f`; the server tests against the server before it pinned.
Mutations below were applied by hand by a script, one at a time, and reverted; no
`cargo mutants` run is claimed for them.

1. `legs_share_one_snapshot` (engine: a ranked, an ordered and an aggregate leg; server: two
   ranked legs and an ordered one, since a multi-query offers no `aggregate_by`): red against
   the stub and the parent (epochs 1, 2, 2, and the rival's row in two legs); green after.
   Mutation "ignore the pin" killed in both suites; "multi-query unpinned" killed (server).
2. `facets_share_the_ranked_snapshot` (server): red against the parent (`facets_epoch` 2
   beside `epoch` 1, the facet count 3); green after. Mutation "facets unpinned" killed.
3. `a_pinned_request_reads_head_once` (engine and server, `eventual` and `strong`): red
   against the stub and the parent (3 HEAD reads); green after (1 HEAD read, and 1 lane-
   registry read for `strong`). Killed by "ignore the pin" and "multi-query unpinned".
4. `mixed_levels_are_unpinned` (server): a regression guard, green on the parent and after,
   as the spec says. Mutation "pin at the first leg's level and serve every level from it"
   killed (the strong leg answered 200 at eventual). It asserts the `not_folded` code, not only
   the 503.
5. `a_reaped_bounded_snapshot_retries_fresh` (engine): red against the stub (2 HEAD reads
   after the cached one); green after (1). Mutations "no retry in `pinned`" and "a leg
   under the pin retries from a fresh HEAD" killed.

Beyond the criteria: `other_levels_and_indexes_bypass_the_pin` (engine, added by code
review round 1): under an eventual pin of `idx`, a strong leg of `idx` is still refused
`NotFolded` and reads of another index and by another engine read their own HEAD (4 HEAD
reads, 1 lane-registry read). A guard of the SPEC's Delta, green on the implementation;
mutations "drop the level term", "drop the index term", "drop the engine term" (added after
code round 2) and "`&&` to `||`" in the pin predicate each turned it red.

Gates, in a cloud container: every workspace test ran under
`scripts/coverage.sh --all-features --no-fail-fast --fail-under-regions 95 --fail-under-lines 95`,
skipping M127's two `[::1]` tests (no IPv6 here): 2,198 passed, 0 failed; regions 95.01%,
lines 96.58%, both floors held. `cargo fmt --check` and
`cargo clippy --all-targets --all-features -- -D warnings` passed.
`scripts/check-poison.sh`, `check-links.sh`, `build-index.py --check`, `check-slos.py`,
`check-milestone-ids.py` and `check-portable.sh` passed; `scripts/recall.sh`,
`scripts/ndcg.sh` and `scripts/depth.sh` passed. NOT-RUN here: `scripts/mutants.sh` and
`cargo deny check` (their tools are not installed in this container). The two
round-1 additions ran after that sweep, with their suites, `fmt` and `clippy`, and passed.
