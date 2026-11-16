# M272: the turbopuffer compatibility manifest

## Serves
Workstream B of [the delivery plan](../../comparisons/turbopuffer-next-plan.md), its
completion clause: "Maintain an explicit compatibility manifest linking supported
operations/variants to fixtures; never advertise 'compatible' based only on JSON
validation." The facade is M268 to M271. The pinned client is still `turbopuffer` 3.1.0.

## Delta
1. **The manifest**, `api/compat/turbopuffer.json`. It names the SDK, pinned by version and
   wheel hash, and holds three maps:
   - `operations`: every public method of the SDK's namespace resource, and the client's
     `namespaces`. Each has its `route` (method and path), a `status` (`served` or
     `refused`), the `native` call it becomes, and its `evidence`: the tests that run it.
     An SDK test is written `sdk:<name>` and a Rust test `rust:<name>`.
   - `write_keys`: every key of the SDK's write parameters, each `served`, `hint`
     (accepted and dropped, with nothing in pstore to mean) or `refused`, with a `note`
     where the mapping is not a pass-through.
   - `query_keys`: the same for the SDK's query parameters.

   It also lists what a retry of each write can do, as the gap list asked
   ([item 5](../../comparisons/turbopuffer-gaps-2026-11-15.md)). The SDK resends a 408, 409,
   429 or 5xx, and a conditional or by-filter write applied before a 5xx is evaluated again
   against the rows as they are then. Nothing in this item changes what a request does.
2. **Held to the code, both ways.** Three checks, each a predicate over files or a run:
   - The routes the server registers with `.facade(` in `src/lib.rs` are exactly the
     manifest's routes. A route added without a manifest entry fails, and so does an entry
     with no route.
   - Each manifest key's status is what `write_body` and `query_body` do. A `refused` key is
     refused with the facade's "is not translated" error, naming it. A `served` or `hint`
     key never is.
   - Every `operations` entry has evidence, and every test it names exists:
     `def <name>(` in `clients/turbopuffer/test_sdk.py`, or `fn <name>(` in the facade's Rust
     tests.
   - In the SDK run (`scripts/facade.sh`), every public method of the pinned SDK's
     namespace resource, and every key of its write and query parameters, is in the
     manifest. A new SDK version with a new method or key fails until the manifest says what
     the facade does with it.

**No setting.** A file and checks: nothing a request sends or receives changes.

## Acceptance criteria
1. **The manifest is complete against the SDK** (`./scripts/facade.sh`; Python test names
   carry no code marks): test_the_manifest_names_everything_the_sdk_sends passes, and is
   red with a method or a key removed from the manifest.
2. **The manifest is the code:**
   - `the_manifest_is_the_facades_routes` in `tests/it/facade.rs`: red with a route
     removed from the manifest, and red with an extra route in it;
   - `the_manifest_keys_are_the_translation` in `src/facade.rs`: red with a served key
     marked refused, and red with a refused key marked served;
   - `the_manifest_evidence_resolves` in `tests/it/facade.rs`: red with a test name
     misspelt.
3. **Gates:** fmt, clippy, the workspace tests, `scripts/facade.sh`, and each red run above
   observed.

## Test plan
Each criterion's red run is an edit to the manifest, made by hand, run and reverted.

## RA budget
None: nothing on a request path changes.

## Risks
- **A manifest is only as true as its checks.** Statuses are held to `write_body` and
  `query_body`, and routes to the registration, both mechanically. The `native` and `note`
  fields are prose, and a reviewer reads them as prose.

## Tasks
- **M272.1** The spec.
- **M272.2** The manifest, its three checks, the SDK completeness test, docs.
