---
name: gate-design
description: Choose the cheapest reliable mechanism for a new check. Use before adding any check, gate, or review step — and before writing an instruction that says "remember to" or "make sure you".
---

# Designing a check

⚠️ **An agent instruction is the weakest possible enforcement.** It costs tokens on
every session, is invisible to anyone not reading the prompt, gives a different answer
each run, and **dies with the session that wrote it.** Reach for it last.

## The ladder

Work down it. Stop at the first rung that can carry the rule.

| # | Mechanism | Why it beats the one below |
|---|---|---|
| **1** | **Make the bad state unrepresentable** | Nothing to check. `TenantId` and `IndexId` are distinct types, so passing one where the other belongs cannot compile. |
| **2** | **Derive from a source of truth** | `unsafe_code = "forbid"` at the workspace root makes `git diff Cargo.toml` the complete unsafe audit — no list to maintain. |
| **3** | **Deterministic gate** (script or cargo command) | Same answer every run, for everyone, in CI, in milliseconds. |
| **4** | **Commit the fact, diff the change** | A recorded backend capability profile makes a semantic divergence a diff a human reads. |
| **5** | **Generate, don't maintain** | A hand-written count of documents goes stale on the next commit. `build-index.sh` regenerates it. |
| **6** | **Agent review** | Only for what no predicate can express. |
| **7** | **An instruction in a prompt** | Only when nothing above applies — and say so out loud. |

## The test

**Can you state the rule as a predicate over files in the tree?** If yes it is rung 3
or better, and an agent must not be asked to do it.

- "no crate but `pstore-kernel` relaxes the unsafe lint" → predicate → script
- "every relative markdown link resolves" → predicate → script
- "coverage ≥ 95%" → predicate → `cargo llvm-cov`
- "the diff does what the task said" → **not** a predicate → agent
- "would this test fail if the code were wrong" → **not** a predicate → agent (or `cargo mutants`, which is rung 3 and better)

## When an agent is genuinely superior

Reserve rung 6 for judgement needing intent reconstructed:

1. **Task-versus-diff mismatch.** No script has the task in its head.
2. **Would this test catch a bug?** Mutation score approximates it; naming the
   surviving mutant does not.
3. **Is this request rate scaling with the wrong thing?** Requires reading the design.
4. **Is this argument sound?** Research, design decisions, weighing alternatives.

Everything else an agent is asked to "check" is a script nobody has written yet.

## ⚠️ The failure mode this exists to prevent

**A check that cannot fail deterministically tends to report success while checking
nothing.** Two found in this repository already:

- `rust_2018_idioms` was configured at the same lint priority as the individual lints,
  so `cargo clippy` refused to compile the workspace at all. Caught by *running* it.
- The document and question counts in `INDEX.md` were hand-edited on every commit,
  and drifted within three commits. Fixed by moving to rung 5.

Neither was caught by an instruction telling an agent to be careful.

## Before you add one

1. **Name the rung.** If it is 6 or 7, write one sentence in the commit body saying why
   1–5 cannot carry it.
2. **Make it fail first.** Construct the input it must reject and watch it reject that
   input. A gate never observed failing is not known to gate anything — same rule as
   [`tdd`](../tdd/SKILL.md), for the same reason.
3. **Check for a false-refusal path.** A gate that rejects legitimate input gets
   switched off, and then it enforces nothing.
4. **Wire it in**, or it is a preference — `.github/workflows/ci.yml`, or a cargo alias.
5. **State what it cannot see.** A documented blind spot beats an undocumented mechanism
   that half works.
