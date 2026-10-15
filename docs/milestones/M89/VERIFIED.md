# M89 — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ **Where this ran:** a Linux x86-64 cloud container, in-memory store, the real pylance datasets.

Command: `cargo test -p pstore-format --test lance`: 14 tests.

1. `the_hint_names_the_newest_version`: `deleted`'s hint `{"version":2}` gives manifest
   `18446744073709551613.manifest`, the one the test helper picks as newest, and
   `pin_lance_latest` finds row 0 deleted; `many/` with a trailing slash gives its one manifest.
2. `a_hint_that_lies_is_refused_never_guessed_around`: each of the twelve malformed, absent,
   oversized and dangling hints is an error; the 1,024-byte hint is read.
3. **Gates:** `./scripts/gates.sh`, green by the pre-commit hook on this commit. **Mutation**
   (`cargo mutants`, `--no-config`, changed lines): first sweep 22 mutants, 2 missed (the size
   boundary; an equivalent empty-string check, removed); re-sweep: **20 mutants, 19 caught, 1
   unviable, 0 missed.** ⚠️ `cargo deny check advisories` still fails on `paste` (via `foyer`),
   as before.

**Not covered:** Lance 2.1+; nested Parquet columns.
