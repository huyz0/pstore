#!/usr/bin/env python3
"""Does a change need the Rust jobs? Prints `code=true` or `code=false` for CI.

Rung 2 of the gate-design ladder: what counts as documentation is derived from the tree,
not listed by hand. A path is inert when it is under `docs/`, `.agents/` or `.claude/`, or
is a Markdown file or `LICENSE` at the root -- **unless a Rust source names it**. Tests read
documentation (`include_str!("../../../../docs/deploy.md")`, or `CARGO_MANIFEST_DIR` plus
`/../../docs/research`), and a doc edit that breaks such a test must still run the tests.
Every `../docs/...` string in `crates/**/*.rs` is therefore a prefix that counts as code.

Fails safe: an unreadable diff, an empty diff or any error answers `code=true`. Only the
Rust jobs are skipped; the `harness` job (links, index, ledgers, SLOs) always runs.

What it cannot see: a Rust source that reaches a document through a path it assembles
from pieces, with no `../docs/` literal in it. `dev/`, `scripts/` and everything else are
code.

  ci-changes.py BASE HEAD    classify `git diff --name-only BASE...HEAD`
  ci-changes.py --self-test  prove it keeps the cases that must run
"""
import pathlib, re, subprocess, sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
INERT_DIRS = ("docs/", ".agents/", ".claude/")
DOC_REF = re.compile(r"(?:\.\./)+(docs/[^\"\s{}]*)")


def read_by_code(root: pathlib.Path) -> set:
    """Every `docs/...` path or prefix a Rust source names through `../`."""
    refs = set()
    for rs in (root / "crates").rglob("*.rs"):
        for m in DOC_REF.finditer(rs.read_text(encoding="utf-8", errors="replace")):
            refs.add(m.group(1).rstrip("/"))
    return refs


def inert(path: str, refs: set) -> bool:
    if any(path == r or path.startswith(r + "/") for r in refs):
        return False
    if path.startswith(INERT_DIRS):
        return True
    return "/" not in path and (path.endswith(".md") or path == "LICENSE")


def needs_code(paths: list, refs: set) -> bool:
    return not paths or not all(inert(p, refs) for p in paths)


def self_test() -> int:
    refs = {"docs/deploy.md", "docs/research"}
    cases = [
        (["docs/comparisons/vespa.md"], False),
        (["README.md", "AGENTS.md", "LICENSE", ".agents/skills/x/SKILL.md"], False),
        (["docs/milestones/M1/SPEC.md", "docs/comparisons/DELIVERY.md"], False),
        (["docs/deploy.md"], True),
        (["docs/research/INDEX.md"], True),
        (["docs/a.md", "crates/pstore-types/src/lib.rs"], True),
        (["crates/pstore-server/README.md"], True),
        (["scripts/gates.sh"], True),
        ([".github/workflows/ci.yml"], True),
        (["Cargo.lock"], True),
        ([], True),
    ]
    bad = [(p, want) for p, want in cases if needs_code(p, refs) != want]
    found = read_by_code(ROOT)
    if not {"docs/deploy.md", "docs/research"} <= found:
        bad.append((sorted(found), "the tree's own references to docs/deploy.md and docs/research"))
    for p, want in bad:
        print(f"FAIL {p}: expected {want}")
    print("ok ci-changes self-test" if not bad else f"{len(bad)} failing case(s)")
    return 1 if bad else 0


def main(argv: list) -> int:
    if argv[1:] == ["--self-test"]:
        return self_test()
    try:
        base, head = argv[1:]
        out = subprocess.run(
            ["git", "diff", "--name-only", f"{base}...{head}"],
            cwd=ROOT, check=True, capture_output=True, text=True,
        ).stdout.split()
        code = needs_code(out, read_by_code(ROOT))
    except Exception as e:  # noqa: BLE001 -- any doubt runs everything
        print(f"ci-changes: {e!r}; running everything", file=sys.stderr)
        code = True
    print(f"code={'true' if code else 'false'}")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
