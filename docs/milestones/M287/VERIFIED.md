# M287 verification

The spec was not sent to a separate reviewer before code. The PR's review covers it.

1. Each provider answers from a fake of its endpoint, which records every request:
   `imds_v2_takes_a_token_then_the_role_then_its_credentials` (a token PUT, then the role,
   then its credentials, each with the token header),
   `an_instance_that_answers_no_session_token_is_refused_not_read_as_v1`,
   `ecs_credentials_come_from_the_relative_uri_with_its_token`,
   `web_identity_is_exchanged_with_sts` (a form POST of `AssumeRoleWithWebIdentity`), and
   `credential_answers_missing_a_field_are_errors`. The chain's order:
   `the_chain_takes_the_first_provider_that_is_configured`, renamed from the spec's
   `the_chain_takes_the_first_provider_that_answers` by the spec's correction banner: a
   configured provider that fails is an error, never a fall-through. Commands:
   `cargo test -p pstore-blob --features object_store,s3-native --test it native_s3_creds`,
   10 passed, and `cargo test -p pstore-blob --features s3-native --lib creds`, 3 passed.
2. `credentials_refresh_before_they_expire` (credentials inside the five-minute window are
   fetched once more, and fresh ones are not fetched again) and
   `a_failed_refresh_keeps_credentials_that_have_not_expired`. Same command as 1.
3. `rfc3339_round_trips`: every day for about 600 years, and every second of three days
   across 29 February 2000, against `amz_date`. Command as 1's second.
   `rfc3339_refuses_what_it_does_not_understand` covers offsets other than `Z` and `+00:00`.
4. The emulator run, `scripts/conformance.sh` on 2026-11-19 against `rustfs/rustfs:1.0.0`
   in a cloud container, `provisional` (D-99). The probe now has a `rustfs-native` section
   beside `rustfs`. The first run **disagreed** on `cas_after_delete_is_lost`: the native
   client read rustfs's 404 to an `If-Match` PUT on a deleted key as an I/O error, where
   `object_store` reads it as `Lost`. The fake S3 answered 412 there, so the fake-based
   conformance test had not seen it. The fake now answers 404, as S3 does, which turned
   `native_s3_conformance` red; the client now maps a 404 under `If-Match` to `Lost`
   (`a_compare_and_swap_on_a_deleted_key_is_lost`, which also holds that a 404 to a create
   stays an error). The second run's `rustfs` and `rustfs-native` rows agree on all
   thirteen probes and every measured field, and both admit durable writes
   (`docs/profiles/capability-matrix.md`). So the default is `native`.
5. `blob_client_setting_is_parsed_and_refused_when_unknown` (unset is `native`, both names
   parse, anything else is refused by name). `both_s3_clients_serve_the_same_answers` runs
   the same puts, conditional puts, reads, ranges, suffixes, stats, listings, batch deletes
   and missing-key reads through `open_s3` with each client on the fake S3, and compares
   every answer. `the_native_client_finds_keys_through_the_chain_without_static_ones`. The
   existing `object_store` tests in `s3_retry` keep every assertion and now name their mode;
   `the_native_client_sends_every_request_once` runs the native client on the same fake.
   Command: `cargo test -p pstore-server --all-features --test it -- s3_retry blob_client`,
   10 passed. Not done as the spec's test plan put it: the rest of the server's S3 tests
   were not duplicated per mode. The comparison test is the per-mode run.

Found and fixed on the way: `object_store` mode's `map_cas_err` read any message containing
`409` as a conflict, and the message carries the elapsed time, so a 503 that took `1.409ms`
became `Contended`. It flaked `s3_retry` once in the full run.
`a_409_in_the_elapsed_time_is_not_a_409` was red before the fix. Command:
`cargo test -p pstore-blob --features object_store --lib object_store_backend`, 6 passed.

Whole suite: `cargo test --all-features --no-fail-fast` on 2026-11-19, 3,024 passed and 3
failed. One was the 409 flake above, which passes since. The other two,
`ipv6_clients_refuse_malformed_replies_and_release_wire_capacity` and
`serialization_failures_never_send_partial_requests_or_responses`, bind `[::1]` and fail
with `Address family not supported` because this container has no IPv6. They are not
touched here, and CI runs them.

Mutations: `cargo mutants --in-diff` over this milestone's diff in `pstore-blob` and
`pstore-testkit`, tested by `pstore-blob`'s suite, on 2026-11-19: 134 mutants, 102 caught, 25
unviable, **7 missed**: the RFC 3339 parser's width check (`&&` to `||`), and six boundaries of
the credential cache (`<` to `<=` on the refresh window and the expiry, and the re-check after
waiting for a refresh). Added `the_refresh_bounds_are_strict`,
`callers_waiting_on_a_refresh_share_its_answer`, and two cases in
`rfc3339_refuses_what_it_does_not_understand`. The re-run over `creds.rs`'s parser and cache:
87 mutants, 84 caught, 3 unviable, none missed. CI's shard 2 reported the same parser survivor
on the first push. The six mutants in `pstore-server/src/lib.rs` were not swept locally: the
sweep ran out of disk, so CI's shards are their evidence.
