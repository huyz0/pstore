# M112 — Lance fixtures are keyed with `/` on Windows

**Serves:** CI's `portable (windows-latest)` job, red on `main` at `7e3bb01`. No backlog row:
this is a test-harness defect. No shipping code changes.

## What is true today

- **Five test fixtures turn a file path into a blob key with `to_str()`.** Each walks a Lance
  dataset under `tests/data/`, strips the dataset's root, and writes the file under
  `lake/<name>/<rel>`:
  `pstore-engine/tests/external_index.rs`, `pstore-index/tests/external.rs` (twice),
  `pstore-format/tests/lance.rs` and `pstore-format/tests/lance21.rs`.
- **On Windows `rel` is `_versions\latest_version_hint.json`.** A key is a `/` path, so the
  reader asks for `lake/one/_versions/latest_version_hint.json` and finds nothing.
  `windows-latest` at `7e3bb01`:
  `register_then_query_by_name_equals_the_library` panicked at `external_index.rs:94:14` with
  `External(Source("lance object: no such key: lake/one/_versions/latest_version_hint.json"))`.
  `cargo test` stops at the first failing binary, so the other four never ran there. They
  build their keys the same way.
- **Shipping code is not involved.** The reader derives every key it asks for. Only the
  fixtures that load the dataset into a `MemoryStore` start from a filesystem path.

## Delta

1. Each of the five fixtures joins `rel`'s components with `/`, so the key is the same on
   every OS. Everything else that reads `rel` keeps working: the `.manifest` suffix test and
   the manifest ordering compare names, and no separators.

**Not changed:** the datasets, the assertions, and every crate's `src/`.

## Acceptance criteria

1. **No fixture builds a key from a path's `to_str()`.**
   `grep -rn 'strip_prefix(&root).unwrap().to_str()' crates` prints nothing.
2. **The five fixtures pass where they did.** On Linux, `external_index`, `external`, `lance`
   and `lance21` pass, unedited apart from the key line.
3. **They pass on Windows.** CI's `portable (windows-latest)` job gets past `cargo test`'s
   external-index test binary in `pstore-engine` on the commit after M112. ⚠️ Red is observable only there: on Linux
   and macOS `to_str()` already gives `/`, so no local run can fail on the old line.
4. `./scripts/gates.sh` passes.
