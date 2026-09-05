# The Agent Harness: What Was Distilled, and What Was Left

**Answers:** Q47
**Status:** Complete (v1)
**Source:** `opensearch-bin-ingester` (sibling repo), read 2026-09-05

## 1. Why distil rather than copy

That project runs a mature harness: 13 skills, 16 pre-commit gates, two reviewer
subagents per commit, a generated gate index. It also **hit both of the pitfalls to
avoid, left the post-mortems in the code**, and fixed them. That evidence is worth more
than the skill count.

pstore is at a different point — research complete, one crate of code, no backlog — so
copying 13 skills would ship capability that cannot run. **A skill that cannot run is
worse than no skill, because it reads as capability.** Four skills were taken; nine were
deliberately left (§6).

## 2. Pitfall one: the review loop that does not terminate

Measured in the source repo's `.harness/review/`:

| Task | Rounds | Total packet | ~Tokens |
|---|---|---|---|
| M-1.1 | **6** | 62,954 lines | **~755,000** |
| M-1.2 | 4 | 17,403 lines | ~208,000 |
| M0.4 | 5 | 10,811 lines | ~129,000 |
| M0.5 | 7 | 1,558 lines | ~18,000 |
| M0.6 | 3 | 460 lines | **~5,000** |

Their own comment explains why "loop until clean" has no fixed point:

> It always finds something. Reviewers verify by mutation and the marginal round is
> never empty. **What decays is severity** — on the ten-round task, rounds 7–10 found
> only false sentences in the prose describing the review, each correction voiding the
> verdicts and buying the next round.

Two root causes, both instructive:

1. **A broken counter.** The round number was resolved from a diff-sha that only existed
   *after* the packet was made, so it printed "round 1 of 2" on **every** round for the
   life of the gate. The cap never fired once. One task reached **eleven** rounds.
2. **The cap refused the wrong thing.** It refused the *commit*; nothing refused the next
   *round*. So an author could spend pairs of agents indefinitely, and each round felt
   justified because it found something.

### What we took

| Mechanism | In `scripts/review.sh` |
|---|---|
| **The budget refuses the round**, not just the outcome | Hard exit at round 5, `REVIEW_ROUND_BUDGET` override that must be typed |
| **The remedy is SPLIT, not another round** | Printed in the refusal — a change that cannot survive two rounds is too big |
| **Only `blocking`/`major` stop a commit; minors land** | In the packet header |
| **"An empty findings list is a valid and expected outcome"** | In the packet header — without this the reviewer manufactures work |
| Fixing a minor is *usually wrong* | Stated: the fix's new prose is the next round's surface |
| **The counter is itself tested** | `scripts/selftest-review.sh` |

That last row earned itself immediately: our first counter had `set -e` + `pipefail`
abort when `ls` matched nothing, so round 1 crashed rather than counting 0. **The
selftest caught it on the first run** — the same class of silent-counter failure, found
in minutes instead of eleven rounds.

## 3. Pitfall two: token cost

The M-1.1 packets were **9,949 / 9,976 / 9,977 / 9,977 / 9,978 / 13,097** lines. Near-
identical, because every round re-sent the whole diff. By M0.6 a round was 109 lines —
a **~78× reduction per round**, ~150× per task.

### What we took

1. **Verify rounds get the delta**, not the whole diff again. Round one has the full
   diff; every later round has only what changed since the last reviewed round.
2. **"Gates that already passed — do not re-check these."** The packet runs the
   deterministic gates and lists them, so the reviewer spends its budget on judgement
   rather than re-deriving what a script already knows.
3. **Progressive disclosure**, four layers. This is the big one, and the reason it
   matters here is the corpus: 53 documents cannot be loaded, and `INDEX.md` giving each
   a one-line finding is usually the whole answer.

Measured cost of our layer 0 + 1: **~4,800 tokens per session** (`AGENTS.md` plus four
skill descriptions). That is the standing charge; everything else is on demand.

> ⚠️ **Layer 1 is the whole mechanism.** A skill's `description` is all an agent sees
> before deciding to load it, so it must say **when to use this**, not what it is.

## 4. The single most valuable artifact: the gate-design ladder

Taken nearly intact, because it is the meta-rule that keeps a harness from becoming a
pile of instructions:

| # | Mechanism |
|---|---|
| 1 | Make the bad state unrepresentable |
| 2 | Derive from a source of truth |
| 3 | Deterministic gate |
| 4 | Commit the fact, diff the change |
| 5 | Generate, don't maintain |
| 6 | Agent review |
| 7 | An instruction in a prompt |

> **The test: can you state the rule as a predicate over files in the tree?** If yes it
> is rung 3 or better, and an agent must not be asked to do it.

And the failure mode it exists to prevent: *a check that cannot fail deterministically
tends to report success while checking nothing.* Their examples included a licence gate
grepping SPDX ids against a report emitting licence *prose* (so `GPL-2.0` could never
match — passing everything) and two gates that printed `FAIL` and exited `0`.

**This repository produced two instances within a day of adopting it**, both recorded in
the skill:
- `rust_2018_idioms` set at the same lint priority as individual lints, so `cargo clippy`
  refused to compile the workspace. Caught by *running* it.
- The document and question counts in `INDEX.md`, hand-edited every commit. When
  `build-index.py` was written (rung 5), it found the count had already drifted from 43
  to 53. **The argument for rung 5, made by this repository against itself.**

## 5. What we took, in full

| Taken | Form here |
|---|---|
| `AGENTS.md` as a deliberate index; `CLAUDE.md` a two-line adapter | 117 lines, layer 0 |
| Progressive disclosure, four layers | `.agents/skills/README.md` |
| Vendor-neutral skills; `.claude/` holds only pointers | `.claude/commands/*.md` |
| The gate-design ladder | `.agents/skills/gate-design/SKILL.md` |
| Bounded review with a tested counter | `.agents/skills/review/SKILL.md`, `scripts/review.sh` |
| Reviewer denied the author's reasoning | packet header |
| Generate, don't maintain | `scripts/build-index.py` |
| **"Never claim a gate ran without running it"** as non-negotiable 1 | `AGENTS.md` |
| An honest *Gates* section saying what actually runs today | `AGENTS.md` |
| Naming things a reader can hold, not bare IDs | `AGENTS.md` § Never |

## 6. What we deliberately left, and when to add it

| Left | Add when |
|---|---|
| `milestone`, `next-task` | A backlog exists. A loop over an empty backlog is not autonomy. |
| `spec` | Milestones acquire acceptance criteria — currently the roadmap's exit conditions carry this. |
| `bench`, `cost-budget` | There is a hot path and a request counter to measure. The invariant tests (engineering-standards §7) carry it meanwhile. |
| `adr` | Decisions here are `D-<n>`, numbered inline in the research docs and already searchable. A parallel ADR tree would be a second home for the same fact. |
| `wire-format-change` | `pstore-format` exists. |
| `milestone-review`, `skill-forge` | There is enough process to review or forge. |
| Two-reviewer split (production vs tests) | There is production code and tests to separate. The *reason* — test weakness is invisible to coverage — is already carried by the mutation gate. |
| Pre-commit hook wiring | More than two scripts exist to wire. |

⚠️ Each of these is a **capability whose absence is stated**, not an oversight. The
harness's own honesty rule applies to itself.

## 7. Where we diverged

1. **No `.harness/` in the tree.** Their review verdicts live in a gitignored directory,
   which they note honestly means the gate is local-only and a `--no-verify` commit
   carries no verdict. We inherit the same limitation and gitignore `.harness/` too —
   but with one reviewer invoked by hand rather than a hook, the limitation is at least
   visible rather than implied.
2. **`AGENTS.md` is 117 lines, not 278.** Theirs carries a long *Gates* section
   documenting blind spots in gates we do not have. When ours has that many gates it
   will grow — but layer 0 is paid every session, so growth needs a reason.
3. **Mutation testing is a gate here, not an aspiration.** Theirs names
   `check-mutants.sh` as not-yet-existing; ours is wired in CI from the start, because
   `cargo mutants` needs no build system work that `cargo` does not already do.

## 8. Open questions raised

- OQ-161 — Is a ~4,800-token layer 0+1 the right standing charge? It is affordable, but
  `AGENTS.md` restates the five load-bearing conclusions that `INDEX.md` also carries.
  Duplication in layer 0 is the expensive kind.
- OQ-162 — The review packet currently runs the gates itself. If a gate becomes slow,
  packet construction becomes slow. Cache the verdicts by tree-sha?
- OQ-163 — When a backlog exists, does `milestone` autonomy need a *token* budget as
  well as a round budget? Rounds bound one task; nothing bounds a long run.
- OQ-164 — `build-index.py` counts every `.md` under `docs/research/`, so `INDEX.md` and
  the plan count as documents. Honest, but is "53 documents" the number a reader wants,
  or should meta-files be excluded?

## Sources

- `opensearch-bin-ingester` — `AGENTS.md`, `.agents/skills/`, `scripts/review.sh`,
  `.harness/review/` (round counts and packet sizes measured directly)
- [Agent Skills specification](https://agent-skills.org)
