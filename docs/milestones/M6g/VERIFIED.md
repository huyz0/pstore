# M6g — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ Every "observed red" is a **mutation applied to the shipped code**, the named test run, and
the failure read. Command: `cargo test -p pstore-catalog --test split`.

1. **Every tenant survives a split** — `every_tenant_survives_a_split`: the same 400 tenants
   before and after, enumerated at the new width.
2. ⚠️ **A record lands where the new width says** — `a_record_lands_where_the_new_width_says`,
   reading each bucket **directly**. Enumerating reads every bucket, so a census passes whether
   or not anything moved — the partition predicate could be inverted and it would agree.
   Observed red by inverting it. The test also asserts that *some but not all* tenants moved,
   so a partition that moved everything or nothing is caught too.
3. ⚠️ **The old buckets still hold every record** —
   `the_old_buckets_still_hold_every_record`. This is "stale rather than wrong", and it is the
   property pruning would break. Observed red by writing the root **first**, which publishes a
   width whose buckets do not exist yet — six of the eight tests fail on that one.
4. **A census at a width the deployment has left is refused** —
   `a_census_at_a_width_the_deployment_left_is_refused`, naming both widths. Observed red by
   dropping the check, which returns a census that is quietly short.
5. ⚠️ **A tenant in two buckets is one tenant** — `every_tenant_survives_a_split` again, and
   **this is a defect the milestone found rather than introduced**: `merge` was applied per
   bucket and the results concatenated, so a tenant appearing in two buckets appeared twice.
   That is the unpruned post-split state exactly, and it was already reachable for any record
   written under a stale width. The spec claimed the deduplication already happened; it did
   not. `enumerate` now merges across buckets.
6. **A stale-width writer is still found** — `a_stale_width_writer_is_still_found`: an appender
   built at `w` records after the split, and a census at `2w` finds it, because the old bucket
   it wrote to is still read. ⚠️ The worse case of criterion 3 — a lost **write**, not a stale
   read — and the reason pruning needs a bound on stale-process lifetime that nothing provides.
7. **Splitting stops at the key format** — `splitting_stops_at_the_key_format`, and
   `the_default_width_doubles_into_the_format` states the arithmetic as a fact:
   `DEFAULT_WIDTH * 4 == MAX_WIDTH`, so the current key space allows exactly two doublings.
   ⚠️ **`OBSERVED-NOT` for the guard itself.** Removing the explicit `<= MAX_WIDTH` filter
   changed nothing, because `Width::new` already refuses anything past the key format — the
   filter was dead. It is **removed rather than tested**, the same call M5f and M6e made on
   their own unreachable guards.
8. **A split that loses the root CAS changes nothing observable** —
   `the_old_buckets_still_hold_every_record` covers it through criterion 3's mutation: with the
   buckets written and the root unmoved, every reader still reads the old shape and finds every
   tenant. The root write is conditional on the tag the split read.
9. **Zero LIST** — `splitting_does_not_list`, across the split and a following census.
10. **A backend that cannot fence is refused** —
    `a_catalog_write_on_a_divergent_backend_is_refused`, with `split` added to that existing
    loop over every guarded door rather than as a second fixture.
11. **Gates** — `./scripts/gates.sh` green. Mutation over this milestone's functions, in the
    `dev` container (`scripts/mutants.sh --check 'bucket::split|enumerate_since'`): **22
    mutants, 18 caught, 4 missed** — and **none of the four is in `pstore-catalog`**. They are
    the engine's stale-commit test helper, one in `pstore-node`, and two in `pstore-testkit`,
    all of which predate this milestone and survive every sweep in this session.
    ⚠️ **CORRECTION.** An earlier version of this line claimed the first sweep run for this
    milestone "measured nothing and looked clean". **That was wrong, and it is corrected here
    rather than deleted.** The first sweep used the regex `split|enumerate_since|gather`; I
    grepped its output for `pstore-catalog`, found none, and concluded it had skipped this
    milestone's code. It had not — `cargo mutants` prints only **missed** mutants by file, so
    zero catalog lines meant zero catalog mutants *missed*, which is the good outcome. That
    regex selects **14 catalog mutants of its 66**, and every one was caught. Both sweeps were
    valid; the narrower one above is simply the one scoped to this milestone.
    ⚠️ The lesson is the one this project keeps relearning about its own gates: a summary read
    the wrong way is indistinguishable from a real finding, and I published it as one.
    ⚠️ It did surface something real, now backlog item 18: `pstore-index/src/lire.rs` has 8
    missed and 2 timeouts across its split routines. That module is explicitly a spike for
    OQ-51 and is not wired into the dense index, so it is not the correctness core — but a
    spike whose arithmetic nothing constrains can answer its open question wrongly.

## What is not built, and named rather than omitted

- ⚠️ **Pruning the old buckets, which is the only step that makes anyone wrong.** Until it
  happens a split **doubles the deployment's bucket objects** and reclaims nothing: two
  doublings from the default leaves 65,536 heads where 16,384 stood, with the old ones still
  holding records that have moved. Doing it safely needs a bound on how long a stale-width
  process may live, and nothing provides one — criterion 6 is why that matters more for
  writers than readers.
- ⚠️ **Anything past 65,536 buckets.** That is the fifth hex digit in `{bucket:04x}` and a
  different key space.
- **Shrinking**, and **automatic splitting**: nothing measures bucket occupancy and nothing
  calls `split`.
- ⚠️ **`split` is not atomic and not resumable.** A crash between the bucket writes and the
  root CAS leaves the new buckets written and the width unmoved — observably nothing, by
  criterion 8 — and a re-run redoes the partitioning from the same source.
- ⚠️ **Two tests changed their asserted round-trip counts**, and neither was loosened to pass:
  `enumeration_is_two_rounds_deep` goes 2 → 3 and its cold path 3 → 4, and
  `enumeration_requests_do_not_scale_with_tenants` goes `width * 2` → `width * 2 + 1`. Both
  remain exact, both name the round they pay for, and what the second test is *about* — that
  the count does not scale with tenants — is unchanged.
- ⚠️ **Several fixtures now write a root.** A test enumerating at width 1 with no root modelled
  a deployment that cannot exist: an absent root means "never widened", which is the default
  width, so a second process would read a different shape.
