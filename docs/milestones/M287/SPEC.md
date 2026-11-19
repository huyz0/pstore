# M287: the S3 credential chain, and the server's S3 client as a setting

## Serves
D-1 and D-2 ([rust-object-store-crates.md](../../research/02-object-storage/rust-object-store-crates.md)),
and M286's plan ([object-store-layer-2026-11-18.md](../../comparisons/object-store-layer-2026-11-18.md)):
the native client cannot be the server's S3 client until it finds credentials on EC2, ECS
and EKS the way `object_store` does.

## Delta
1. **`pstore_blob::s3` gains a credential chain**, tried in `object_store`'s order, first
   that answers wins:
   - static credentials the operator gave (`PSTORE_ACCESS_KEY` / `PSTORE_SECRET_KEY`, as today);
   - `AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY` / `AWS_SESSION_TOKEN` (M286's);
   - **web identity** (EKS IRSA): `AWS_WEB_IDENTITY_TOKEN_FILE` + `AWS_ROLE_ARN`, exchanged
     with STS `AssumeRoleWithWebIdentity` (`AWS_ROLE_SESSION_NAME` optional, STS endpoint
     regional, `AWS_ENDPOINT_URL_STS` overrides);
   - **ECS container credentials**: `AWS_CONTAINER_CREDENTIALS_RELATIVE_URI` (from
     `169.254.170.2`) or `_FULL_URI`, with `AWS_CONTAINER_AUTHORIZATION_TOKEN` or `_FILE`;
   - **IMDSv2** (EC2 instance profile): a `PUT` session token, then the role name, then its
     credentials; never IMDSv1. `AWS_EC2_METADATA_DISABLED=true` skips it.
   Temporary credentials are cached and refreshed **before** they expire (5 minutes ahead),
   by one request at a time; a request never waits on a refresh unless the cached ones have
   already expired. A refresh that fails keeps unexpired credentials and is retried.
   Timestamps are parsed in-crate (no `chrono`): RFC 3339 `Z` times, as all three return.
2. **The server's S3 client is a setting**, `PSTORE_BLOB_CLIENT=native|object_store`, in
   `docs/deploy.md`'s Environment table (one name for every backend: M288 extends it to
   Azure; until then Azure ignores it). It applies to the S3 backend and to every
   replication source (`PSTORE_SOURCE_*`). An unknown value is refused at startup, as
   `PSTORE_BACKEND`'s is. `pstore-node` constructs the native client.
   - **The default is `native`** if criterion 4 holds: M286's bench is the measurement
     (55-85% fewer bytes allocated a request on five of seven patterns, CPU no worse than
     noise), and this milestone adds the emulator run it lacked.
   - **No index-level override, and why**: the client is a transport the process opens once
     for its one store; it changes no byte on disk and no answer, only the server's CPU and
     allocations, so there is nothing per index to choose. An index-level override would
     mean two clients of one bucket in one process with nothing to tell them apart.
     Rollback is the setting itself: either client reads what the other wrote.
3. **No retries in the native mode**, as M286 specified: `Congested` is the one retry layer.
   `object_store` mode keeps M282's two clients exactly as they are.

**Not changed:** Azure (M288), on-disk bytes, the `BlobStore` trait. `UNSIGNED-PAYLOAD`
stays out: neither client sends it and it is not needed to switch.

## Acceptance criteria
1. Each provider answers from a fake of its endpoint, with the request AWS documents:
   `imds_v2_takes_a_token_then_the_role_then_its_credentials`,
   `ecs_credentials_come_from_the_relative_uri_with_its_token`,
   `web_identity_is_exchanged_with_sts`, and the chain's order and fall-through:
   `the_chain_takes_the_first_provider_that_answers`.
2. Credentials near expiry are refreshed once, before use, and a failed refresh keeps
   unexpired ones: `credentials_refresh_before_they_expire`,
   `a_failed_refresh_keeps_credentials_that_have_not_expired`.
3. RFC 3339 parsing matches `amz_date` for every second of a span: `rfc3339_round_trips`.
4. The emulator run: `scripts/conformance.sh` probes `rustfs` with both clients and the
   capability matrix rows agree; recorded in VERIFIED.md. If they disagree, the default
   stays `object_store` and VERIFIED.md says which probe differs.
5. Both modes serve the same server: `blob_client_setting_is_parsed_and_refused_when_unknown`,
   and the server's S3 tests run once per mode (`both_s3_clients_serve_the_same_answers`).

## Test plan
- 1, 2: the fakes count requests and record headers; written first and red until each
  provider exists (mutations: IMDSv1 fallback, skipping the token header, refreshing on
  every request).
- 3: written against `amz_date` before the parser (mutation: a day off at a month end).
- 5: the existing S3 server tests pin `object_store` explicitly and keep every assertion;
  the same tests run under `native`.

## RA budget
Unchanged per request. Credential requests are per process and per expiry (about one an
hour), never per request or per index.

## Risks
- IMDS hop limits and ECS on Fargate differ in the field; the fakes test the protocol, not
  AWS. `provisional` until a real EC2/EKS run.
- `rustfs` is not S3 (D-99): the conformance run tests plumbing.

## Tasks
- M287.1 SPEC.
- M287.2 the credential chain and its fakes.
- M287.3 the setting, both modes in the server's tests, the conformance run, VERIFIED.md.
