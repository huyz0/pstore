# pstore

A masterless, object-storage-native search engine in Rust — vector ANN + BM25 +
filtered hybrid search — where the blob store is the **only** durable tier, for data
*and* metadata. Rust, 1M tenants × ~50 indexes, 10,000 nodes, no control plane.

This file is loaded into every session by every agent tool. It is **deliberately an
index**. Detail lives in the linked files.

## Start here

- **[docs/research/INDEX.md](docs/research/INDEX.md) — the map.** Every document has a
  one-line finding, the five load-bearing conclusions with sources, and the glossary.
  Route through it; it is layer 3's table of contents, not a document to read.
- [dev/README.md](dev/README.md) — containerized WSL2 dev environment. Not in the corpus.

⚠️ **Never read the corpus wholesale.** Use the
[`research`](.agents/skills/research/SKILL.md) skill to navigate rather than to read.

⚠️ Several documents carry **correction banners** (`C-1`, `M-1`, `⚠️ Corrected`) where a
later finding overturned an earlier one. **When a section and a banner disagree, the
banner wins.**

## Skills

Procedures, in [.agents/skills/](.agents/skills/README.md), written to the Agent
Skills spec so any tool reading `SKILL.md` can use them. `.claude/` holds **thin
adapters that delegate here** and contain no logic.

<!-- index:skills:start -->
| Skill | Use when |
|---|---|
| [`gate-design`](.agents/skills/gate-design/SKILL.md) | Before adding any check, gate, or review step — and before writing an instruction that says "remember to" or "make sure you" |
| [`research`](.agents/skills/research/SKILL.md) | Before any web search or design argument about object storage, cost, ANN indexes, or cluster topology — the answer is usually already here, with numbers |
| [`review`](.agents/skills/review/SKILL.md) | Before every commit that changes code — defines what the reviewer is given, what it is deliberately denied, and the round budget that stops the loop |
| [`spec`](.agents/skills/spec/SKILL.md) | Starting a milestone, when a task lacks a checkable acceptance criterion, or when what to build is clearer than how it will be checked |
| [`tdd`](.agents/skills/tdd/SKILL.md) | Writing any code — covers the red-green cycle, what to assert, and why coverage alone does not answer the question |
<!-- index:skills:end -->

**Progressive disclosure.** This file is layer 0. Skill *descriptions* are layer 1 (a
few hundred words). A skill's *body* is layer 2. The corpus and standards are layer 3,
loaded only when a skill says to read one — never wholesale.

## Non-negotiables

⚠️ Cite these **by name, never by number**. The numbering is for reading order and shifts when a rule is added — a numbered cross-reference in another file goes stale silently. It already did once.


1. **Nothing is implemented before it is specified**, and a task without a *checkable*
   acceptance criterion is not ready to start. Criteria are checkable by a test, a gate,
   or a number inside a bound — never by an opinion.
   → [`spec`](.agents/skills/spec/SKILL.md)
2. **Never claim a test passes, a gate ran, or a number was measured, without having
   done it.** **No script enforces this and none can.** Every other rule rests on it.
   A green gate reported by someone who did not run it is worth less than no gate.
3. **Never move a threshold in the weakening direction, and never delete or weaken a
   test, to make a check pass.** Coverage and mutation floors are thresholds.
4. **The test is written first and observed to fail.** A test never seen red is not
   known to test anything.
5. **`unsafe` exists in exactly one crate**, `pstore-kernel`. Enforced by
   `unsafe_code = "forbid"` at the workspace root — a stray `unsafe` block is a compile
   error, and `git diff Cargo.toml` is the complete audit.
6. **A new check is deterministic by default.** Work down the ladder in
   [`gate-design`](.agents/skills/gate-design/SKILL.md). **If the rule can be stated as
   a predicate over files in the tree, an agent must not be asked to check it.**
7. **Numbers measured on WSL2 or against an emulator are `provisional`** and say so.
   They are relative, never absolute.

## Gates

⚠️ **This is the honest answer to "what actually runs".** A skill may name a script
that does not exist; `scripts/` is the truth on the day you read it.

<!-- index:gates:start -->
| Command | Enforces |
|---|---|
| `scripts/check-dev-env.sh` | the local build ceilings exist. ⚠️ `dev/README.md` step 2 said "copy the cargo config" and it had never been done here, so every build used all 20 cores — a mutation sweep on top of that killed the WSL2 VM twice. Rung 3 replacing an instruction that says "remember to" |
| `cargo fmt --check` | formatting |
| `cargo clippy --all-targets -- -D warnings` | the workspace lint set, including `unsafe_code = "forbid"` |
| `cargo test` / `cargo nextest run` | tests |
| `cargo llvm-cov --fail-under-lines 95` | engineering-standards D-110 |
| `scripts/coverage.sh --fail-under-regions 95` | the **region** floor on the crates that ship; the test-only set is derived from the dependency graph, not named |
| `scripts/mutants.sh` | D-111 — the gate that makes coverage mean something. **Incremental by default**: a full sweep is 462 mutants × a 70-second suite, so the bare command tests only what this branch changed, and the full sweep is nightly and sharded |
| `cargo deny check` | licences, advisories, and the `object_store` ban outside `pstore-blob` |
| `scripts/check-links.sh` | every relative markdown link resolves |
| `scripts/build-index.py --check` | the generated regions in `AGENTS.md` are current, and the Gates table matches what CI runs |
| `scripts/check-verified.py` | every acceptance criterion has an evidence line, and every test it names resolves (OQ-167) |
| `scripts/recall.sh` | recall@10 above its floor (D-35). Runs outside `cargo test`, so `cargo mutants` does not rebuild a gate-scale corpus once per mutant |
| `scripts/ndcg.sh` | ranking quality above its floor (D-31) — **and a control ranker below it**, so a judged set we generated cannot pass everything |
| `scripts/depth.sh` | round-trip depth and query bytes **at gate scale** (20,000 rows). Outside `cargo test` for the same reason `recall.sh` is: a sweep reruns the suite once per mutant |
| `git config core.hooksPath scripts/githooks` | **run once per clone.** Refuses a commit whose tree is red — the rule AGENTS.md already states, moved from an instruction to a predicate |
<!-- index:gates:end -->

**Named by skills but absent today:** `scripts/review.sh` runs, but the reviewer
subagents it prepares context for are invoked by hand. There is no backlog, so there
is no `next-task` or `milestone` skill — deliberately, because a skill that cannot run
is worse than no skill.

## Never

These are the five load-bearing conclusions in the only form layer 0 needs. The
reasoning, with numbers and sources, is in `INDEX.md`.

- Never read the research corpus wholesale. Route through `INDEX.md`.
- Never **LIST** on a read, write, or startup path. A LIST is priced like a PUT and
  returns ≤1000 keys; keys are derived, not discovered.
- Never add a blob request that scales with **records, documents, indexes, or elapsed
  time per index**. Requests scale with nodes and bytes.
- Never put a **data-dependent chain** of blob fetches on a user-facing path. The budget
  is three sequential round trips; fan-out within a round is free, depth is not.
- Never add a **lock, lease, or leader election** for correctness. CAS on a blob is the
  fencing mechanism — a paused or partitioned writer is already safe.
- Never give a node **ownership** of data. Nodes own nothing; that is what makes scaling
  free and an AZ loss a cold-start event.
- Never push unless asked.
- Never commit a tree you know is broken, including "I will fix it next commit".
- Never widen scope silently. Doing more than asked breaks one-change-one-commit as
  surely as doing less.
