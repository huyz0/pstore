# M103 — a mutation sweep removes what it created

**Serves:** the owner's report that the dev drive filled up. No backlog row: this is a harness
leak, found by looking for it.

## What is true today

These facts come from reading `scripts/mutants.sh` and from one session's disk. Nothing here was
measured on the owner's machine.

- **The tree copies.** cargo-mutants copies the whole tree, build directory included, into
  `$TMPDIR/cargo-mutants-*.tmp`, one copy per job. `mutants.sh` points `TMPDIR` at
  `~/.cache/pstore-mutants` unless `MUTANTS_TMPDIR` or `TMPDIR` is set. A sweep that finishes
  removes its copies. A sweep that is killed leaves them, and each one holds a build directory.
  In one session, copies left by killed sweeps, with the main build directory, filled the
  container's disk twice.
- **The diff file.** The incremental mode writes `git diff` to a `mktemp` file and sets
  `trap 'rm -f "$diff"' EXIT`. The script then ends in `exec cargo mutants`, and `exec` throws
  the trap away. So every incremental sweep leaked its diff file, killed or not: 27 of them
  were found in `/tmp`.
- **Why it matters on WSL2.** The dev environment's virtual disk grows and does not shrink
  when files are deleted. A leak there takes space from the Windows drive until someone
  compacts the disk by hand.

## Delta

1. **A directory per run.** `mutants.sh` makes `pstore-mutants-run.XXXXXX` under the base
   (`TMPDIR` if set, else `pick_tmpdir`). It points `TMPDIR` into it, and writes the diff file
   there too. The `EXIT` trap removes exactly that directory.
2. **Nothing is removed by a glob.** A sibling run directory, or a `cargo-mutants-*.tmp` straight
   under the base, may be a sweep still running, older or younger than this one. Each is reported
   on stderr with its size and kept.
3. **Not `exec`.** cargo-mutants runs in the background and the script waits on it. `INT` and
   `TERM` are passed on to it, and the script exits 130 or 143 through the trap. A trap does not
   run while bash waits on a foreground child, so without this a `kill` of the script would wait
   for the whole sweep. The script exits with cargo-mutants' own status.

**Not changed:** what is mutated, the job count and the timeouts. Not covered: the copies a
`kill -9` of the script itself leaves. The next sweep reports them.

## Acceptance criteria

1. `scripts/selftest-mutants.sh` runs `mutants.sh` in a scratch repository with a fake `cargo`.
   The fake makes a copy under `$TMPDIR` the way cargo-mutants does. Four cases:
   - **Killed:** the fake is killed with `SIGKILL`. The exit status is not 0, and the base holds
     only the copy that was there before. That copy is named on the output.
   - **Younger:** another sweep's copies appear beside this one while it runs, in both layouts.
     They are kept, and the script exits with the fake's status 2.
   - **Early exit:** with no diff, cargo-mutants never runs, the script exits 0 and the base is
     as it was.
   - **TERM:** a `TERM` to the script stops it within 5 s, with status 143. The fake is no longer
     running and the base is empty.

   Each case is observed red:
   - the killed case against the `mutants.sh` before this milestone;
   - the younger and early-exit cases against a first attempt that removed every
     `cargo-mutants-*.tmp` not seen at the start;
   - the TERM case against this script with cargo-mutants run in the foreground.
2. CI's `harness` job runs it. It is not in `gates.sh`, because `mutants.sh` is
   `portable: no`.
3. Gates: `scripts/check-portable.sh`, `scripts/build-index.py --check` and the rest of
   `gates.sh`.
