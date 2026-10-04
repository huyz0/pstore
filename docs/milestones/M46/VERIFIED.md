# M46 — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ **Where this ran:** a Linux x86-64 cloud container, with no Docker daemon. The Azure
evidence is **Azurite 3.34.0** from npm (criterion 5), which makes it **provisional**:
emulator plumbing, silent on Azure's latency, cost, and CAS under contention (M0b).

⚠️ **A test that fails to compile is not counted as red**, so each criterion names the hand
mutation seen to fail it, or that the test went red before the code existed.

1. **A store without suffix reads opens an unknown-length segment by `head` then range.**
   `without_suffix_reads_an_unknown_length_is_asked_first`:
   - over `Accounted` `NoSuffix`, a 100-row segment (over 8 KiB) and a 2-row one each bill 1
     read with a known length, and 2 without;
   - the bytes are equal.
   - Red before the fallback existed: the unknown-length open failed.
2. **`as_of` answers on such a store.** `a_store_without_suffix_reads_answers_as_of`: three
   folds, a compaction, then every leg `as_of` the epoch before it, the same as over
   `MemoryStore`. Killed by hand: the fallback disabled.
3. **Every store says what it has, and the conformance suite measures it.**
   - `azure_declares_no_suffix_and_batches_of_256` (`cargo test -p pstore-blob --features
     object_store`): `suffix_read` false, `max_batch_delete` 256, `delete_is_free` false from a
     profile that said true, and the profile's `cas` kept. Killed by hand: `azure` keeping
     suffix reads.
   - `suffix_read_is_measured_not_declared`: a store that claims suffix reads and fails them
     is observed without. Killed by hand: the report copying the declared value.
   - Added for the sweep: `a_store_without_suffix_reads_conforms_on_everything_else`.
     `NoSuffix` diverges on `suffix_read` alone, refuses both suffix forms, and forwards a
     listing.
4. **Configuration.** `azure_is_configured_with_an_account`:
   - parses the account, key, container and endpoint as given;
   - refuses a missing or empty account, and a key without an account;
   - treats an empty key or container as unset (added in code review);
   - `azure_capabilities` names `azure(acct)` with S3's profile rule.
   - `a_backend_or_profile_it_does_not_know_is_refused_by_name` names `gcs` now.
   - Killed by hand: the empty-account filter removed; the empty-key filter removed.
5. **End to end against Azurite (provisional).** `./scripts/azurite.sh` passes both ignored
   tests:
   - `writes_folds_queries_and_reads_as_of_against_azurite`: the engine over
     `ObjectStoreBackend::azure`, with all three legs before and after a compaction, and
     `as_of` the epoch before it.
   - `the_server_writes_folds_and_answers_as_of_against_azurite`, added in code review: writes,
     folds and asks `as_of` through `Api`'s router.
   - **With the fallback disabled, both fail with the real Azure client's refusal**:
     "Operation not supported: Azure does not support suffix range requests". So the
     emulator path genuinely reaches the `head` fallback.
6. **Every existing test passes, unchanged except for the new field and the renamed unknown
   backend.** `./scripts/gates.sh` was green by the pre-commit hook on M46.1. Its first
   attempt was refused when the disk filled, and it passed after space was freed.
   - ⚠️ **`cargo deny check` does not pass, and not because of M46.** Bans, licences and
     sources pass with the Azure dependencies. Advisories fail only on RUSTSEC-2024-0436:
     `paste` is unmaintained, reached through `foyer-memory` → `pstore-cache`. The same
     failure appears on the parent commit with this diff stashed, and `Cargo.lock` is
     unchanged by M46. It is reported, not fixed here.
7. **Mutation.** `cargo mutants --in-diff` over M46's source diff (`f98d1f8..18d52e4`),
   testing the five touched crates: 41 mutants, 22 caught, 15 unviable, 4 missed.
   - The misses were `azure`'s free-delete override (the test's profile already said false) and
     three `NoSuffix` methods nothing called.
   - Their tests were strengthened (criterion 3), and the sweep of those two files over the
     new diff found **0 missed** (28 mutants: 18 caught, 10 unviable), run against `pstore-blob` and `pstore-testkit` alone, so the fixture is pinned by its own tests.

**The capability matrix was edited by hand**, against its "do not edit" line: there is no
Docker here for `rustfs` and `fake-gcs-server`.
- Each probed backend's new `suffix_read` row is its recorded `suffix_read` probe outcome: true
  for rustfs, false for Azurite.
- The header now says three of seven fields are measured.
- `probe.rs` now prints exactly these rows, in this place, and this header, so the next
  `scripts/conformance.sh --check` finds no diff from this edit. `--check` was not run.
- The probe still builds Azurite with `ObjectStoreBackend::new` and the unprobed profile, so
  the matrix declares `max_batch_delete: 1000` where the shipped `azure` declares 256 (code
  review).

**Residue, said so:**
- An `as_of` read on Azure pays a billed `head` before each buried segment it opens, warm
  cache or not, because `Caching::head` is not cached. Recording graveyard lengths is the
  rejected alternative the spec names, a cost judgement to revisit.
- The read cache's store identity is written as `s3 {endpoint} {bucket} {id}` on Azure too.
  It is correct, because the id is unique per container, but misleading (code review).
- Replication sources and `pstore-node` stay S3-only.
- Nothing about real Azure is measured: M0b.
