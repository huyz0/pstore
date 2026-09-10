# M6b — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ Every "observed red" is a **mutation applied to the shipped code**, the named test run, and
the failure read. All of them are in `cargo test -p pstore-meter --test quota`.

1. **Inside its quota, a tenant is untouched** — `a_tenant_inside_its_quota_is_untouched`: six
   operations, six requests underneath on `Accounted`, byte-identical results.
   `an_unlimited_quota_is_transparent` is the stronger form and the one that matters for
   putting this in a stack — 200 reads plus a fan-out under `Quota::unlimited()`, balances
   still infinite after a simulated day. ⚠️ A bucket that read an infinite rate as "no tokens"
   would refuse everything, which is the worst possible failure for a component whose job is to
   be invisible.
2. **Refused by name, and one tenant's quota is not another's** —
   `an_exhausted_tenant_is_refused_by_name` (the message carries `429`, `tenant 1` and
   `blob requests`) and `one_tenants_quota_is_not_anothers`. Observed red by keying the bucket
   map on a constant instead of the tenant: **a single global bucket passes every
   single-tenant test in the file**.
3. **A fan-out over quota issues nothing, and reserves the coalesced plan** —
   `a_fan_out_that_would_exceed_the_quota_issues_nothing` drives **both entry points** and
   asserts **zero** requests on `Accounted` beneath, not `plan.len()` − 1.
   `a_coalesced_fan_out_reserves_what_it_will_actually_issue` is the other half: observed red
   by reserving `ranges.len()`, which refuses a fan-out that was comfortably inside the quota.
   ⚠️ Two fixtures because one cannot do it: at `with_coalesce_gap(0)` the plan length *equals*
   the range count and the two implementations are indistinguishable.
4. **Refill is at the configured rate, from a supplied clock** —
   `tokens_refill_at_the_configured_rate`: four admitted, the fifth refused, the clock advanced
   one second, exactly four more admitted. Observed red by resetting to full when no time has
   passed — which is the bug the first implementation actually had, and it made **seven of the
   thirteen tests fail at once** rather than one.
5. **The burst bounds an idle** — `a_long_idle_does_not_admit_more_than_the_burst`: an hour of
   virtual idling admits four, not 14,400.
6. **The buckets are independent, and an overrun is owed** —
   `bytes_and_requests_are_separate_buckets` (requests plentiful, bytes exhausted, the error
   names bytes and not requests), `a_byte_refusal_costs_no_request_tokens`, and
   `an_overrun_is_still_owed_on_the_next_operation`.
   ⚠️ **The second is also from code review.** `admit` reserved requests before checking byte
   credit, so nine byte-refusals spent a tenant's entire request burst on operations that never
   issued a request — after which it was refused for the *wrong resource*. Bytes are checked
   first now, which costs nothing because checking credit takes no tokens. The existing test
   could not see it: it set the request rate to `1e9`.
   ⚠️ The **third** is the one that makes the byte quota a bound at all: observed red by
   saturating the debit at zero, which forgives every overrun and lets a tenant repeat "one
   giant read per refill tick" forever while passing every other test here.
   `exactly_zero_credit_is_no_credit` pins the boundary `>` and `>=` differ on — a budget spent
   exactly to zero must refuse, or a quota of N admits N + 1.
7. **Bytes are debited on what moved** — `a_short_read_bills_what_it_returned` (a suffix longer
   than the object bills the object), `a_refused_request_bills_nothing`, and
   `the_meter_and_the_accountant_agree_on_a_coalesced_fan_out` for the third clause.
   ⚠️ **Code review found this one wrong, and measured it.** The first implementation billed
   the sum of the slices handed back to the caller — but the coalescer merges nearby ranges and
   the wire moves the gap bytes too, which is what `Accounted` beneath bills. On an 8-range
   fan-out at a 1 KiB gap: **64 bytes against 456**, a 7× undercharge on exactly the read shape
   the byte bucket exists for. `Metered` now coalesces **once** and dispatches the merged
   fetches itself, summing what each returned. Observed red by billing the requested ranges.
8. **The meter and the accountant agree** — `the_meter_and_the_accountant_agree`, over all four
   `OpClass`es and both counters, across ten different operations, **and**
   `the_meter_and_the_accountant_agree_on_a_coalesced_fan_out`. ⚠️ **Two fixtures, because one
   could not fail.** At `with_coalesce_gap(0)` every plan entry is one requested range, so the
   two byte figures are equal by construction and a meter billing the wrong one agrees on every
   input — which is how the 7× undercharge in criterion 7 survived a 99.26%-region,
   73-of-73-mutant sweep. Criterion 3 already made this argument for the request count; it
   needed making for the byte count too. ⚠️ Scoped to operations that **succeeded**,
   deliberately: on a fan-out that fails partway `try_join_all` returns `Err`
   while the fetches that did return are already billed underneath, so an unscoped claim would
   be false. ⚠️ The fixture seeds its object as a **third** tenant — seeding as A left A's
   ledger holding a write the meter never saw, and the test was comparing the fixture.
9. **`get_tag` is counted and never refused** — `get_tag_is_counted_and_never_refused`, on a
   store whose quota is exhausted. It returns `Option<CasTag>` with no error channel, so a
   refusal could only be `None` — which reads as "the object is absent" on the **rebase step of
   the CAS commit protocol**, turning a quota refusal into a create-if-absent against an object
   that exists. A hole, pinned so it cannot close by accident into a silent wrong answer.
10. **Gates** — `./scripts/gates.sh` green, `cargo deny check` green, 97 suites.
    `pstore-meter` **96.59% regions**, 97.93% lines, **100% functions**
    (`cargo llvm-cov -p pstore-meter --all-features --lib --tests`); workspace **95.44%** via
    `./scripts/coverage.sh --fail-under-regions 95`. Mutation: **86 mutants, 77 viable, 77
    caught, 0 missed, 0 timeouts**. ⚠️ Re-run after the code-review fixes rather than quoted
    from before them — a sweep is a statement about the tree that produced it, and the tree
    changed. The pre-fix figure was 73 of 73 on a smaller module.

## Two more properties, neither of them in the spec

Both came out of coverage being under the floor, and both are the kind of thing a request
counter cannot see:

- **Every method is metered, and metered once.** `every_method_is_metered_and_every_method_refuses`
  drives all fourteen and asserts zero requests reached the store;
  `every_method_costs_exactly_one_request` asserts the other direction. ⚠️ This is the hole the
  crate exists to close, stated as a test: a method forwarded without a reservation is
  unmetered, and every other test here drives `get`.
- **`put_conditional` refuses with `CasError::Io`, never `Contended`.** Asserted through
  `should_rebase()`, which is the predicate the retry protocol actually branches on: every
  caller in the tree treats `Contended` as "retry the same attempt unchanged", so a quota
  refusal shaped that way is a retry storm against the quota.
- **A short coalesced fetch is an error, not a panic** —
  `a_short_coalesced_fetch_is_an_error_not_a_panic`. ⚠️ Re-implementing the fan-out to bill
  correctly meant re-implementing the trait default's slicing, and that default **indexes** the
  merged buffer — which panics if a backend answers a range with the bytes that exist. HTTP
  permits exactly that, and M0a's conformance suite found it on its first run against a foreign
  backend. Checked here, with the shortfall in the message.
- **A fan-out whose fetch fails bills no bytes and still charges its requests** —
  `a_fan_out_whose_fetch_fails_is_an_error_and_bills_no_bytes`, which is criterion 8's scoping
  made executable rather than left as prose.

## What is not built, and named rather than omitted

- **Nothing is wired into any stack**, so nothing is refused yet. `Quota::unlimited()` is the
  default for exactly that reason, and criterion 1 is what makes adding the decorator safe.
- **Per tenant, where Design rule 13 says per index.** `BlobStore` has no index concept, so one
  index can starve the other forty-nine in the same tenant. Declared in the spec and in the
  crate's own docs, not inferred from silence.
- **Per node, so not a fleet-wide quota**, and `usage` is this node's view. Summing across the
  fleet is the billing rollup's problem.
- **`Usage` in `TenantRecord`** — removed by spec review, and the reasoning is in the spec: it
  is unsound in both directions and the catalog's records carry no version field.
- **CPU and scan-byte accounting (D-63/D-64), cache-byte quotas (OQ-21), the billing rollup,
  and shedding policy.** Each needs something that does not exist yet.
