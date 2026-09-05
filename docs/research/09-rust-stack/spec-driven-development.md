# Spec-Driven Development: What to Adopt, and What Modern Models Change

**Answers:** Q48
**Status:** Complete (v1)
**Retrieved:** 2026-09-05

## 1. The question

`opensearch-bin-ingester` has an SDD standard and a `spec` skill. Is it worth copying,
or is something on the internet better — more token-efficient, higher quality with an
Opus- or Fable-class model?

Short answer: **copy theirs, take one idea from OpenSpec, and skip Spec Kit and BMAD
entirely** — because pstore already has more spec than any of them would generate. But
the more interesting answer is that **a stronger model shifts which SDD artifact
matters**, and it is not the spec.

## 2. The landscape, measured

| Framework | Artifacts per unit | Size | Ceremony | Model of a spec |
|---|---|---|---|---|
| **GitHub Spec Kit** | 7+ files per feature | **~800 lines** | constitution → specify → clarify → plan → tasks → implement | Change artifact, branch-per-spec |
| **BMAD-METHOD** | 7 artifact types, **12+ agent personas** | most | Analyst → PM → Architect → SM → Dev → QA → Writer | Handoff chain |
| **OpenSpec** | ~4 files per change | **~250 lines** | proposal → design → tasks, **delta-based** | Delta against current behaviour |
| **Amazon Kiro** | in-IDE, EARS notation | — | IDE-native | Vendor-integrated |
| **`opensearch-bin-ingester`** | `SPEC.md` + `VERIFIED.md` per milestone | **274–518 / 22–144 lines** | 10 rules, one skill | Milestone contract + evidence ledger |

The published criticisms are more useful than the feature lists:

- **Spec Kit** — *"branch-per-spec model treats specs as change artifacts, not long-lived
  capability contracts"*, and on brownfield *"artifacts don't compound into system-level
  documentation."* A real case study found **agents ignoring the constitution's rules**,
  and needing *"context-rich explanations of why, not just what."*
- **BMAD** — *"if your team doesn't have structured processes, BMAD won't conjure them —
  it'll reproduce your chaos across seven agents."* Handoff failures between personas are
  a real debugging surface, and more agents means more tokens per cycle.
- **OpenSpec** — specs *"don't self-update during implementation"*; drift needs manual
  reconciliation.
- **All three** — *"a stale spec misleads agents that don't know any better, and they'll
  execute a plan that no longer matches reality without flagging anything wrong."*

And the overhead critique that applies to all of them: SDD is *"requirement documents
reborn"* unless specs stay lean and iterative; small fixes do not warrant the ceremony.

## 3. The finding that matters: a stronger model moves the risk

Most SDD scaffolding exists to compensate for models that lose the thread, cannot hold
context, or drift without checkpoints. An Opus- or Fable-class model needs much less of
that. But it fails differently:

> **A weak model fails visibly** — broken code, obvious nonsense.
> **A strong model fails invisibly** — plausible code, plausible tests, and a confident
> completion report for work it did not verify.

So the value of the artifacts inverts:

| Artifact | Value with a weak model | Value with a strong model |
|---|---|---|
| Elaborate spec templates | High — scaffolds thinking | **Falls.** The model can hold the design; sections get filled with filler. |
| Multi-persona handoffs (BMAD) | Medium — forces perspective changes | **Falls sharply.** One model role-playing seven agents burns tokens on handoff artifacts it wrote to itself. |
| Task decomposition | High | Moderate — still useful for commit boundaries. |
| **Checkable acceptance criteria** | High | **Rises.** The only defence against a confident wrong answer. |
| **A verification ledger** (`VERIFIED.md`) | Medium | **Highest.** It is the artifact that catches "I implemented it" when nothing ran. |
| **Prohibitions with a one-line why** | Medium | **Rises** — see below. |

> **D-113.** Spend the SDD budget on **acceptance criteria and the verification ledger**,
> not on spec ceremony. With a model that can hold a design, the scarce thing is not a
> plan — it is *evidence that the plan was executed and checked*.

### The "why" finding cuts against pure terseness
Spec Kit's case study — agents ignoring constitution rules until given *why* — is the one
result that argues **against** maximum token efficiency. The resolution is not paragraphs:
it is **one clause of why, attached to the rule**, which is what `AGENTS.md` § Never
already does ("never LIST on a hot path — *a LIST is priced like a PUT and returns ≤1000
keys*"). A bare prohibition gets rationalised around; a prohibition with its reason does
not.

## 4. Why pstore does not need Spec Kit or BMAD

pstore already has, before any code: 54 research documents, an architecture doc, a
roadmap with per-milestone exit conditions, 164 risk-ranked open questions, and
engineering standards naming ten architectural invariants as tests.

> **The corpus *is* the spec.** Running `/speckit.constitution` and `/speckit.specify`
> here would produce a *second* statement of things already stated — and duplication has
> already bitten this repository twice (`AGENTS.md` vs `INDEX.md`; the hand-maintained
> document counts). Spec Kit's own criticism — *artifacts don't compound into
> system-level documentation* — is exactly inverted here: the system-level documentation
> exists, and the risk is specs **fragmenting** it.

What is genuinely missing is thin: the layer between *"M0a's exit condition"* and *"a task
an agent can start and a way to know it finished."*

## 5. What to adopt

Their 10-rule standard, nearly intact — it is the leanest credible option at **23 lines**
— with four changes.

### Take as-is
1. Nothing is implemented before it is specified; a task without acceptance criteria is
   not ready to start.
2. One task = one commit = one change leaving the tree green.
3. Task IDs stable, never reused.
4. **Only the current milestone is decomposed in detail.** Tasks written three milestones
   ahead are wrong by the time they are reached.
5. **Acceptance criteria are checkable by something other than an opinion** — a test, a
   gate, a number inside a bound.
6. A spec is reviewed before code is written. *A wrong spec produces correct code solving
   the wrong problem, and every downstream gate passes.*
7. A wrong spec is amended, not worked around.
8. Consult the corpus before designing; contradicting a conclusion is allowed, doing it
   silently is not.

### Change 1 — cite `D-<n>`, do not invent requirement IDs
Theirs cites FR/NFR IDs from a requirements document. pstore has **113 numbered decisions
(`D-1`…`D-113`)** and 164 open questions already scattered through the corpus and
searchable. Inventing an FR taxonomy would be a second home for the same facts.

> **D-114.** A task cites the decision or open question it serves (`D-<n>`, `OQ-<n>`) or
> the document that specifies it. No new ID space.

### Change 2 — specs are **deltas**, borrowed from OpenSpec
A spec states **what changes** against the corpus, not what the system is. This is the
one idea worth importing, and it fits because the corpus already works this way: research
documents carry **correction banners** where a later finding overturned an earlier one.
Extending delta-thinking from findings to specs is continuous with existing practice.

### Change 3 — the cost section becomes the **request-amplification budget**
Theirs requires every spec to state its cost impact against numbered rules. pstore's
equivalent is `RA` — requests per operation, split W / Rseq / Rpar / List — which the
research docs already state per subsystem.

> **D-115.** Every spec states the change's `RA` and its sequential round-trip depth.
> "Unchanged" is a valid answer; silence is not.

### Change 4 — `VERIFIED.md` is the centrepiece, not an afterthought
Theirs is excellent and is the single artifact I would keep if forced to keep one. Its
shape, from a real milestone:

- One line per acceptance criterion, naming **the test and the command**.
- **The mutation each test is meant to catch** — *"Mutation verified killed (omitting
  `_version`)"*. A test whose catchable mutation cannot be named is coverage, not
  verification.
- **Drift recorded rather than hidden** — *"SPEC's test-table name for this row has
  drifted — tracked as M1.22."*
- **`NOT-RUN` and `OBSERVED-NOT` are required options.** One criterion reads
  *"OBSERVED-NOT, resolved by construction instead"*, then names the three runtime tests
  attempted and withdrawn as unfalsifiable, and points at the ADR.

That last property is the whole point, and it is what §3 says a strong model most needs:
**a structure where "I did not actually check this" is a first-class thing to write
down.** The script cannot check the evidence is true. It forces **enumeration**, which is
what catches the quiet omission of criterion 6.

## 6. Token budget

| | Lines |
|---|---|
| Spec Kit, per feature | ~800 |
| OpenSpec, per change | ~250 |
| obi `SPEC.md`, per milestone | 274–518 |
| **pstore target, per milestone** | **≤200 spec + ≤100 verified** |

Lower than theirs because the delta rule (Change 2) removes the restatement, and because
the corpus carries the reasoning. **A spec that explains *why* is duplicating the corpus;
it should link and move on.**

## 7. What we still do not solve

**Spec drift.** No framework surveyed solves it, and neither does this. Their own
`VERIFIED.md` contains a recorded instance — a test name in the spec that no longer
matched the test that ran, caught by a human reading both. Our mitigations are partial
and should be stated as such: the spec is a delta (less surface to drift), `VERIFIED.md`
names real test identifiers (drift becomes visible when the name does not resolve), and
a spec that turns out wrong is amended rather than worked around. **None of these is a
gate**, and per [`gate-design`](../../../.agents/skills/gate-design/SKILL.md) that makes
them preferences until something checks them.

One rung-3 gate now exists: **`scripts/check-verified.py`** (OQ-167, closed). It
asserts every acceptance criterion has an evidence line, that every test named resolves
to a real test in the tree, and that a line naming no test says `NOT-RUN` or
`OBSERVED-NOT`. Nine selftest cases, four of which check the *false-refusal* path,
because a gate that rejects legitimate input gets switched off and then enforces nothing.

⚠️ It closes only the mechanical half. It **cannot** check that the evidence is true —
that remains the "never claim a gate ran without running it" rule, which no script can
enforce. It also cannot see macro-generated tests, so a proptest-generated name will not
resolve; name the enclosing `#[test]` fn or abstain with a reason.

## 8. Open questions raised

- **OQ-165** — At what change size does a spec stop paying for itself? Theirs specs per
  *milestone*; OpenSpec per *change*. Milestone granularity may be too coarse once M0a
  decomposes.
- OQ-166 — Should spec review be a separate agent pass, or does the `review` skill's
  round budget cover it? A wrong spec is the failure every downstream gate misses, which
  argues for separate — but it is also another loop to bound.
- **OQ-167** — Gate `VERIFIED.md`: every named test resolves to a real test, every
  criterion has a line. Rung 3, and the only drift mitigation here that could be
  mechanical.
- OQ-168 — Does `NOT-RUN` get used honestly in practice, or does it decay into a rubber
  stamp? It is the artifact's load-bearing property and nothing enforces it.

## Sources

- `opensearch-bin-ingester` — `docs/internal/standards/sdd.md`, `.agents/skills/spec/SKILL.md`, `docs/internal/product/milestones/*/{SPEC,VERIFIED}.md` (sizes and shape measured directly)
- [github/spec-kit — GitHub](https://github.com/github/spec-kit)
- [Spec Kit vs BMAD vs OpenSpec: Choosing an SDD Framework in 2026 — DEV](https://dev.to/willtorber/spec-kit-vs-bmad-vs-openspec-choosing-an-sdd-framework-in-2026-d3j)
- [BMAD vs Spec Kit vs OpenSpec — Reenbit](https://medium.com/@reenbit/bmad-vs-spec-kit-vs-openspec-choosing-your-spec-driven-ai-framework-in-2026-a6996b3ebb8d)
- [OpenSpec vs Spec Kit: Lightweight vs Full Toolkit — codemyspec](https://codemyspec.com/blog/openspec-vs-spec-kit)
- [OpenSpec vs GitHub Spec-Kit — A Hands-On Comparison](https://ypyl.github.io/programming/2026/06/03/openspec-vs-spec-kit-sdd.html)
- [9 Best AI Tools for Spec-Driven Development in 2026 — MarkTechPost](https://www.marktechpost.com/2026/05/08/9-best-ai-tools-for-spec-driven-development-in-2026-kiro-bmad-gsd-and-more-compare/)
- [Spec-Driven Development for AI Agents: Governing Specs — TrueFoundry](https://www.truefoundry.com/blog/spec-driven-development-ai-agents)
- [Spec-Driven Development — Sometimes the Simpler Instructions.md is Enough](https://medium.com/@mpholoane/spec-driven-development-sometimes-the-simpler-instructions-md-is-enough-b960fbda772b)
- [GitHub Spec Kit Takes Off as Antidote to Piecemeal 'Vibe Coding' — Visual Studio Magazine](https://visualstudiomagazine.com/articles/2026/05/12/github-spec-kit-takes-off-as-antidote-to-piecemeal-vibe-coding.aspx)
