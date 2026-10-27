#!/usr/bin/env bash
# gate-design step 2: make it fail first. `coverage.sh --changed` measures only the crates a
# change touches, so the one thing it must not get wrong is WHICH crates: a crate left out is
# coverage nobody measured, reported as if it had been.
set -euo pipefail
cd "$(dirname "$0")/.."
. scripts/lib/py.sh
# A three-crate workspace, one of them test-only, with a binary entry point.
MD='{"workspace_root":"/w","packages":[
 {"name":"pstore-a","manifest_path":"/w/crates/pstore-a/Cargo.toml","metadata":null,
  "targets":[{"kind":["lib"],"src_path":"/w/crates/pstore-a/src/lib.rs"}]},
 {"name":"pstore-b","manifest_path":"/w/crates/pstore-b/Cargo.toml","metadata":null,
  "targets":[{"kind":["lib"],"src_path":"/w/crates/pstore-b/src/lib.rs"},
             {"kind":["bin"],"src_path":"/w/crates/pstore-b/src/main.rs"}]},
 {"name":"pstore-kit","manifest_path":"/w/crates/pstore-kit/Cargo.toml",
  "metadata":{"pstore":{"ships":false}},
  "targets":[{"kind":["lib"],"src_path":"/w/crates/pstore-kit/src/lib.rs"}]}]}'
changed() { printf '%s' "$MD" | py scripts/lib/scope.py changed "$@"; }

out=$(changed crates/pstore-b/src/x.rs crates/pstore-b/tests/it/y.rs docs/z.md)
[ "$(printf '%s\n' "$out" | sed -n 1p)" = "pstore-b" ] \
  || { echo "FAIL a change to pstore-b did not select exactly pstore-b: $out"; exit 1; }
# The report leaves out every crate it did not run, and still leaves out what the full run does.
ignore=$(printf '%s\n' "$out" | sed -n 2p)
for must in 'pstore-a' 'pstore[-_]kit' 'pstore-b/src/main.rs'; do
  case "$ignore" in *"$must"*) ;; *) echo "FAIL the report would include $must: $ignore"; exit 1 ;; esac
done
case "$ignore" in *'crates/pstore-b/'*) echo "FAIL the report would leave out pstore-b: $ignore"; exit 1 ;; esac

out=$(changed crates/pstore-a/src/x.rs crates/pstore-b/Cargo.toml)
[ "$(printf '%s\n' "$out" | sed -n 1p)" = "pstore-a pstore-b" ] \
  || { echo "FAIL two changed crates were not both selected: $out"; exit 1; }

# A test-only crate is not measured, so changing it alone selects nothing to measure.
out=$(changed crates/pstore-kit/src/x.rs docs/z.md)
[ -z "$(printf '%s\n' "$out" | sed -n 1p)" ] \
  || { echo "FAIL a test-only crate was selected for coverage: $out"; exit 1; }
echo "ok --changed selects the touched shipping crates and reports nothing else"
