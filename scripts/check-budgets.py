#!/usr/bin/env python3
"""Every request budget is spent by a test, and every budget a test spends exists (M278).

Rung 3 of the gate-design ladder: a predicate over files. The table is
docs/research/11-design/request-budgets.md; a test spends a row with
`spend("<name>", ...)` or `spend_per("<name>", ...)` (pstore_testkit::budget). A row no
test spends is a number nothing holds, which is how a budget table goes stale; a name with
no row fails at run time already, and is refused here before anything compiles.

What it cannot see: whether the test that spends a row measures what the row describes.

  check-budgets.py              check the tree
  check-budgets.py --self-test  prove it refuses an unspent row and an unknown name
"""
import pathlib, re, sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
TABLE = ROOT / "docs/research/11-design/request-budgets.md"
ROW = re.compile(r"^\|\s*`([a-z0-9_]+)`\s*\|\s*(exact|max)\s*\|\s*([0-9]+)\s*\|")
SPENT = re.compile(r"\bspend(?:_per)?\(\s*\"([a-z0-9_]+)\"")


def rows(text: str) -> set[str]:
    return {m.group(1) for line in text.splitlines() if (m := ROW.match(line.strip()))}


def spent(sources: list[str]) -> set[str]:
    return {m.group(1) for src in sources for m in SPENT.finditer(src)}


def problems(table: set[str], used: set[str]) -> list[str]:
    errs = [f"budget `{n}` is in the table and no test spends it" for n in sorted(table - used)]
    errs += [f"a test spends `{n}`, which the table does not have" for n in sorted(used - table)]
    return errs


def self_test() -> int:
    table = rows("| `a` | exact | 1 | x |\n| `b` | max | 2 | y |\n| Budget | Bound | Requests | What |")
    if table != {"a", "b"}:
        print(f"self-test FAILED: parsed {table}")
        return 1
    if problems(table, spent(['spend("a", 1); spend_per("b", 2, 3);'])):
        print("self-test FAILED: refused a tree where every row is spent")
        return 1
    if not problems(table, spent(['spend("a", 1);'])):
        print("self-test FAILED: accepted an unspent row")
        return 1
    if not problems(table, spent(['spend("a", 1); spend("b", 1); spend("c", 1);'])):
        print("self-test FAILED: accepted an unknown name")
        return 1
    print("self-test ok")
    return 0


def main() -> int:
    if sys.argv[1:] == ["--self-test"]:
        return self_test()
    table = rows(TABLE.read_text(encoding="utf-8"))
    if not table:
        print(f"FAIL {TABLE} has no budget rows")
        return 1
    sources = [p.read_text(encoding="utf-8") for p in ROOT.glob("crates/**/*.rs")]
    errs = problems(table, spent(sources))
    if errs:
        print("FAIL request budgets:")
        for e in errs:
            print(f"  {e}")
        return 1
    print(f"ok {len(table)} request budgets, each spent by a test")
    return 0


if __name__ == "__main__":
    sys.exit(main())
