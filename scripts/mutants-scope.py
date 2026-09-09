#!/usr/bin/env python3
"""The packages whose tests can catch a mutant in the given files.

⚠️ `test_workspace = true` exists for a real reason: `pstore-blob` defines the store
decorators and the conformance suite that interrogates them lives in `pstore-testkit`, so
cargo's default (the mutated package only) reported a mutant MISSED while a test that
fails on it sat two directories away.

But "every package" is not the fix for that -- "every package that could possibly see it"
is. Nothing in `pstore-gossip` can catch a mutant in `pstore-format`, and running its
tests costs 7.5 s on every one of them. This computes the reverse-dependency closure,
which is strictly safer than cargo's default and strictly cheaper than the workspace.

Rung 2 of the gate-design ladder: derived from `cargo metadata`, so a new crate is
scoped correctly the day it is added.

    mutants-scope.py crates/pstore-format/src/sparse.rs [...]   -> package names, one per line
"""
import json, pathlib, subprocess, sys

md = json.loads(subprocess.run(
    ["cargo", "metadata", "--no-deps", "--format-version", "1"],
    capture_output=True, text=True, check=True).stdout)
names = {p["name"] for p in md["packages"]}
root = pathlib.Path(md["workspace_root"])

# package -> the workspace packages it depends on, dev-dependencies INCLUDED. A dev
# dependency is exactly how a test reaches into another crate, which is the case that
# made `test_workspace` necessary in the first place.
deps = {p["name"]: {d["name"] for d in p["dependencies"] if d["name"] in names}
        for p in md["packages"]}
# package -> its directory, for mapping a changed file back to a crate.
where = {p["name"]: pathlib.Path(p["manifest_path"]).parent for p in md["packages"]}

seeds = set()
for arg in sys.argv[1:]:
    f = (root / arg).resolve()
    best = max((n for n, d in where.items() if d in f.parents or d == f.parent),
               key=lambda n: len(str(where[n])), default=None)
    if best:
        seeds.add(best)
if not seeds:
    sys.exit(0)

# Everything that depends on a seed, transitively.
scope, frontier = set(seeds), set(seeds)
while frontier:
    nxt = {p for p in names if deps[p] & frontier} - scope
    scope |= nxt
    frontier = nxt
print("\n".join(sorted(scope)))
