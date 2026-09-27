#!/usr/bin/env python3
"""Every milestone directory is named `M<n>`: a plain number, never a letter suffix.

Rung 3 of the gate-design ladder: the rule is a predicate over directory names, so no agent
is asked to remember it.

⚠️ Why this exists. From M0a to M9j a roadmap theme was decomposed into lettered
sub-milestones (M5a..M5i, M8a..M8m, M9a..M9j). Nothing chose that deliberately: the `spec`
skill fixed the TASK id format (`M<n>.<k>`) and said nothing about the milestone's, so the
first lettered name was copied by every milestone after it. From M10 on a milestone is the
next integer, and its tasks are `M<n>.<k>`.

The lettered names that already exist stay: renaming them would break every link, commit
subject and ledger that cites them. They are recognised by rule rather than listed -- a
letter after 0..8, or a letter a..j after 9 -- so there is no list to maintain, and no new
lettered name can pass.

What it cannot see: a milestone named plainly but spoken of with a letter in prose.

  check-milestone-ids.py              check docs/milestones/
  check-milestone-ids.py --self-test  prove it refuses a lettered new name
"""
import re, sys, pathlib

ROOT = pathlib.Path(__file__).resolve().parent.parent
PLAIN = re.compile(r"M(0|[1-9][0-9]*)")
LEGACY = re.compile(r"M([0-9])([a-z])")


def allowed(name: str) -> bool:
    if PLAIN.fullmatch(name):
        return True
    m = LEGACY.fullmatch(name)
    if not m:
        return False
    n, letter = int(m.group(1)), m.group(2)
    return n < 9 or letter <= "j"


def refused(names: list[str]) -> list[str]:
    return [n for n in names if not allowed(n)]


def self_test() -> int:
    must_refuse = ["M10a", "M9k", "M12b", "M10.1", "m10", "M010", "Mx"]
    must_accept = ["M0", "M10", "M11", "M123", "M0a", "M5i", "M8m", "M9a", "M9j"]
    bad = [n for n in must_refuse if allowed(n)] + [n for n in must_accept if not allowed(n)]
    if bad:
        print(f"self-test FAILED: misjudged {bad}")
        return 1
    print("self-test ok")
    return 0


def main() -> int:
    if sys.argv[1:] == ["--self-test"]:
        return self_test()
    base = ROOT / "docs" / "milestones"
    names = sorted(p.name for p in base.iterdir() if p.is_dir())
    bad = refused(names)
    if bad:
        print("FAIL milestone directories not named M<n> (a plain number, no letter):")
        for n in bad:
            print(f"  docs/milestones/{n}")
        return 1
    print(f"ok: {len(names)} milestone directories")
    return 0


sys.exit(main())
