# M92 — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ **Where this ran:** a Linux x86-64 cloud container, in-memory stores, the real `good.parquet`
fixture. A "second store" is a second `MemoryStore`; **nothing here ran against a real bucket**, so
what an S3 ETag does across buckets is the spec's argument, not a measurement.

Commands: `cargo test -p pstore-engine --test external_copy` (5 tests), `--test external_retry`
(4) and `--test external_index` (5) beside it; `cargo test -p pstore-server --test external` (6).
**Red first:** `external_copy.rs` failed to compile before `copy_external` existed (and, once it
did, on a `Flaky` import path of its own).

1. `a_copy_answers_as_its_source_does`: in this tenant under a new name, and from another tenant,
   the copy's ranking equals the source's to the bit; the counting store records no ranged read
   of the customer's object while copying; the copy's codes key differs from the source's and the
   rest of the registration is equal.
2. `a_copy_from_another_store_reads_the_customers_object_in_its_own`: with the object in the
   copying store it answers as the source does; with none, a query is refused (`Source`, "no
   object"); with a different one, `Changed`.
3. `a_copy_is_refused_unless_this_deployment_allows_it`: off makes no request (read and write
   counts unchanged); a prefix that does not cover the registration's source is refused and the
   codes blob is never read.
4. `a_copy_is_refused_with_its_reason`: no source index (`NotFound`), a name taken by the source
   and by an earlier copy (`Exists`), three invalid names and an invalid source name (`Refused`),
   a codes blob that is not one (`Source`, and nothing committed), and a source store whose reads
   fail (`Blob`, and nothing committed).
5. `a_copy_outlives_its_source_and_buries_only_its_own_codes`: after dropping the source and
   `gc(0)`, its codes are gone, the copy's remain and the copy answers as before; dropping the
   copy buries its codes at that epoch and GC reaps them, and the customer's file is untouched.
6. Over the API: `a_copy_answers_as_its_source_does_and_outlives_it` (same tenant, another
   tenant, the source dropped) and `each_copy_refusal_has_its_status_and_code` (`404
   external_index_not_found`, `400` for an unknown store with the `PSTORE_SOURCES` hint, a bad
   tenant, a missing `index` and an unknown field, `409 index_exists`, `403 external_sources_off`).
7. **Gates:** `./scripts/gates.sh`, green by the pre-commit hook on this commit. **Mutation**
   (cargo-mutants with `--no-config --in-diff`): the engine's copy, commit and registration
   changed lines are **4 mutants, 4 caught** (the commit loop's retry guards were moved out of the
   registration unchanged, and are the retry tests' from M86's sweep). The server's external
   and replication modules are **4 mutants, all unviable** (the handler's return value replaced
   by a Json that does not compile): nothing to catch or miss.

**Not covered:** a real bucket, and so an ETag across buckets; a *continuous* follower of a
source's re-registrations (a new copy does it); authentication: a copy names any tenant's
external index in the same bucket, as a replication names any tenant's index (`auth` in
[deploy.md](../../deploy.md)).
