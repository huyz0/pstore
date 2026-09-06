# M4a — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md), naming the test or command that
demonstrated it.

Gate: `scripts/check-verified.py`.

1. `reading_the_roster_costs_one_get_and_no_list`
   (`cargo test -p pstore-cluster --test roster`), asserted against `Accounted`, which counts
   by `OpClass` — so a LIST could not be miscounted as a read.
2. `a_lost_refold_rebases_rather_than_overwriting`, `a_corrupt_roster_is_refused_not_guessed`
   (`--test roster`). CAS on an **observed** tag, never create-if-absent: MinIO ignores
   `If-None-Match: *`, so the one primitive that would have been natural here is the one the
   backend does not have.
3. `a_hundred_concurrent_refolds_are_bounded` (`--test roster`), using `Gated` so the race is
   forced rather than hoped for. 100 nodes refolding together issue ≤210 requests in total.
4. `placement_matches_a_golden_vector_in_a_fresh_process`
   (`cargo test -p pstore-cluster --test placement`), against four placements computed by an
   earlier process and committed. ⚠️ Added late: the milestone had only
   `placement_is_deterministic_across_rosters_built_differently`, which compares two instances
   in **one** process and therefore cannot catch a seed shared by both — the exact failure the
   criterion names. Observed to fail before it passed, by perturbing one committed node.
5. `placement_is_balanced_at_c_thirty_two`, `every_member_owns_roughly_its_share_of_keys`,
   `balance_holds_as_the_fleet_grows` (`--test placement`). 100,000 keys on 100 nodes, no node
   above 1.25× the mean.
6. ⚠️ **The stated bound was a mis-measurement, and is corrected rather than weakened.**
   "≤1.6× the minimum" was pinned from a single sample of a statistic that ranges 1.47–2.10×
   across added-node ids and 1.38–1.95× across hash choices — it was never a property, and
   three rounds were spent "fixing" a hash to hit a number that could not be hit reliably.
   Replaced by two criteria that are stable under both:
   `a_bulk_fleet_change_moves_no_more_than_the_minimum` (bulk change ≤1.5× the floor),
   `a_single_node_addition_moves_almost_nothing` (a +1 node change moves <3% of keys, an
   absolute bound), and `growth_moves_no_less_than_the_new_nodes_share`, which is what stops a
   ring that is never rebuilt from scoring a perfect 0% churn. Recorded as
   [C-6](../../research/04-cluster/routing-and-placement.md).
7. `a_fleet_change_copies_nothing` (`--test placement`). ⚠️ Also added late; the milestone
   had no test for it. `Placement` takes no store, so the absence is structural — the test
   pins that the seam stays absent, and additionally asserts the ring **was** rebuilt, so it
   cannot pass against a placement that never changes and therefore also copies nothing.
8. `an_overloaded_node_sheds_only_its_own_shards` (`--test placement`).
9. `skipping_never_returns_fewer_than_r`, `a_small_fleet_places_on_everyone_it_has`
   (`--test placement`).
10. `./scripts/coverage.sh --fail-under-regions 95` → **95.29% region**, 97.21% line, on
    shipped crates. `cargo mutants` on the crates M4 adds — see [M4b](../M4b/VERIFIED.md)
    criterion 7, which carries the combined number. `cargo fmt --check`,
    `cargo clippy --all-targets -- -D warnings`, `cargo test --workspace`, `cargo deny check`
    (advisories, bans, licenses, sources all ok), `scripts/check-links.sh`,
    `scripts/build-index.py --check` all green.

## What this milestone got wrong

⚠️ **`C` cannot be a constant.** D-5 states "C ≈ 32". Measured, that is adequate only for a
small fleet: imbalance is 1.208× at N=100 but **1.811× at N=2000**, which fails criterion 5
outright at scale. `window()` now grows as `max(32, 3·√N)`, and
[C-6](../../research/04-cluster/routing-and-placement.md) records it.

⚠️ **The FNV prime was wrong by a factor of sixteen.** `0x1000_0000_01b3` groups to
`0x1000000001b3`; the underscore placement hid an extra zero. It cost imbalance 1.347 against
1.205, which is a bad-but-plausible number — the kind that gets explained rather than
investigated. The constant is now named, with the failure written beside it.
