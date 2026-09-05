# Skills

Procedures, written to the [Agent Skills](https://agent-skills.org) spec so any tool
that reads `SKILL.md` can use them. Files under `.claude/` are **thin adapters that
delegate here** — they never contain logic of their own.

## Progressive disclosure

This repository holds a research corpus compiled before any code existed. Loading it
into every session is impossible, and would be useless if it were possible. Four
layers, each loaded only when the one above says it is relevant.

| Layer | What | Loaded |
|---|---|---|
| **0** | [`AGENTS.md`](../../AGENTS.md) — an index, deliberately | every session |
| **1** | The `description:` line of every skill | every session (a few hundred words) |
| **2** | A skill's body | when that skill is invoked |
| **3** | The corpus, standards, design docs | when a skill says to read one |

⚠️ **Layer 1 is the whole mechanism.** A skill's `description` is the only thing an
agent sees before deciding to load it, so it must say **when to use this**, not what it
is. "Review the diff" is a title; "before every commit that changes code" is a
description that gets the skill loaded at the right moment.

**Layer 3 is never loaded wholesale.** `docs/research/INDEX.md` gives every document a
one-line finding, which is usually enough on its own.

## The rules

1. **Skills call scripts in `scripts/`, never a tool-specific built-in.** The script is
   the enforcement path and must work for a developer on any tool.
2. **`SKILL.md` frontmatter is `name` and `description`, both required**, `name` equals
   the directory name.
3. **No vendor-specific syntax in `AGENTS.md` or any `SKILL.md`.** Claude Code's
   `@import` belongs in `CLAUDE.md`, which is the adapter.
4. **A skill is a procedure, not an explanation.** Rationale lives in the corpus; the
   skill says what to do and links to why.
5. **Adapters stay thin.** A `.claude/commands/*.md` containing a procedure rather than
   a pointer is a fork waiting to drift.

## The skills

<!-- index:skills:start -->
| Skill | Use when |
|---|---|
| [`gate-design`](gate-design/SKILL.md) | Before adding any check, gate, or review step — and before writing an instruction that says "remember to" or "make sure you" |
| [`research`](research/SKILL.md) | Before any web search or design argument about object storage, cost, ANN indexes, or cluster topology — the answer is usually already here, with numbers |
| [`review`](review/SKILL.md) | Before every commit that changes code — defines what the reviewer is given, what it is deliberately denied, and the round budget that stops the loop |
| [`tdd`](tdd/SKILL.md) | Writing any code — covers the red-green cycle, what to assert, and why coverage alone does not answer the question |
<!-- index:skills:end -->

## Why only four

⚠️ **A skill that cannot run is worse than no skill**, because it reads as capability.
There is no backlog yet, so there is no `next-task` or `milestone`; there is almost no
code, so there is no `bench` or `cost-budget`. Each arrives when the thing it operates
on exists. See [`docs/research/09-rust-stack/agent-harness.md`](../../docs/research/09-rust-stack/agent-harness.md)
for what was deliberately left out and why.
