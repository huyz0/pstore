# M8a — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ **Where each ran.** This host is Windows with Docker Desktop. It had **no MSVC linker**
while the milestone was built, so every cargo-bearing line below ran in the Linux dev
container (`pstore-dev-dev`), and every Docker line ran from Git Bash on the host. After the
close, Visual Studio Build Tools 2022 (17.14.41) was installed and the full gate was run
natively too — criterion 7. There is no Mac. Criterion 12's green run is CI's, and CI runs
on push.

1. **The matrix names `rustfs`, and `--check` agrees with a fresh run** —
   `./scripts/conformance.sh --check`, run from the dev image on host networking against the
   compose stack, first **FAILED** against the old matrix with a diff of exactly two lines,
   `## minio` → `## rustfs` and the declared backend label. Regenerated with
   `./scripts/conformance.sh`, then `--check` printed `ok the matrix matches a fresh run`.
   ⚠️ The first attempt was red for a second reason, and a real one: the new bucket step
   ended in `|| true`, and Debian bookworm's curl 7.88 does not send the payload-hash header
   S3 requires, so RustFS answered 400 and the matrix recorded `rustfs` UNREACHABLE with
   `NoSuchBucket`. The header is now sent and the status asserted.
2. **RustFS can fence** — measured **before any file changed**, by running the probe against
   a scratch RustFS 1.0.0 (`cargo run -p pstore-blob --features compat --example probe`):
   create-if-absent and compare-and-swap Supported, ABA Divergent (MD5 ETags), durable writes
   admitted — the same ten outcomes MinIO had. The regenerated matrix records it. Also by hand
   with a signed curl: second create-if-absent 412, stale If-Match 412, current If-Match 200.
3. **Two containers, one bucket, against RustFS** — `./scripts/byoc.sh` printed `byoc ok`
   with every arm green: the unprobed refusal naming backend and primitive, the missing lane,
   A's durable write invisible to B before the fold and visible after, B's write on lane 2
   seen by A, `/metrics` with zero LISTs, and the duties list matching `docs/deploy.md`. The
   profile shapes stay pinned by `an_unprobed_s3_profile_cannot_fence_and_a_conforming_one_can`.
   ⚠️ **Red first, for a reason no spec named**: Git Bash rewrote the container argument
   `/data` into `C:/Program Files/Git/data`, RustFS exited "Volume not found", and the script
   reported only curl's exit 7. Both Docker harnesses now export `MSYS_NO_PATHCONV=1`.
4. **The roster CAS against RustFS** — `./scripts/cluster.sh up 5` then
   `./scripts/cluster.sh converge`: `converged: every node sees 5 in 11433ms = 57 periods
   (provisional)`. Nodes learn their seeds only from the roster, so a store that could not
   hold it would not converge.
5. **No MinIO image or client left where anything runs** — `grep -rnE 'minio/(minio|mc)' dev scripts .github`
   printed **six sites** on the pre-change tree (compose, CI, two in each of the two Docker
   harnesses) and nothing after.
6. **The portability grep refuses and accepts what it should** —
   `./scripts/check-portable.sh --selftest`: 28 cases. **Mutation verified killed**: with the
   scan body replaced by `return 0` the selftest fails on its first case. Its exec-bit rule
   was written after review found `check-portable.sh` itself staged 100644, and was observed
   red on that file before the mode was fixed. `./scripts/check-portable.sh` passes on the tree.
7. **Every non-cargo gate natively on Windows** — the twelve non-cargo lines of
   `./scripts/gates.sh`, each run from Git Bash with `PYTHONUTF8` unset: all PASS. Red first,
   on this host, before any change: every Python gate exited 49 (the `python3` Store stub).
   And without the helper, `PYTHONUTF8=0` on the old `check-slos.py` raised
   `UnicodeDecodeError: 'charmap' codec` and the new one passes.
   **And then all of it, cargo included**: with MSVC Build Tools 17.14.41 installed,
   `./scripts/gates.sh` run natively from Git Bash on `a86c439` (`CARGO_TARGET_DIR=target/windows`)
   passed all fifteen gates — fmt, clippy `-D warnings`, the whole workspace's tests, and every
   script gate. Nothing past M8a.1's fixes was needed: moving `split-debuginfo` out of the
   profile and forcing LF were what stood between the code and a Windows build.
8. **The same under bash 3.2 with busybox** — `bash:3.2` (GNU bash 3.2.57, BusyBox 1.37
   userland): the same twelve non-cargo lines of `./scripts/gates.sh`, all PASS. Red first: `check-poison.sh` exited 127 there,
   `env: can't execute 'bash'`, because this very checkout had CRLF endings.
9. **An autocrlf clone is LF** — `git clone -c core.autocrlf=true` of M8a.1's commit: 405 of
   405 files `w/lf`, `./scripts/build-index.py --check` passes, and `./scripts/check-poison.sh`
   runs under `bash:3.2`. The same clone of M7g.3's commit: 401 of 401 `w/crlf`, and
   `check-poison.sh` exits 127.
10. **`split-debuginfo` reaches rustc on Linux and nowhere else** — in the dev container,
    `cargo build -v -p pstore-types` passes `split-debuginfo=unpacked` twice with `RUSTFLAGS`
    unset (profile variable and Linux `rustflags`) and once with `RUSTFLAGS="-D warnings"`
    (profile variable alone) — the path `mutants.sh` takes. On this host,
    `rustc -Csplit-debuginfo=unpacked` on the MSVC toolchain answers "unstable on this
    platform", which is what the repo's profile used to make every build do.
11. **The full gate in the Linux dev container** — `./scripts/gates.sh`: all fifteen gates
    PASS on the final M8a.1 tree, and again on the final M8a.2 tree.
12. **The `portable` job is defined, and the parity check holds** — `./scripts/build-index.py --check`
    passes with `scripts/gates.sh` and `scripts/check-portable.sh` in the Gates table, and was
    red before the two rows were added. The job has no `continue-on-error`. NOT-RUN: its green
    run on `windows-latest` and `macos-latest` — CI runs on push, and pushing is the
    maintainer's call. Criterion 7's native Windows run is the same command on a Windows
    host, but a developer's machine, not the runner image; criterion 8 is a proxy for macOS.
    Neither substitutes for the job.
