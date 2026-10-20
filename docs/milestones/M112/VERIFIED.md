# M112 — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ **Where this ran:** a Linux x86-64 cloud container. **No Windows machine was available**, so
the fix was never seen to turn the failing test green here: on Linux the old line already
gives `/`. The red is CI's, quoted in the spec, from `portable (windows-latest)` at `7e3bb01`.

1. `grep -rn 'strip_prefix(&root).unwrap().to_str()' crates` prints nothing and exits 1.
   Before M112 the same pattern found the five lines the spec names.
2. `cargo test -p pstore-engine --test external_index -p pstore-index --test external
   -p pstore-format --test lance --test lance21` passed: 64 tests, 0 failed. `git diff` on
   those four files touches only the key line and its comment.
3. NOT-RUN here: no Windows machine. This is the claim to check on CI for this commit:
   `portable (windows-latest)` gets past the engine's external-index test binary. A Windows
   failure after it is not M112's.
4. `./scripts/gates.sh` passed, run by the pre-commit hook on this commit.
