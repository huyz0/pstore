#!/usr/bin/env python3
"""Regenerate the index regions in AGENTS.md, .agents/skills/README.md and
docs/research/INDEX.md.

Rung 5 of the gate-design ladder: generate, don't maintain. These counts were
hand-edited on every commit and drifted within three commits -- which is the
canonical argument for this rung, made by this repository against itself.

  build-index.py           rewrite in place
  build-index.py --check   exit 1 if a region is stale
"""
import re, sys, pathlib, collections

ROOT = pathlib.Path(__file__).resolve().parent.parent
CHECK = "--check" in sys.argv


def region(text: str, key: str, body: str) -> str:
    s, e = f"<!-- index:{key}:start -->", f"<!-- index:{key}:end -->"
    return re.sub(f"{re.escape(s)}.*?{re.escape(e)}", f"{s}\n{body}\n{e}", text, flags=re.S)


def skills_table(rel_prefix: str) -> str:
    rows = []
    for sk in sorted((ROOT / ".agents/skills").glob("*/SKILL.md")):
        fm = sk.read_text().split("---")[1]
        name = re.search(r"^name:\s*(.+)$", fm, re.M).group(1).strip()
        desc = re.search(r"^description:\s*(.+)$", fm, re.M).group(1).strip()
        # The description's job is to say WHEN, so trim the leading "what it is".
        when = desc.split("Use when", 1)[-1].split("Use before", 1)[-1]
        when = re.sub(r"^\s*[.:]?\s*", "", when).rstrip(".")
        if "Use before" in desc:
            when = "Before " + when
        rows.append(f"| [`{name}`]({rel_prefix}{name}/SKILL.md) | {when[0].upper()}{when[1:]} |")
    return "| Skill | Use when |\n|---|---|\n" + "\n".join(rows)


def corpus_counts() -> dict:
    docs = sorted((ROOT / "docs/research").rglob("*.md"))
    qs, oqs = set(), set()
    for d in docs:
        t = d.read_text()
        qs |= {int(m) for m in re.findall(r"^\*\*Answers:\*\*\s*(?:D\d+,?\s*)?Q(\d+)", t, re.M)}
        oqs |= {int(m) for m in re.findall(r"OQ-(\d+)", t)}
    return {"docs": len(docs), "questions": len(qs), "open": len(oqs), "maxq": max(qs, default=0)}


def apply(path: pathlib.Path, new: str) -> bool:
    old = path.read_text()
    if old == new:
        return False
    if CHECK:
        print(f"STALE {path.relative_to(ROOT)}", file=sys.stderr)
        return True
    path.write_text(new)
    print(f"wrote {path.relative_to(ROOT)}")
    return True


stale = False
a = ROOT / "AGENTS.md"
stale |= apply(a, region(a.read_text(), "skills", skills_table(".agents/skills/")))
r = ROOT / ".agents/skills/README.md"
stale |= apply(r, region(r.read_text(), "skills", skills_table("")))

c = corpus_counts()
i = ROOT / "docs/research/INDEX.md"
txt = i.read_text()
txt = re.sub(r"\*\*Status: research phase complete\.\*\* \d+ documents, \d+ research questions answered, \d+ open\nquestions logged\.",
             f"**Status: research phase complete.** {c['docs']} documents, {c['questions']} research questions answered, {c['open']} open\nquestions logged.", txt)
txt = re.sub(r"Question IDs \(`Q1`…`Q\d+`\)", f"Question IDs (`Q1`…`Q{c['maxq']}`)", txt)
txt = re.sub(r"The plan: \d+ questions", f"The plan: {c['questions']} questions", txt)
txt = re.sub(r"\*\*\d+ open questions, risk-ranked\.\*\*", f"**{c['open']} open questions, risk-ranked.**", txt)
stale |= apply(i, txt)

# ⚠️ The Gates table in AGENTS.md is hand-written prose -- exactly the list that goes
# stale, and it already had: it named build-index.sh for months after the rename to .py.
# We cannot generate the descriptions, but we CAN assert that the set of scripts CI runs
# and the set the table claims are the same set. Rung 3 beating rung 7.
ci = (ROOT / ".github/workflows/ci.yml").read_text()
ci_scripts = {m for m in re.findall(r"\./(scripts/[\w.-]+)", ci) if "selftest-" not in m}
gates_tbl = re.search(r"<!-- index:gates:start -->(.*?)<!-- index:gates:end -->",
                      (ROOT / "AGENTS.md").read_text(), re.S).group(1)
tbl_scripts = set(re.findall(r"`(scripts/[\w.-]+)", gates_tbl))
for m in sorted(ci_scripts - tbl_scripts):
    stale = True
    print(f"STALE AGENTS.md Gates: CI runs {m}, the table does not list it", file=sys.stderr)
for e in sorted(tbl_scripts - ci_scripts):
    stale = True
    print(f"STALE AGENTS.md Gates: the table lists {e}, CI does not run it", file=sys.stderr)

# ⚠️ **And the third set.** This compared CI against the table and nothing against
# `gates.sh` -- so a gate added to `gates.sh` alone runs locally, never runs in CI, and both
# sets agree that everything is fine. Found while adding `check-poison.sh`, which is exactly
# the failure that gate exists to prevent, one level up. `gates.sh` is what a developer runs
# and `ci.yml` is what merges; they must be the same set.
gates_sh = (ROOT / "scripts/gates.sh").read_text()
sh_scripts = {m for m in re.findall(r"\./(scripts/[\w.-]+)", gates_sh) if "selftest-" not in m}
#
# ⚠️ **One direction only, and the asymmetry is the point.** Everything `gates.sh` runs must
# also run in CI -- a gate that only ever runs locally does not gate anything. The converse is
# false on purpose: `coverage.sh`, `recall.sh`, `ndcg.sh`, `depth.sh` and `mutants.sh` run in
# CI and are deliberately absent from the fast local script, which is why its header calling
# itself "every gate" is optimistic. Checking both ways reported those five as drift on the
# first run.
for m in sorted(sh_scripts - ci_scripts):
    stale = True
    print(f"STALE gates: gates.sh runs {m}, ci.yml does not -- it would never gate a merge",
          file=sys.stderr)

if CHECK and stale:
    print("FAIL run scripts/build-index.py to refresh", file=sys.stderr)
    sys.exit(1)
print(f"ok {c['docs']} docs, Q1-Q{c['maxq']} ({c['questions']} answered), {c['open']} open questions")
