#!/usr/bin/env bash
# Every relative markdown link resolves. Rung 3 (gate-design): a predicate over
# files in the tree, so an agent must never be asked to check it.
set -euo pipefail
cd "$(dirname "$0")/.."
python3 - "$@" <<'PY'
import re, sys, pathlib
roots = [pathlib.Path("docs"), pathlib.Path("dev"), pathlib.Path(".agents"),
         pathlib.Path(".claude"), pathlib.Path("README.md"), pathlib.Path("AGENTS.md")]
files = []
for r in roots:
    files += [r] if r.is_file() else list(r.rglob("*.md")) if r.exists() else []
bad = []
for f in files:
    for m in re.finditer(r"\]\((?!https?:|mailto:)([^)#]+)", f.read_text()):
        target = (f.parent / m.group(1).strip()).resolve()
        if not target.exists():
            bad.append(f"{f}: {m.group(1).strip()}")
if bad:
    print("FAIL broken relative links:", file=sys.stderr)
    print("\n".join(f"  {b}" for b in bad), file=sys.stderr)
    sys.exit(1)
print(f"ok {len(files)} markdown files, all relative links resolve")
PY
