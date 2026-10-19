# M107 — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ **Where this ran:** a Linux x86-64 cloud container. Criteria 1, 2 and 4's unit half ran
against byte strings and a mock HTTP service. Criteria 3 and 4's emulator half ran against
fake-gcs-server 1.55 in Docker. **Emulator evidence** (D-99): provisional. It says the request is
shaped right and answered part by part. It says nothing about how real GCS throttles a batch,
which is M0b's.

1. **The encoding is pinned without an emulator.** In `cargo test -p pstore-blob --features gcs --lib`:
   - `three_keys_are_three_delete_parts_and_a_closing_boundary` checks three `DELETE` parts
     with `Content-ID`s 1 to 3 and the closing boundary;
   - `a_name_is_encoded_outside_the_unreserved_set` checks that `/`, a space and `+` are
     `%2F`, `%20` and `%2B`, and that the unreserved set passes through.
2. **The response is read part by part.** In the same run:
   - `a_2xx_or_a_404_is_a_delete` passes `200`, `204` and `404`;
   - `a_403_in_the_second_part_fails_naming_the_second_key` names the second key and `403`;
   - `two_answers_for_three_keys_fail`, `a_part_with_no_status_line_fails` and
     `an_answer_that_is_not_multipart_fails` each fail as named.

   Seen red against stubs: 6 of the 7 failed. The seventh is the 2xx-and-404 test, which
   passed against a stub that accepts everything, by design. Through the mock service:
   - `a_batch_is_one_post_with_the_bearer_and_the_encoded_body`;
   - `an_empty_bearer_sends_no_authorization`;
   - `a_failed_part_fails_the_delete`;
   - `a_throttled_batch_slows_down_and_any_other_failure_names_its_status`;
   - `gcs_declares_batches_of_100_and_sends_each_as_one_request`.

   These were seen red, 4 of 4, under seeded defects: the batch routing removed, the bearer
   check inverted, and 503 not mapped to `SlowDown`. `--features gcs --lib`: 24 passed.
3. **On the emulator, one request deletes 100 objects.** `one_request_deletes_a_hundred_objects`
   (`./scripts/fake-gcs.sh`): 97 objects written, 3 absent, and one `delete_batch` of 100.
   - The Delete bill rose by 1.
   - The client log is exactly `["POST /batch/storage/v1"]`, with 0 `DELETE`s.
   - Every key `get`s `NotFound`.
   - The emulator's own top-level tally for the run is 1 `POST /batch/storage/v1` and 0
     top-level `DELETE`s, beside the 97 uploads and the test's GETs.

   Seen red against `ObjectStoreBackend::new` twice. With 3 keys absent it failed with
   `NotFound` at the first absent key: the per-key path fails a batch the batch endpoint
   accepts, recorded in the spec's "Found while building it". With all 100 present it
   failed with `100 DELETEs` (left 100, right 0), while the bill read 1.
4. **Over the cap is refused.** `over_the_cap_is_refused_before_any_request`: 101 keys are an
   error naming 101 and 100, and the log is empty. The emulator run had 2 passed.
5. **M7a's gap line** is struck, linking here.
   `grep -c "DONE\*\* by \[M107\]" docs/milestones/M7a/VERIFIED.md` → 1.
6. **Gates.** `./scripts/gates.sh`, by the pre-commit hook on this commit. Review round 1
   found that `.cargo/mutants.toml` built the sweep without `gcs`, so every mutant in
   `gcs_batch.rs` would read as MISSED with nothing compiled. `pstore-blob/gcs` is now in its
   additional cargo args. The diff's sweep ran with `--features pstore-blob/gcs`
   (`./scripts/mutants.sh --features pstore-blob/gcs`): `20 mutants tested in 2h: 19 caught,
   1 unviable`, 0 missed.
   `./scripts/check-portable.sh` passes with `scripts/fake-gcs.sh` declaring `portable: no`.
