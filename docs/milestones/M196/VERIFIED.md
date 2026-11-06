# M196 verification

Implemented in:
- `crates/pstore-engine/src/replica.rs`: `CopyOptions` and `create_copy_with`, `view`, the
  create's refusals, the sync's view, `mirror`'s remap and counts, the check before publishing,
  and the status.
- `crates/pstore-engine/src/head.rs`: the `copy_as_of` section, its filter, the forced counts,
  and failure code 2.
- `crates/pstore-format/src/lib.rs`: `footer_rows`.
- `crates/pstore-server/src/replication.rs`: the `as_of` field and its refusal on a replication.

The docs are in `docs/api-migrations.md` and the DELIVERY row. All runs were in a 4-core cloud
container, so every number is `provisional`.

How to run:
- `cargo test -p pstore-engine --lib head::`
- `cargo test -p pstore-engine --test it replicate::`
- `cargo test -p pstore-server --test it replication::a_copy_takes_an_as_of_epoch`

**Red first.**
- The HEAD test failed against a field with no encoding (.harness/m196-red-head.log).
- The four engine tests failed against an API that ignored `as_of` (.harness/m196-red-engine.log).
- The server test and its unit test failed against a route without the field
  (.harness/m196-red-server.log).
- Three tests were written after their code and never seen red: `footer_rows_reads_the_count_the_open_reads`,
  `answerable_is_what_as_of_answers` and `a_remapped_past_epoch_keeps_its_counts`. Each is shown
  to test something by a mutation it kills (below).

**Suite:** 2,702 passed and 0 failed, with the two IPv6 tests skipped because the container has
no IPv6. fmt and clippy pass (.harness/m196-suite.log).

**Coverage:** `scripts/coverage.sh` over the shipping crates gave lines 97.30%, regions 95.97%
and functions 97.04% (.harness/m196-coverage.log).

**Mutations.** I applied 43 mutations one at a time by script and reverted each one
(.harness/m196-mutations.log). Each ran the tests that claim it. A container restart cut the
first round short after three mutations and left the fourth applied; I restored it by hand and
resumed. Four survived across the rounds, and each was resolved:
- **The sync's `once` filter on `as_of` removed.** `copy_as_of` as decoded only holds names
  `once` or `copies` holds, so the filter was dead. Removed.
- **A remap's lapse reported as a transient failure.** Killed by a new case in
  `a_reaped_epoch_ends_a_copy_typed`: a GC runs at the first segment read, so the remap's view
  fails.
- **Counts from bytes used for a copy without `as_of`.** Equivalent, since the bytes say what
  HEAD says. The guard was removed and the spec amended.
- **A reused delete vector keeping the view's count.** Killed by the new
  `a_remapped_past_epoch_keeps_its_counts`.

The rest were killed, including: the view ignored; the present schema kept; each create
refusal dropped; the present fingerprint checked; `as_of` not written, or a stale one kept;
`as_of` left out of the match or the status; the sync not reading it; the step-1 lapse retried;
the remap reading the present; rows, length or delete counts left as the view's; the check before
publishing dropped, or blind for a local or a remote source; both edges of `answerable`; the
encode filter; each forced count; the decoder's end check; failure code 2 decoded as 1; and each
server change.

No `cargo mutants` run is claimed. CI's mutation shards are the check.

1. PASS: `an_export_holds_a_past_epoch` and `a_remapped_past_epoch_keeps_its_counts`, for a
   source in the same tenant, another tenant and another store. After a delete, a replacement,
   a compaction and the drop, the export `as_of E` answers and digests as the source did at
   `E`. It has `E`'s 59 documents and segment count and its schema, and each ref's rows and
   length match its bytes. A remap keeps the reused delete vector's count.
2. PASS: `a_past_epoch_is_refused_as_a_query_refuses_it`. It refuses each of these, and HEAD's
   epoch is unchanged after each:
   - a future epoch and one below the horizon (`TimeTravel`);
   - an epoch with no segment (`SourceNotFound`);
   - `as_of` with a reindex (`Invalid`);
   - a replica source (`NoHistory`).
3. PASS: `a_reaped_epoch_ends_a_copy_typed`, for all three source kinds. A GC passes `E`
   before the sync, between the copies and the commit, or at the first segment read. The copy
   ends `failed` with `epoch_unavailable` and `ended_ms` set, and `dest` holds nothing. A
   present copy in the same sync succeeds.
4. PASS: `an_as_of_never_outlives_its_copy`. The same `as_of` is matched, and another one or
   none is refused `Exists`. A later copy into the failed name reports `as_of = None` and holds
   the present.
5. PASS: `copy_as_of_round_trips_after_copy_times`. Over three base HEADs it checks:
   - the section round-trips alone and after the other sections;
   - a filtered name encodes as M194's bytes;
   - a trailing byte is refused, and no cut decodes the section;
   - failure code 2 round-trips.
6. PASS: `a_copy_takes_an_as_of_epoch` and `an_unavailable_epoch_has_its_code`. It checks:
   - `as_of` in the `POST` answer and the status;
   - `succeeded` with that `source_epoch`, holding the rows a present copy made at `E` holds;
   - `null` for a present copy;
   - 400 for a future epoch (`time_travel_horizon`), with `schema`, and on a replication;
   - the code `epoch_unavailable`.
7. PASS: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, the suite and
   `scripts/coverage.sh --fail-under-lines 95 --fail-under-regions 95` ran here (see above).
   `scripts/mutants.sh` is left to CI's shards.

Code review (packet from `scripts/review.sh context m196`, reviewed on 2026-11-06) passed in
round 1 with no blocker or major. Three minors, each left as it is:
- Every copy now takes rows and length from its bytes (the spec is amended to say so). A footer
  that does not parse would record 0 rows where HEAD's count was kept before. A segment whose
  footer does not parse cannot be opened by a query either.
- A lapse found on the last commit attempt ends the sync with `Lost`, and the copy is ended by
  the next sync's view.
- A source dropped and re-made as a replica after the create is not refused again at the sync.
  `create_inner` refuses any name `dropped` still remembers, so this needs GC to pass the drop.
  That also passes the epoch, and the view then ends the copy.
