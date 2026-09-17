#!/usr/bin/env python3
"""Every service objective is `enforced` with a gate that exists, or `blocked` with a blocker.

⚠️ This is rung 3 of the gate-design ladder: the rule is a **predicate over files in the
tree**, so no agent is asked to check it. The failure it exists to prevent is the one an
SLO document always has -- a row that says `enforced` and names nothing, or names a script
that was deleted two milestones ago, which is a promise nobody keeps and nobody notices.

⚠️ What it CANNOT check: whether the gate it names actually asserts the objective. Naming
`cargo test` would satisfy it. Stated, because a documented blind spot beats a mechanism
that half works.

  check-slos.py [FILE]
"""
import pathlib, re, sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
DOC = pathlib.Path(sys.argv[1]) if len(sys.argv) > 1 else ROOT / "docs/research/11-design/slos.md"
STATUSES = {"enforced", "blocked"}


def known_tests() -> set[str]:
    names = set()
    attr = re.compile(r"#\[(?:tokio::|async_std::)?[a-z_:]*test")
    for rs in ROOT.rglob("crates/**/*.rs"):
        lines = rs.read_text().splitlines()
        for i, line in enumerate(lines):
            if not attr.search(line):
                continue
            for nxt in lines[i + 1 : i + 6]:
                m = re.search(r"\bfn\s+([A-Za-z_][A-Za-z0-9_]*)", nxt)
                if m:
                    names.add(m.group(1))
                    break
    return names


def main() -> int:
    if not DOC.exists():
        print(f"FAIL {DOC} does not exist")
        return 1
    tests, errs, rows = known_tests(), [], 0
    for n, line in enumerate(DOC.read_text().splitlines(), 1):
        if not line.startswith("|") or line.startswith("|---"):
            continue
        cells = [c.strip() for c in line.strip("|").split("|")]
        if len(cells) < 3 or cells[-2] not in STATUSES and cells[-2].lower() == "status":
            continue  # the header row
        status = next((c for c in cells if c in STATUSES), None)
        if status is None:
            if any(c in ("Status",) for c in cells):
                continue
            errs.append(f"{DOC}:{n}: no status of {sorted(STATUSES)} in: {line.strip()}")
            continue
        rows += 1
        last = cells[-1]
        named = re.findall(r"`([^`]+)`", last)
        if status == "enforced":
            # A gate is a script that exists, or a test function that resolves.
            ok = any(
                (ROOT / ref).exists() if "/" in ref else ref.split("::")[-1] in tests
                for ref in named
            )
            if not ok:
                errs.append(
                    f"{DOC}:{n}: enforced, but names no gate that exists: {last!r}"
                )
        elif not last or last.lower() in ("", "-", "none"):
            errs.append(f"{DOC}:{n}: blocked, but names no blocker")
    if errs:
        print("FAIL service objectives:")
        for e in errs:
            print(f"  {e}")
        return 1
    print(f"ok {rows} objectives, every enforced one names a gate that exists")
    return 0


if __name__ == "__main__":
    sys.exit(main())
