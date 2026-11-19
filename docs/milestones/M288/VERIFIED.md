# M288 verification

The spec was not sent to a separate reviewer before code. The PR's review covers it.

Test-first, honestly: the fake Azure service and the client were written together, so most
tests were not seen red against an absent method. What was seen red: the conformance test
against `object_store`'s client on the same fake, twice (the fake's listing lacked
`Last-Modified` and `Content-Type`, which `object_store` needs, and the fake answered an
`If-Match` on a missing blob with a 404 where Azurite answers 412), and the mutation sweep
below, whose survivors each got a test that kills them or a simplification that removes
them.

1. `native_azure_conformance`: the same probes and the same observed profile through
   `AzureBackend` and `ObjectStoreBackend::azure` on `pstore_testkit::fake_azure`, which
   verifies every request's SharedKey (batch sub-requests included) with pstore's signer;
   the one divergence is `suffix_read` (C-14), as on Azurite. Command:
   `cargo test -p pstore-blob --features compat --test it native_azure`, 18 passed.
2. `shared_key_matches_object_store`: `object_store`'s own SharedKey requests, every verb and
   a batch, pass the fake's check, and a wrong key fails it. Azure's documented layout:
   `the_string_to_sign_is_azures_layout` and `shared_key_is_hmac_sha256_of_the_string_to_sign`
   (a fixed key and string-to-sign). Command as 1, and
   `cargo test -p pstore-blob --features azure-native --lib azure`, 9 passed.
3. `a_batch_delete_is_one_request` (256 keys, missing ones included, in one `POST`; 257
   refused before any request; one key is a plain `DELETE`), `a_refused_batch_is_an_error`,
   and `a_batch_answer_names_the_keys_that_failed` (a sub-request's failure names its key, a
   404 is not a failure). Command as 2's second, and as 1 with `--features azure-native`.
4. The emulator run, `./scripts/conformance.sh` on 2026-11-19 against Azurite 3.34.0 in a cloud
   container, `provisional` (D-99). The probe has an `azurite-native` section beside
   `azurite` (`docs/profiles/capability-matrix.md`): all thirteen probes and every measured
   field agree, and both admit durable writes. The declared batch limit differs, 256 against
   1000: the probe's `azurite` entry wraps the object_store client with generic capabilities
   rather than the Azure constructor's 256. It is declared, not measured.
   So the default is `native`.
5. `managed_identity_token_from_imds`, `managed_identity_token_from_identity_endpoint`,
   `workload_identity_exchanges_the_federated_token`, `client_secret_token`, each against a
   fake that records the request, and `a_bearer_token_signs_instead_of_the_key`. The chain:
   `the_chain_takes_the_first_provider_that_is_configured` (the correction banner), and
   `token_answers_name_their_expiry_or_are_refused`. Commands as 2's second and 3's second.
6. On 2026-11-19: `cargo tree -p pstore-server -e normal -i object_store`, and the same with
   `-e dev` and with `-i humantime`, each answer "did not match any packages". With the
   humantime ignore removed from `deny.toml`, `cargo deny check advisories` passes (exit 0)
   on default features, and `cargo deny --all-features check advisories` reports
   RUSTSEC-2026-0542. CI's cargo-deny step scans all features, so the ignore stays, reworded,
   as the correction banner records. The chrono ignore stays for `parquet`.
7. `both_blob_clients_serve_the_same_answers` runs the same calls through both clients on the
   fake S3 and the fake Azure, under `--features object-store-client` (so under CI's
   `--all-features`). `object_store_mode_needs_its_feature` checks whichever build it is in:
   with the feature, the mode opens; without it, `PSTORE_BLOB_CLIENT=object_store` is refused
   with `OBJECT_STORE_ABSENT`, which names the feature. CI runs only the first branch. The
   second was run here: `cargo test -p pstore-server --test it -- blob_client s3_retry`
   (default features), 5 passed. Also `the_native_azure_client_takes_a_token_without_a_key`
   and, in `tests/azure.rs`, `the_native_azure_store_builds_offline_and_names_its_account`;
   the existing `object_store` Azure test keeps its assertions behind the feature. Command:
   `cargo test -p pstore-server --all-features --test it -- blob_client s3_retry`, 13 passed,
   and `cargo test -p pstore-server --all-features --test azure`, 2 passed (2 ignored: the
   Azurite tests, which need the emulator).

Mutations: `cargo mutants --in-diff` over this milestone's diff in `pstore-blob` and
`pstore-testkit`, tested by `pstore-blob`'s suite, on 2026-11-19: 298 mutants, 165 caught,
106 unviable, **25 missed** and 2 timeouts. The misses, and what answered each:
- `AzureBackend::send` sent `content-length` only with a body or on PUT and POST (5
  mutants of that condition): it now always sends it, a zero signed as empty.
- `ranged`'s answer to a `200` (a server that ignores the range, 4 mutants) was never exercised: the fake
  can now ignore ranges, and `a_server_that_ignores_ranges_still_answers_the_range`.
- The signer's `x-ms-` filter, `k.len() > 5` to `>=` (equivalent): now `k.get(..5)`.
- Fifteen in the fake, and both timeouts (the listing's marker and page count). Removed as unused by either client: `+` decoded as a space, the
  `comp=` guards on the container's two operations, a separate arm for an `If-Match` on a
  missing blob (the mismatch arm already answers it), open-ended ranges. Tested:
  `a_dropped_fake_stops_answering`, the second half of
  `a_conditional_write_that_lands_and_then_gets_a_503_is_not_repeated` (a fault that did not
  land stores nothing), `the_fake_refuses_batches_the_service_refuses` (an empty batch, 257
  parts, a sub-request it cannot verify), `the_fake_lists_one_level_a_page_at_a_time`.

The re-run over the same files: 282 mutants, 175 caught, 106 unviable, none missed, 1
timeout, the listing's marker comparison (`<` to `==`), whose wrong pages sent the client
round in a loop. The fake now starts the page with a range from the marker, and
`cargo mutants -f crates/pstore-testkit/src/fake_azure.rs --re 'in list'` (no config,
tested by `pstore-blob` with `azure-native`): 8 mutants, 8 caught.
The mutants in `pstore-server` were not swept locally, as in M287: CI's shards are their
evidence.

Whole suite: `cargo test --all-features --no-fail-fast` on 2026-11-19, 3,059 passed and 2
failed: `ipv6_clients_refuse_malformed_replies_and_release_wire_capacity` and
`serialization_failures_never_send_partial_requests_or_responses`, which bind `[::1]` and fail
because this container has no IPv6, as in M287. CI runs them.
