# M103 — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ **Where this ran:** a Linux x86-64 cloud container. Nothing here is a measurement.

1. `./scripts/selftest-mutants.sh` prints four `ok` lines: killed, younger, early exit, TERM.
   Each was seen red first. The original `mutants.sh` left `cargo-mutants-repo-XXXX.tmp` and
   a `tmp.*` diff file. The first attempt removed a younger sweep's
   `cargo-mutants-younger.tmp`, and on an early exit the copy that was there before. The
   foreground variant was still running 5 s after a `TERM`.
2. `.github/workflows/ci.yml`'s `harness` job runs `./scripts/selftest-mutants.sh`, and
   `./scripts/check-portable.sh` passes with it outside `gates.sh`.
3. `./scripts/gates.sh` passed, run by the pre-commit hook on this commit.
