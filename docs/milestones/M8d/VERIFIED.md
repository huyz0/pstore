# M8d — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

All runs are in the Linux dev container; criteria 2 and 4 in a scratch copy of the tree, so
the repository was never edited to stage a failure.

1. **Exactly the entry points leave the sweep** — `cargo mutants --list` with the globs
   `scope.py mutants` prints: 3,400 before, **3,339** after. The 61 that no longer appear are
   57 under `crates/pstore-node/src/main.rs` and 4 under `crates/pstore-server/src/main.rs`,
   and no line appears after that did not appear before.
2. **The exclusion is derived, and only entry points are mutation-excluded** — in the scratch
   copy, `crates/pstore-types/src/bin/extra.rs` with a mutable `double` function: 5 of its
   mutants listed by `cargo mutants --list` without the excludes, `scope.py mutants` named the new file, and 0 listed
   through it. Marking `pstore-types` `ships = false` added `pstore[-_]types` to
   `scope.py llvm-cov` and left `scope.py mutants` unchanged.
   ⚠️ **This criterion found a bug on its first run**: `scope.py` sorted the package dicts
   themselves, which works with one `ships = false` crate and raises `TypeError` with two. The
   one-crate tree could never have shown it. Now sorted by name.
3. **`coverage.sh`'s regex is unchanged** — the old embedded derivation and `scope.py llvm-cov`,
   run on the same `cargo metadata`, compared with `cmp`: byte-identical,
   `pstore[-_]testkit|pstore-node/src/main.rs|pstore-server/src/main.rs`.
4. **A scope failure fails the gate** — with `scope.py` replaced by `sys.exit(1)` in the
   scratch copy, `./scripts/mutants.sh --check zzz_no_such_mutant` exited **1** with
   `FAIL could not derive the mutation scope`, before cargo-mutants ran.
5. **The full gate** — `./scripts/gates.sh` in the dev container, on a scratch copy holding
   exactly this milestone's tree (the uncommitted `pstore-node` work of M8e reverted): all
   fifteen gates PASS.
