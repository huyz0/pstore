# M8a — RustFS instead of MinIO, and gates that run on three operating systems

**Serves:** **D-99/D-100** (the emulator is a measured plumbing check with a checked-in
profile), and gate-design's rule that a check which runs on one OS is a preference on the
others. Two independent tasks, one commit each.

**Depends on** [M7a](../M7a/SPEC.md) (the matrix, `conformance.sh`), [M7g](../M7g/SPEC.md)
(`byoc.sh`).

## Delta

**RustFS replaces MinIO**, as the maintainer directed, pinned to `rustfs/rustfs:1.0.0` (GA 2026-09-16).
- ⚠️ **Measured before anything changed, and it is the premise.** RustFS answers the ten
  probes **exactly as MinIO did**: `cas` and `create_if_absent` `Supported`, `aba_resistance`
  `Divergent` (MD5 ETags), durable writes admitted. M7g's ABA argument — the HEAD nonce and
  the lane registry's monotonicity — carries over, and no engine or refusal code moves.
- Changes at every site that runs it: compose, CI `test` services, `conformance.sh`,
  `byoc.sh`, `cluster.sh`, `cluster-report.py`'s store filter, the probe's label. Buckets
  are made with **`curl --aws-sigv4`**, not MinIO's `mc`. Every instance sets
  `RUSTFS_CONSOLE_ENABLE=false`: a second port per instance collides under host networking.
- `conformance.sh` skips `docker compose` when S3 already answers `/health`, so it runs
  inside the dev container, which gains the `curl`, `openssl` and `xxd` its signing needs.
- T-1's "RustFS ETag quoting mismatches (#1458)" is contradicted by 1.0.0: banner **C-16**.
- **Not changed:** C-13 and the comments citing MinIO's measured shapes — true of MinIO still.

**The gates run on Linux, macOS and Windows.** Measured on Windows before any change: every
Python gate fails (`python3` is the Store stub, exit 49); Python reads the UTF-8 tree as
cp1252; no `.gitattributes`, so autocrlf hands bash `pipefail\r`; and rustc refuses
`.cargo/config.toml`'s `split-debuginfo = "unpacked"` on MSVC, failing every cargo command.
- `scripts/lib/py.sh` finds a Python by **running** candidates and sets `PYTHONUTF8`; Python
  gates pass `encoding="utf-8"`; `.gitattributes` forces LF.
- `split-debuginfo` leaves the profile. ⚠️ Not for `target.rustflags` alone: a set
  `RUSTFLAGS` hides it, and `mutants.sh` sets one in the dev container — the workload that
  killed the VM. So `CARGO_PROFILE_DEV_SPLIT_DEBUGINFO=unpacked` in `Dockerfile.dev` (which
  `RUSTFLAGS` cannot hide), plus a Linux-scoped `rustflags` for a bare WSL2 host; the sample
  config follows. macOS already defaults to `unpacked`.
- **`scripts/check-portable.sh`** (rung 3) scans every file under `scripts/` whose first line
  is a `bash`/`sh` shebang (so `githooks/pre-commit` too), **ignoring full-line comments**. It
  refuses `python3` or a directly-run `scripts/*.py` (`lib/py.sh`, which defines `py`, is
  exempt from this rule by name), `sed -i`, `grep -P`, `base64 -w`, `readlink -f`, `date -d`,
  `stat -c`, `nproc`, `timeout`, `mapfile`, `readarray`, `declare -A`, and a fixed `/tmp/`.
  A script may declare `# portable: no -- <reason>`, **refused on `gates.sh`, the hook, and
  every script `gates.sh` runs** — derived by parsing `gates.sh`, not listed.
- Dispositions: `check-dev-env.sh` (`nproc` → `getconf`), `conformance.sh`, `coverage.sh`
  rewritten; `byoc.sh` (host networking, `timeout` as its hang guard), `cluster.sh`
  (host-network fleet, a `/tmp` file shared across invocations), `mutants.sh` (sized from
  cgroup memory) declare `portable: no`.
- **A `portable` CI job, required**: `gates.sh` on `ubuntu-latest`, `windows-latest`, and
  `macos-latest` **with `/bin` first on `PATH`** — Homebrew's bash 5 otherwise shadows
  `/bin/bash`, and the job would test bash 3.2 not at all.

## Acceptance criteria

1. The matrix has a `rustfs` section and no `minio` one; `conformance.sh --check` passes
   against a fresh run of all three emulators.
2. RustFS's `cas` and `create_if_absent` are measured `Supported` and it admits durable writes.
3. `scripts/byoc.sh` passes end to end against RustFS.
4. `scripts/cluster.sh up 5` then `converge` reports every node seeing all 5 — the roster
   CAS against RustFS.
5. `git grep -nE 'minio/(minio|mc)' -- dev scripts .github` prints nothing.
6. `check-portable.sh --selftest` rejects a fixture per forbidden form and an exemption on a
   gated script, accepts `py`, a comment naming a form, and an exempt ungated script; the
   tree passes.
7. Every non-cargo gate in `gates.sh` passes natively on Windows Git Bash, `PYTHONUTF8` unset.
8. The same gates pass under `bash:3.2` with busybox — the local proxy for macOS.
9. A clone made with `core.autocrlf=true` has only LF text files, and `build-index.py --check`
   passes in it.
10. `cargo build -v` in the dev container passes `-C split-debuginfo=unpacked` both with and
    without `RUSTFLAGS` set; the repo config no longer sets it in a profile.
11. `scripts/gates.sh` passes in full in the Linux dev container.
12. `ci.yml` has the `portable` job on three OSes with no `continue-on-error`, and
    `build-index.py --check` passes with `gates.sh` and `check-portable.sh` in the Gates table.
    ⚠️ Its green run is **not claimable here** — CI runs on push, and pushing is the
    maintainer's call. `VERIFIED.md` records it `NOT-RUN` until observed.

## Test plan

| # | Fails first | Mutation it catches |
|---|---|---|
| 1 | `--check` against the old matrix, on the renamed section | a label renamed without regenerating |
| 2 | the probe run, recorded before any edit | swapping to an emulator that cannot fence |
| 3, 4 | the old scripts with no `mc` image | a bucket step that silently makes nothing |
| 5 | the grep before the edit prints every site | a straggler still pulling MinIO |
| 6 | selftest written before the scan; each fixture observed red | a pattern deleted from the list |
| 7 | observed red on this host (exit 49) | a new bare `python3` |
| 8 | run before the fixes | a bash-4 or GNU-only form |
| 9 | the scratch clone without `.gitattributes` | the attribute removed |
| 10 | rustc's MSVC refusal reproduced; `-v` grep with `RUSTFLAGS` set | the env var dropped from the image |
| 11 | — (aggregate of the above) | any gate broken by the port |
| 12 | `build-index.py --check` with the job added and the table not | a gate run in CI and absent from the table |

## RA budget

Unchanged: emulator, harness and build configuration only.

## Risks

- **A tag is not a digest.** `conformance.sh --check` is what notices RustFS changing.
- **Windows and macOS are not run end to end here**: no MSVC linker on this host, no Mac.
  Criteria 7–8 are proxies; criterion 12's job is the evidence, once CI runs.
- **`check-portable.sh` is a grep** and catches only the listed forms.

## Tasks

- **M8a.1** — portable gates: `lib/py.sh`, UTF-8, `.gitattributes`, `split-debuginfo`,
  `check-portable.sh`, dispositions, the `portable` job, Gates-table rows.
- **M8a.2** — RustFS: compose, CI, `conformance.sh`, `byoc.sh`, `cluster.sh`,
  `cluster-report.py`, probe label, regenerated matrix, dev image tools, C-16.
