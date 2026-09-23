"""What the gates measure: every workspace crate, minus the two things `cargo test` cannot
reach or does not ship. One derivation, called by `coverage.sh` and `mutants.sh`, so the two
gates cannot disagree about what the codebase is -- they did, twice (M8b, M8d).

  py scripts/lib/scope.py names     human-readable, one line
  py scripts/lib/scope.py llvm-cov  the --ignore-filename-regex llvm-cov takes
  py scripts/lib/scope.py mutants   one `cargo mutants --exclude` glob per line -- entry
                                    points only; see the note in main()

Reads `cargo metadata --no-deps --format-version 1` on stdin.

Excluded from COVERAGE, and why (mutation excludes only the second):

* **A crate that declares `[package.metadata.pstore] ships = false`.** Declared, not inferred
  from the dependency graph: the inferred version excluded `pstore-engine`, the correctness
  core, because only a dev-dependency named it (M7a). A crate is measured unless it says
  otherwise, and saying otherwise is a line in a diff.
* **Binary entry points.** Composition roots: they read the environment, open connections and
  loop, and nothing outside the operating system can call one. This excludes WIRING, never
  logic -- logic left in a `main.rs` is invisible to every gate, so moving it into the library
  is the only way to get it tested.

Both are derived from metadata, so a new binary or a new `ships = false` crate is covered the
day it is added.
"""
import json
import pathlib
import sys


def scope(md):
    root = pathlib.Path(md["workspace_root"])
    # Sorted BY NAME: sorting the package dicts themselves works with one `ships = false` crate
    # and raises with two -- found by M8d's criterion 2, which adds a second.
    test_only = sorted(
        (p for p in md["packages"]
         if (p.get("metadata") or {}).get("pstore", {}).get("ships") is False),
        key=lambda p: p["name"],
    )
    bins = sorted(
        pathlib.Path(t["src_path"])
        for p in md["packages"]
        for t in p["targets"]
        if t["kind"] == ["bin"]
    )
    return root, test_only, bins


def rel(root, path):
    return path.relative_to(root).as_posix()


def main():
    mode = sys.argv[1] if len(sys.argv) > 1 else "names"
    md = json.load(sys.stdin)
    root, test_only, bins = scope(md)
    if mode == "names":
        print(" ".join([p["name"] for p in test_only] + [b.name for b in bins]))
    elif mode == "llvm-cov":
        # Crate names match either spelling (`pstore-testkit` / `pstore_testkit`), and entry
        # points by their last three path components, so either separator matches.
        pats = [p["name"].replace("-", "[-_]") for p in test_only]
        pats += ["/".join(b.parts[-3:]) for b in bins]
        print("|".join(pats))
    elif mode == "mutants":
        # ⚠️ ENTRY POINTS ONLY -- not the `ships = false` crates. Coverage leaves those out
        # because they do not ship; mutation keeps them because they ARE testable, and
        # `pstore-testkit` holds the conformance probes eleven crates test against, for which
        # mutation is the only check that they can tell right from wrong (M8d).
        # Relative to the workspace root with forward slashes: cargo-mutants matches a glob
        # containing `/` against exactly that path, so any other form excludes nothing.
        for b in bins:
            print(rel(root, b))
    else:
        print(f"unknown mode {mode!r}: names | llvm-cov | mutants", file=sys.stderr)
        return 2
    return 0


if __name__ == "__main__":
    sys.exit(main())
