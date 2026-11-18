# M275 verification

The spec was not sent to a separate reviewer before code; the PR's code review covers it.
The SDK test is Python, so it is named without code marks.

Tests: `cargo test -p pstore-server --test it facade` (19 passed), and
`FACADE_PYTHON=python3.13 ./scripts/facade.sh` (8 OK).

Red runs. With the new tests and the M274 facade, `facade_schema_lists_every_recorded_declaration`
failed on its write (`"big" is declared {"type":"uint64"}`, refused), and the SDK test failed
with `[] != ['id', 'title', 'vector']`. Mutations, each applied by a script and reverted, all
killed by the Rust test: `uint64` not renamed `uint`; `[]datetime` not mapped; `ann: true`
kept; the primary text field given `type: "string"` (it first survived: every text field in
the test was a further one, so the test gained a namespace whose `text` is primary, and it then
failed); metadata answering the old `attributes`; a named field typed at `vector`'s width.

1. `facade_schema_lists_every_recorded_declaration`: green. The `typed` namespace answers
   `id` `uint`, `vector` `[2]f16` with the cosine metric under `ann`, `title` and `body` with the
   native analyzers (`body`'s `stemming: true`), `path` `regex: true`, `at`, `big` and `seen`
   as `datetime`, `uint` and `datetime`; `metadata()`'s `schema` equal.
2. `facade_schema_lists_every_recorded_declaration`: the `named` namespace answers `vector`
   `[2]f32` and `img` `[3]f32`, both under the index's euclidean metric, and its primary text field's analyzer, no type.
3. `facade_schema_lists_every_recorded_declaration`'s first write, with `uint`, `[]datetime`, `{"type": "uint"}` and
   `{"type": "[2]f16", "ann": true}`, answered 200 (refused 400 before).
4. `./scripts/facade.sh`, test_the_sdk_against_the_facade: green; `schema()` holds `id`,
   `title` and `vector` as SDK models, `title.type` is `None`.

Gates: `cargo fmt --check` and `cargo clippy -p pstore-server --all-targets -- -D warnings`
pass. Coverage, mutants and `cargo deny` are CI's, NOT-RUN here.
