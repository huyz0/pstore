# M280 verification

The spec was not sent to a separate reviewer before code; the PR's review covers it.
⚠️ `provisional`: deterministic simulations in a cloud container, slots and hit ratios only.

1. `cargo test -p pstore-server --lib ingress -- --nocapture`:
   `borrowing_admission_measured_against_the_fixed_cap` passes and printed (slot-steps over
   200 steps, 64 slots, own cap 8, reserve 8):

   | workload | fixed: hot slots, small refused | borrowing |
   |---|---|---|
   | hot tenant alone | 8, - | **56**, - |
   | hot + 1 small a step | 8, 0 of 200 | **52**, 0 of 200 |
   | hot + bursts of 8 small | 8, 0 of 80 | **56**, 0 of 80 |
   | hot + bursts of 24 small | 8, **0 of 240** | 56, 160 of 240 |
   | small tenants only, 10 a step | identical | identical |

   Three wins, one tie, one loss: default on (`DEFAULT_BORROW_RESERVE = Some(8)`). Operators
   whose small tenants arrive in bursts larger than the reserve should set
   `tenant_borrowing: false` (`docs/deploy.md`). Red: with the borrow test's `>` made `>=`
   (borrowing one slot into the reserve), it and `the_fixed_cap_is_m128s_and_borrowing_keeps_its_reserve`
   failed.
2. `cargo test -p pstore-cache --test it tenant_share -- --nocapture`:
   `a_tenant_share_measured_against_one_shared_lru` passes and printed (hit permille, 64-entry
   bulk quota, share 250):

   | workload | one shared LRU | share 250 |
   |---|---|---|
   | cold scan beside 4 small tenants | 0 each | **1000 each** |
   | one tenant, working set 32 entries | **1000** | 0 |
   | 8 small tenants, 6 entries each | 1000 each | 1000 each |
   | 2 tenants of 24 entries | **1000 each** | 0 each |

   One win, one tie, two losses: default off. Red: with the tenant's own eviction loop
   removed, it failed.
3. Both modes:
   - `cargo test -p pstore-server --lib ingress`: 18 passed, among them
     `a_permit_returns_everything_and_idle_tenants_leave_no_entry` (pinned to the fixed cap),
     `the_fixed_cap_is_m128s_and_borrowing_keeps_its_reserve`,
     `a_reserve_the_server_cannot_leave_free_is_the_fixed_cap` and
     `borrowing_is_configured_off_on_or_by_its_reserve`.
   - `cargo test -p pstore-cache --test it tenant_share`: `no_share_is_one_shared_lru_and_a_share_of_1000_is_none`
     and `a_key_of_no_tenant_is_never_capped` pass.
   - `cargo test -p pstore-server --test it -- read_cache ...`: `the_cache_is_configured_by_name`
     and `a_bad_cache_setting_is_refused_by_name` pass.
   - The whole `pstore-server` suite with borrowing on by default, `cargo test -p pstore-server
     --no-fail-fast` skipping the two IPv6 tests a container cannot run: 733 integration tests
     and 134 unit tests passed, none failed. Its M128 tests in `tests/it/ingress.rs` pin the
     fixed cap through their `limits` helper.

**A downside found after the merge request was opened** (CI's coverage run on #72):
`ref_new::adversarial_writers_never_both_win` failed once with borrowing on. Four servers race
100 `applied` writes of one tenant; under the fixed cap each holds at most 8 at once, 32 in
all, which is exactly HEAD's list of outcomes (`OUTCOMES_KEPT`). Borrowing lets each hold up
to 56, so an outcome can fall off the list before its writer reads it, and the write answers
`outcome_unavailable` (applied, outcome not readable) instead of its counts. Nothing is wrong
in the data; the report is lost. The test is pinned to the fixed cap, as the rule for existing
tests says. ⚠️ Under borrowing, a tenant with more than 32 applied writes in flight across
its servers can see `outcome_unavailable`; under the fixed cap only past four servers.
