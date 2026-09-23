# M8d — the mutation gate stops mutating entry points

**Serves:** **D-111** (mutation is what makes coverage mean something) and its exclusion
policy, which permits excluding *genuinely untestable paths* when the exclusion says why.

## Delta

`scripts/mutants.sh` mutates every file, including the two binary entry points,
`pstore-node/src/main.rs` and `pstore-server/src/main.rs`. **No test can run either**: there is
no `CARGO_BIN_EXE` or `assert_cmd` anywhere in `crates/`, so no test can catch a mutant there.
The nightly on `25089c7` missed 172 mutants, **39** of them in `pstore-node/src/main.rs` — a
lower bound, since shards 1 and 3 were cancelled.

**Changes:**
- The scope derivation moves out of `coverage.sh` into **`scripts/lib/scope.py`**, reading
  `cargo metadata`. It prints the excluded scope as names, as the regex `llvm-cov` takes, and
  as `cargo mutants --exclude` globs. `mutants.sh` captures its output into a variable and
  checks the status, never reads it through `< <(...)`, where a failure does not propagate.
- `mutants.sh` passes those globs in every mode (`--in-diff`, `--file`, `--check`, `--shard`,
  `--all`), prints what it excluded to stderr, and **fails if `scope.py` fails** — a silent
  fallback would reinstate the excluded mutants and turn the nightly red with no explanation.
- ⚠️ **The two gates exclude different things, deliberately.** Coverage excludes entry points
  *and* `ships = false` crates. Mutation excludes **entry points only**. `pstore-testkit` does
  not ship, but it *is* testable — its missed mutants were 36 of 303 — and it holds the
  conformance probes eleven crates test against, for which mutation is the only check that
  they can tell right from wrong. Its 36 misses are handled like M8c's, not excluded. So
  `scope.py mutants` prints the entry points and nothing else.
- Globs are `src_path` **relative to `workspace_root`, with forward slashes**
  (`crates/pstore-node/src/main.rs`). cargo-mutants 27.1.0 matches a glob containing `/`
  against the path from the workspace root exactly, so a crate-relative form excludes nothing
  and a bare `main.rs` would match every file of that name.
- `--exclude` only stops files being **mutated**. Which packages' tests run is unchanged.

**Measured before any change** (`cargo mutants --list`): 3,400 mutants, **61** in the two
entry points (57 and 4). After: **3,339**.

⚠️ **The cost, disclosed.** `pstore-node/src/main.rs` is not pure wiring today: it decides when
to heal (`ticks % heal_every == jitter(...)`), whether to republish (`size != last_size`), and
the ownership cadence. Those decisions become invisible to every gate — the same trade
`coverage.sh` states, where moving logic into the library is the only way to get it tested.
And D-110's ≥70% mutation target for `pstore-server` (wiring) is not measured, before or after.

**Does not change:** any threshold, `coverage.sh`'s regex, which crates declare
`ships = false`, or which packages' tests run. The 89 misses in shipping library code and
testkit's 36 are the following milestones.

## Acceptance criteria

1. Through the new excludes, `cargo mutants --list` lists **3,339**, and the 61 it no longer
   lists are exactly the lines under `crates/pstore-node/src/main.rs` and
   `crates/pstore-server/src/main.rs`.
2. The exclusion is derived: in a scratch copy, adding `src/bin/extra.rs` — with a function
   cargo-mutants mutates, not an empty `main` — to a crate adds that
   file to `scope.py mutants` and removes its mutants from `--list`, with no other edit; and
   marking a crate `ships = false` adds it to `scope.py llvm-cov` but **not** to
   `scope.py mutants`.
3. `coverage.sh`'s `--ignore-filename-regex` is **byte-identical** before and after the move.
4. `mutants.sh` exits non-zero when `scope.py` fails.
5. `./scripts/gates.sh` passes in the Linux dev container.

## Test plan

| # | Fails first | Mutation it catches |
|---|---|---|
| 1 | the list before the change: 3,400, 61 under the entry points | a glob matching more than the entry points, or nothing |
| 2 | the scratch copy before the edit: the file is mutated | a hard-coded list; testkit leaking into the mutation scope |
| 3 | — (a move; the strings are compared) | a regex that changed in the move |
| 4 | `scope.py` made to exit 1 | a `|| true` that restores the excluded mutants silently |

## RA budget

Unchanged: gate configuration only.

## Risks

- The regex is rebuilt with `pathlib` rather than by splitting on `/`. Criterion 3 checks it
  on Linux, where CI runs coverage; nothing here claims anything about Windows paths.

## Tasks

- **M8d.1** — `scope.py`, both scripts calling it, the Gates-table row and `mutants.sh`'s
  header count (3,339 after), this ledger.
