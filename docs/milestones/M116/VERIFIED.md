# M116 — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ **Where this ran:** a Linux x86-64 cloud container, 4 cores. The failure this fixes is
Windows-only. **No red was observed here**: Linux refuses a closed loopback port at once, so
the old fixture passes on Linux. The red on record is CI's: `portable (windows-latest)` at
`a9e1c03`, `two failed shares took 3.6344759s`.

1. `peers::failed_shares_are_run_here_together` passes, 3 of 3, each run of the filtered
   binary finishing in 2.58 s, with its `before + 2` failed-part assertion and its 2.9 s bound
   unchanged. clippy and fmt pass for `pstore-server`.
2. NOT-RUN: Windows. This needs CI's `portable (windows-latest)` job on the commit after
   M116, and is read there, not here.
3. `./scripts/gates.sh` passed, run by the pre-commit hook on this commit.
