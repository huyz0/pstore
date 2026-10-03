# M46 — An Azure backend

**Serves:** [BACKLOG](../BACKLOG.md) row 32's remainder, after [M45](../M45/VERIFIED.md) made the
format decision: the Azure adapter, and what it does where HEAD knows no segment length.

## What is true today

- `PSTORE_BACKEND` takes `memory` or `s3`. `azure` is refused by name (`ConfigError::Backend`).
  `tests/deploy.rs::a_backend_or_profile_it_does_not_know_is_refused_by_name` uses `azure` as
  its unknown backend. The server's `Backend` comment and `docs/deploy.md` say why.
- `pstore-server` builds `object_store` with `aws` only, and its `Cargo.toml` argues against
  `azure`. `pstore-blob` reaches `object_store/azure` only through its `compat` feature, for the
  probe example.
- `ObjectStoreBackend::get_suffix` sends `GetRange::Suffix`, which the Azure client refuses
  before building a request (C-14).
- **Measured against Azurite 3.34.0** by `scripts/conformance.sh`
  ([capability-matrix.md](../../profiles/capability-matrix.md), provisional): every probe
  Supported but `suffix_read`. `object_store` 0.14.1 sends `If-None-Match: *` for a create
  and `If-Match` for an update on Azure (`client.rs` 759-763).
- **The Azure client splits a batch delete into requests of 256** (`try_chunks(256)`).
  `Accounted` bills one `Delete` per `delete_batch`, and `unprobed()` declares
  `max_batch_delete: 1000`. So a 1,000-key batch is 4 requests billed as 1.
- **Only `as_of` reaches a suffix read on Azure** (spec review). No build before this one
  could be configured for Azure, so there is no pre-M45 HEAD there. Refs reconstructed for
  `as_of` (head.rs ~885-907) carry `len: None`, because they come from HEAD's graveyard
  (`graveyard: BTreeMap<u64, Vec<String>>`), which records keys and no lengths.
- `Accounted` bills one read per call. So a suffix read emulated *inside* the adapter, as
  `head` then a range, would be billed as one read for two requests and a sequential round
  trip.
- `Caching::head` is not cached.
- With `with_use_emulator(true)`, `MicrosoftAzureBuilder` ignores `with_endpoint` and reads
  `AZURITE_BLOB_STORAGE_URL` (builder.rs 1242-1260). So `probe.rs`'s `PSTORE_AZURE_ENDPOINT`
  does nothing. Outside emulator mode, `build` requires an account, and Azurite is reached at
  `http://127.0.0.1:10000/devstoreaccount1`.
- `pstore-node` hard-codes an S3 builder.

## Delta

1. **`Capabilities` gains `suffix_read: bool`.**
   - `unprobed()` declares `true`, as every backend but Azure has it, and `s3_capabilities` and
     `probe.rs` inherit that.
   - The conformance report **measures** it: `observed.suffix_read` is whether the
     `suffix_read` probe was Supported, never the declared value. The matrix gains the row,
     and its header says three of seven fields are measured.
   - ⚠️ The matrix cannot be regenerated here (no Docker for `rustfs` and `fake-gcs-server`).
     The new rows are written from each backend's `suffix_read` probe row, which the matrix
     already records, and the ledger says so. `conformance.sh --check` is not run. The
     matrix says "do not edit by hand", so the ledger also confirms that the generator now
     emits exactly the rows written, and the next `--check` finds no diff from this edit.
2. **`Segment::open_at(store, key, None)` on a store whose `capabilities().suffix_read` is
   false makes a `head`, then reads the range M45 reads with a known length.**
   - That is two sequential requests, both billed: C-14's exit 3, confined to `as_of`.
   - With a length nothing changes, and on a store with suffix reads nothing changes.
   - ⚠️ **Rejected alternative, and why** (spec review, round 2): record lengths in the
     graveyard too, as Pattern 7 says, and refuse an unknown length outright. The graveyard
     is in HEAD, so this would be one more trailing section, as M45's was. It is rejected for
     three reasons:
     - **Cost.** It is a varint on every buried segment, read with every HEAD by every query,
       until GC reaps the key. The `head` costs only the `as_of` reads that open those
       segments.
     - **Not every burial knows a segment's length.** Only segments go through `open_at`.
       Bundles and delete vectors need none. But segments are buried from seven sites, and
       some bury a name a failed seal may or may not have written (M19), with no length to
       record. A refusal would still need this fallback, or would break `as_of` there.
     - **The path is rare.** `as_of` is not the hot path Pattern 7 protects.

     This is a cost judgement, so it is named for a later milestone to revisit if `as_of` on
     Azure turns out to be hot.
   - ⚠️ **A warm cache still pays the `head`**, because `Caching::head` is not cached. Stated,
     not fixed: an `as_of` read is not the hot path Pattern 7 protects.
3. **`ObjectStoreBackend::azure(...)`**, a constructor beside `new`, whose capabilities
   declare:
   - `suffix_read: false`;
   - `max_batch_delete: 256`, so one `delete_batch` is one request and billed as one;
   - `delete_is_free: false`;
   - the profile's `cas` and `create_if_absent`, as S3's are.
4. **`PSTORE_BACKEND=azure`.** `pstore-server` adds the `object_store` `azure` feature, and its
   `Cargo.toml` comment says why. `cargo deny check` must pass on the new dependencies.
   - `PSTORE_AZURE_ACCOUNT`: **required**, refused when absent (`ConfigError::AzureAccount`).
   - `PSTORE_AZURE_KEY`: optional. Unset, the builder's own credential chain (managed
     identity), as S3's provider chain is.
   - `PSTORE_AZURE_CONTAINER`: default `pstore`.
   - `PSTORE_AZURE_ENDPOINT`: optional. Set, it is the full URL up to the container, used as
     given with HTTP allowed. That is how Azurite (`http://127.0.0.1:10000/devstoreaccount1`)
     and a private endpoint are reached. Unset, the account's public endpoint. **Emulator
     mode is never used**, because it ignores the endpoint.
   - The profile rule is S3's: `unprobed` refuses durable writes, and `conforming` is the
     operator's claim.
   - The `Backend` comment, `ConfigError::Backend`'s message and `docs/deploy.md` are
     corrected.
5. **Not changed, and said so:** replication sources stay S3-compatible, and `pstore-node`
   stays S3-only.

## Acceptance criteria

1. **A store without suffix reads opens an unknown-length segment by `head` then range.**
   Over an `Accounted` `MemoryStore` wrapper whose `capabilities().suffix_read` is false, and
   whose suffix reads fail, `Segment::open_at(…, None)` opens a segment larger than 8 KiB and
   one smaller.
   - Each open bills 2 reads, and its bytes equal the known-length open's bytes.
   - The known-length open bills 1 read and makes no `head`.
   - Test: `pstore-format` `reader::tests::without_suffix_reads_an_unknown_length_is_asked_first`.
2. **`as_of` answers on such a store.** An engine over it answers an `as_of` query at an
   epoch before a compaction, over segments the graveyard reconstructs, the same as over a
   plain store. Test: `pstore-engine` `tests/lengths.rs::a_store_without_suffix_reads_answers_as_of`.
3. **Every store says what it has, and the conformance suite measures it.**
   - `ObjectStoreBackend::azure`'s capabilities give `suffix_read: false` and
     `max_batch_delete: 256`.
   - `MemoryStore`'s and `unprobed()`'s give `true`.
   - The conformance report over a store whose suffix reads fail gives `observed.suffix_read`
     false while its declared value is true.
   - Tests: `pstore-blob` `object_store_backend::tests::azure_declares_no_suffix_and_batches_of_256`
     (built under the `object_store` feature, as that module is: run with
     `cargo test -p pstore-blob --features object_store`), and `pstore-testkit`
     `tests/conformance.rs::suffix_read_is_measured_not_declared`.
4. **Configuration.** In `crates/pstore-server/tests/deploy.rs`:
   - `azure_is_configured_with_an_account`: `PSTORE_BACKEND=azure` parses with the variables
     above, and the endpoint and container are kept as given. With no account it is refused
     by name, and a key alone is not an account.
   - `a_backend_or_profile_it_does_not_know_is_refused_by_name` names `gcs` where it named
     `azure`.
5. **End to end against Azurite (provisional).** `./scripts/azurite.sh` starts Azurite 3.34.0,
   creates the container, and runs `pstore-server`'s ignored test
   `writes_folds_queries_and_reads_as_of_against_azurite`, then stops Azurite.
   - The test, through `Api` over `ObjectStoreBackend::azure` with the `conforming` profile:
     writes, folds, queries all three legs, compacts, and reads `as_of` the epoch before the
     compaction.
   - The test reads `PSTORE_AZURE_KEY`, which the script sets to Azurite's published
     `devstoreaccount1` key. Unset, the key path is managed identity, which cannot reach
     Azurite.
   - It uses `node_modules/.bin/azurite` when present and installs it with npm when not.
     `conformance.sh` uses Docker, and this second route exists because the environments
     that develop this repo do not all have a Docker daemon (this one does not). It declares
     `# portable: no`, and runs outside `gates.sh` as `conformance.sh` does.
   - ⚠️ Emulator evidence. It says nothing about Azure's latency, cost, or CAS under
     contention, which are still M0b's.
6. **Every existing test passes unchanged, except** for the new field in `Capabilities`
   literals and criterion 4's renamed unknown backend. `./scripts/gates.sh`, and `cargo deny
   check`.
7. **Mutation:** the incremental sweep of the changed lines misses 0. `./scripts/mutants.sh`.
