#!/usr/bin/env bash
# Compare public envelope parsing with a local Git revision. Provisional CPU and
# allocation measurements, outside ordinary tests and with no performance threshold.
set -euo pipefail
cd "$(dirname "$0")/.."
. scripts/lib/py.sh
py - "${1:-d1bc45b}" <<'PY'
import os
import pathlib
import shutil
import subprocess
import sys

root = pathlib.Path.cwd()
revision = subprocess.check_output(
    ["git", "rev-parse", "--verify", "--end-of-options", sys.argv[1] + "^{commit}"],
    text=True,
).strip()
project = root / ".harness/m126-measure"
baseline = project / "baseline"
baseline.mkdir(parents=True, exist_ok=True)
(project / "src").mkdir(exist_ok=True)
for name in ("types.rs", "body.rs"):
    contents = subprocess.check_output(
        ["git", "show", revision + ":crates/pstore-server/src/" + name]
    )
    (baseline / name).write_bytes(contents)
template = root / "docs/milestones/M126/measure"
shutil.copyfile(template / "Cargo.toml", project / "Cargo.toml")
shutil.copyfile(template / "main.rs", project / "src/main.rs")
# Resolve overlapping dependencies at the production versions, not a fresh upgrade.
shutil.copyfile(root / "Cargo.lock", project / "Cargo.lock")
env = os.environ.copy()
env["CARGO_BUILD_JOBS"] = str(min(4, int(env.get("CARGO_BUILD_JOBS", "4"))))
env["CARGO_TARGET_DIR"] = str(root / "target/m126-measure")
print("provisional parser measurement; baseline=" + revision, file=sys.stderr, flush=True)
subprocess.run(["cargo", "run", "--release", "--quiet"], cwd=project, env=env, check=True)
PY
