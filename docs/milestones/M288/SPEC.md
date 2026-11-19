# M288: pstore's own Azure client, and `object_store` out of the default build

## Serves
D-1 and D-2, M286's plan ([object-store-layer-2026-11-18.md](../../comparisons/object-store-layer-2026-11-18.md)),
and RUSTSEC-2026-0542 (humantime): the server keeps linking `object_store` while Azure
needs it, and `cargo deny` keeps its ignore while anything in the default build does.

## Delta
1. **`pstore_blob::azure::AzureBackend`** behind a new feature `azure-native`: a
   `BlobStore` over the Blob REST API on M286's `hyper` client. No new crates.
   - Put Blob (block blob, one request), Get Blob with `x-ms-range`, Get Blob Properties,
     Delete Blob, List Blobs for the listing the trait allows; conditional writes by
     `If-Match` / `If-None-Match: *`; `delete_batch` as **one** Blob Batch request of at most
     256 sub-requests (`max_batch_delete: 256`, as `ObjectStoreBackend::azure` declares).
   - The capabilities `ObjectStoreBackend::azure` declares, unchanged: no suffix reads
     (C-14), deletes billed.
   - No retries; errors typed from the status and `x-ms-error-code`: 404 absent, 412
     `Lost`, 409 `Contended`, 503 or `ServerBusy` throttled.
   - **Credentials**, first that answers: the account key (`PSTORE_AZURE_KEY`, SharedKey,
     HMAC-SHA256 with `ring`); a client secret (`AZURE_CLIENT_ID`, `AZURE_TENANT_ID`,
     `AZURE_CLIENT_SECRET`); **workload identity** (`AZURE_FEDERATED_TOKEN_FILE` with
     `AZURE_CLIENT_ID` and `AZURE_TENANT_ID`, `AZURE_AUTHORITY_HOST` overrides); **managed
     identity** (`IDENTITY_ENDPOINT` + `IDENTITY_HEADER` on App Service and Container Apps,
     else IMDS at `169.254.169.254`). Tokens are cached and refreshed before expiry, by
     M287's rule. No certificate credentials, so no `rsa`.
2. **The client setting covers Azure**: M287's `PSTORE_BLOB_CLIENT=native|object_store`
   now chooses the Azure client too. Default `native` by M287's rule, if criterion 4 holds
   for Azurite. No index-level override, for M287's reason.
3. **`object_store` leaves the default build.** The `object_store` mode stays, behind a
   non-default `pstore-server` feature `object-store-client`; a server built without it
   refuses `PSTORE_BLOB_CLIENT=object_store` at startup, by name. CI builds and tests with
   `--all-features`, so both modes stay tested. `pstore-blob`'s own tests of
   `ObjectStoreBackend` move behind its `object_store` feature, so the crate's
   dev-dependencies no longer name `object_store`.
4. **`deny.toml`**: the humantime ignore (RUSTSEC-2026-0542) is removed once `cargo deny
   check` passes without it. The chrono ignore (RUSTSEC-2026-0543) stays while `parquet`, a
   dev-dependency of M235's export tests, brings `chrono`; its reason is reworded to say so.

**Not changed:** GCS (a probe target only, through `object_store` behind `gcs`), on-disk
bytes, the `BlobStore` trait.

## Acceptance criteria
1. `AzureBackend` passes the conformance cases `ObjectStoreBackend::azure` does against an
   in-process fake Blob service that verifies SharedKey: `native_azure_conformance`.
2. SharedKey signing matches a request signed by `object_store`'s Azure client against the
   same fake, and Azure's documented string-to-sign: `shared_key_matches_object_store`.
3. A batch delete is one request of at most 256 deletes, and a sub-request's failure is
   reported for that key: `a_batch_delete_is_one_request`.
4. The emulator run: `scripts/conformance.sh` probes Azurite with both clients and the
   matrix rows agree; recorded in VERIFIED.md. Else the default stays `object_store`.
5. Each token provider answers from a fake of its endpoint:
   `managed_identity_token_from_imds`, `managed_identity_token_from_identity_endpoint`,
   `workload_identity_exchanges_the_federated_token`, `client_secret_token`.
6. `cargo tree -p pstore-server -e normal` and `-e dev` with default features name neither
   `object_store` nor `humantime`, and `cargo deny check advisories` passes without the
   humantime ignore: recorded in VERIFIED.md.
7. Both modes serve the same server, Azure and S3: `both_blob_clients_serve_the_same_answers`
   runs under `--all-features`, and a default build refuses `object_store` by name:
   `object_store_mode_needs_its_feature`.

## Test plan
- 1-3: against `ObjectStoreBackend::azure`'s behaviour first, red until each method exists
  (mutations: a batch split in two, a missing `x-ms-date` in the string-to-sign).
- 5: the fakes record the request; red until each provider exists.
- 7: the existing Azure server tests pin `object_store` mode and keep every assertion.

## RA budget
Unchanged: one request per trait call, a batch delete one request, as today.

## Risks
- Azurite is not Azure (D-99). `provisional`.
- The Blob Batch API is multipart/mixed; Azurite's support for it is what the emulator run
  shows.

## Tasks
- M288.1 SPEC.
- M288.2 `AzureBackend`, its signer, token providers, fakes, tests.
- M288.3 the setting, `object_store` behind a feature, `deny.toml`, the emulator run,
  VERIFIED.md.
