---
name: spec
description: Write a delta spec and its verification ledger before implementing. Use when starting a milestone, when a task lacks a checkable acceptance criterion, or when what to build is clearer than how it will be checked.
---

# Spec

One `SPEC.md` per milestone, in `docs/milestones/<id>/`, with a `VERIFIED.md` beside it.
**≤200 lines of spec, ≤100 of verified.** If it is longer, it is restating the corpus.

## ⚠️ A spec is a delta, not a description

The 54-document corpus already says what the system is. **State only what changes.** A
spec that explains *why* is duplicating a research document — link to it and move on.

Read the corpus first with the [`research`](../research/SKILL.md) skill. Contradicting a
conclusion is allowed; doing it silently is not — that is a correction banner on the
document it overturns.

## Contents

| Section | Must answer | Cap |
|---|---|---|
| **Serves** | Which `D-<n>` decisions or `OQ-<n>` questions? **No new ID space** — cite what exists. | 3 lines |
| **Delta** | What changes against the corpus, and explicitly what does **not** | 30 lines |
| **Acceptance criteria** | How we will know, checkably | 20 lines |
| **Test plan** | Per criterion: the test that must fail first, and the mutation it catches | 40 lines |
| **RA budget** | Requests per operation (W / Rseq / Rpar / List) and sequential depth. "Unchanged" is valid; silence is not | 5 lines |
| **Risks** | What could make this wrong, and what would reveal it | 10 lines |
| **Tasks** | One commit each, stable IDs `M<n>.<k>`, never reused | — |

## Acceptance criteria carry the weight

Each must be checkable by something other than an opinion — a test that passes, a gate
that goes green, a number inside a bound.

- ❌ "Writes are batched efficiently" → ✅ "A 10k-document write issues exactly **1** PUT, asserted by the request-class counter"
- ❌ "Cold queries are fast" → ✅ "A cold vector query's **sequential** blob depth is ≤3, asserted against the fault-injecting store"
- ❌ "Recovery works" → ✅ "Bundle recovery finds every un-folded record across 1,000 simulation seeds with injected node death"

⚠️ If you cannot write a checkable criterion, you do not yet understand the task well
enough to implement it. **That is the useful signal, not an obstacle.**

## Name the mutation, not just the test

For each test: **what wrong behaviour would it detect?** A test whose catchable mutation
cannot be named is coverage, not verification — and `cargo mutants` will say so later, more
expensively. → [`tdd`](../tdd/SKILL.md)

## ⚠️ VERIFIED.md is the point

This is the artifact that matters most, because of how a strong model fails: not with
broken code, but with **plausible code and a confident completion report for work it did
not verify.**

One line per acceptance criterion, naming:

1. **The test** — a real, resolvable identifier.
2. **The command** that ran it.
3. **The mutation verified killed**, if one was.

Three rules that make it worth having:

- ⚠️ **`NOT-RUN` and `OBSERVED-NOT` are required options**, not failures. A criterion
  verified by construction rather than by test says so, and names what was attempted.
- ⚠️ **Record drift rather than hiding it.** If the spec names a test that no longer
  matches what ran, say so and file a task.
- ⚠️ **Never write an evidence line for something you did not run.** That is
  the "never claim a gate ran without running it" non-negotiable, and every other rule
  rests on it.

Enumeration is the mechanism. Nothing can check the evidence is *true*; forcing a line per
criterion is what catches the quiet omission of criterion 6.

## Only the current milestone

Decompose in detail only what is next. Tasks written three milestones ahead are wrong by
the time they are reached — not because the plan was bad, but because the intervening work
changes what the right task is.

## Get it reviewed before writing code

⚠️ **A wrong spec produces correct code solving the wrong problem, and every downstream
gate passes.** Spec review is the cheapest gate available and the only one that catches
this.

## If it turns out to be wrong

Normal and expected. **Amend the spec**; do not silently implement something adjacent.
If the correction overturns a research conclusion, add a correction banner to the document
it overturns — the wrong reasoning stays, because it is evidence about how we think.
