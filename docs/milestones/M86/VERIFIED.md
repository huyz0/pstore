# M86 — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ **Where this ran:** a Linux x86-64 cloud container, in-memory store, the real `good.parquet`
(300 rows of 8) and `lance/one` (2,600 rows of 8) fixtures. **Nothing here ran against a real
bucket or measured a cost**; it checks behaviour, not what a registration costs a customer.

Commands: `cargo test -p pstore-engine --test external_index` (5 tests), `--test external_retry` (4),
`cargo test -p pstore-engine --lib` (head 12, external 1), `cargo test -p pstore-server --test external` (4).
**Red first:** each file failed to compile before the code it names existed (`ExternalKind`,
`register_external`, `allow_external_sources`, `Config::external_sources` were all absent).

1. `register_then_query_by_name_equals_the_library`: Parquet and Lance, three queries each,
   bit-equal to `ExternalIndex::search` over the same column; a second handle answers the same.
2. `an_unregistered_source_is_refused_unless_allowed`: default, wrong prefix and empty prefix list
   are each refused, and the counting store sees no read, no write and no ranged read in all of
   them (queries included). Over the API, `external_sources_are_off_by_default`: `403
   external_sources_off` for both calls.
3. `a_changed_source_is_an_error_not_a_stale_answer`: after the file is rewritten the query is
   `Changed`; over the API `409 external_source_changed` (in `each_refusal_has_its_status_and_code`).
4. `head_with_externals_round_trips_and_an_old_head_decodes`: round trip alone and beside a
   replication, segment lengths and dictionaries; an old HEAD has none; an empty set changes no
   byte of the encoding. `a_damaged_externals_section_is_a_corrupt_head`: every cut inside the
   section is an error or no registration, never a half-registration, and a kind byte of 9 is
   `CorruptHead`.
5. `dropping_a_registration_buries_its_codes`: the graveyard names the codes at the dropping epoch,
   they exist until `gc(0)` and are gone after it, the customer's object remains, and dropping a
   name that is not registered commits nothing.
6. `a_registration_the_engine_cannot_honour_is_refused_with_its_reason`: an unknown column
   (the reader's "no column"), a missing object, a name held by a registration or by an index
   with rows, and a query of the wrong width; no refusal commits.
7. `an_allowed_source_is_registered_queried_and_dropped_by_name`: the wire end to end, row 0 first
   for row 0's own vector. `each_refusal_has_its_status_and_code`: `403 external_source_not_allowed`
   (outside the prefixes and a `..`), `400 external_source_unreadable`, `409 index_exists`,
   `409 external_source_changed`, `404 external_index_not_found`, and `400` for each malformed
   body and each out-of-bounds `k` or `window`. `the_deployment_opts_in_by_name_and_names_its_prefixes`:
   unset and `off` are none, `allow` takes trimmed prefixes, `allow` with none and any other value
   are refused by name.
8. **Gates:** `./scripts/gates.sh`, green by the pre-commit hook on this commit. **Mutation**
   (`cargo mutants --no-config --in-diff`): the engine's first sweep of `external.rs`, `head.rs`
   and `lib.rs` was 52 mutants, 16 missed, all in the two commit-retry guards and `name_free`'s
   `||` chain; `a_contended_registration_retries_and_lands`,
   `a_registration_that_keeps_contending_reports_contention_after_every_attempt` (48 attempts: 24
   and the burial's 24), `a_contended_drop_retries_and_lands`,
   `a_drop_that_keeps_contending_reports_contention_after_every_attempt` and
   `a_name_is_taken_by_any_one_of_the_four_things_that_can_hold_it` close them: the re-sweep of
   those functions is **22 of 22 caught**. The server's `external.rs`: first sweep 1 missed (the
   default window), closed by `an_unnamed_window_is_four_rows_a_hit_and_always_within_the_bound`;
   re-sweep **19 mutants, 6 caught, 13 unviable, 0 missed**. The two re-sweeps ran only the tests
   that exercise these functions (`--test external_retry --test external_index --lib`, and
   `--test external --lib`), which can only under-report a catch, never over-report one.

**Not covered:** a real bucket; the cost of a registration build (it reads the whole column once
and writes the codes) against a customer's request budget; per-tenant allowlists; replication of
a registration (backlog row 79, [M92](../M92/VERIFIED.md)); an engine built before `allow_external_sources` keeps what it was
built with.
