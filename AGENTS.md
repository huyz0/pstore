# pstore

A masterless, object-storage-native search engine in Rust — vector ANN + BM25 +
filtered hybrid search — where the blob store is the **only** durable tier, for data
*and* metadata. Rust, 1M tenants × ~50 indexes, 10,000 nodes, no control plane.

This file is loaded into every session by every agent tool. It is **deliberately an
index**. Detail lives in the linked files.

## Start here

- [docs/research/INDEX.md](docs/research/INDEX.md) — the map. Read this before anything else.
- [docs/research/11-design/architecture.md](docs/research/11-design/architecture.md) — the design, and the five constraints that generated it
- [docs/research/11-design/roadmap.md](docs/research/11-design/roadmap.md) — milestones in execution order
- [docs/research/00-plan/open-questions.md](docs/research/00-plan/open-questions.md) — what we do not know, risk-ranked
- [docs/research/09-rust-stack/engineering-standards.md](docs/research/09-rust-stack/engineering-standards.md) — modularity, coverage, the invariant tests
- [dev/README.md](dev/README.md) — containerized WSL2 dev environment

## The research corpus

[docs/research/](docs/research/INDEX.md) is a corpus compiled **before any code
existed**: the cost model, prior art read from source, and this project's own design.

⚠️ **Never read it wholesale.** `INDEX.md` gives every document a one-line finding —
that is usually enough. Use the [`research`](.agents/skills/research/SKILL.md) skill
to navigate rather than to read.

⚠️ Several documents carry **correction banners** (`C-1`, `M-1`, `⚠️ Corrected`) where a
later finding overturned an earlier one. **When a section and a correction banner
disagree, the banner wins.**

## The five load-bearing conclusions

Everything else follows from these. They are stated with sources in `INDEX.md`.

1. **A PUT costs 12.5 GETs; LIST is PUT-priced for ≤1000 keys.** Batch writes, read
   freely, never LIST.
2. **~30 ms per round trip ÷ a 100 ms budget = ~3 sequential fetches.** This
   disqualifies graph ANN indexes and selects SPANN.
3. **Blob compare-and-swap exists on every cloud since 2024.** It is what makes a
   masterless metadata plane possible, and CAS *is* the fencing mechanism — so no
   locks or leases are needed for correctness.
4. **CAS tops out near 5 writes/s per key, and per-index flushing has a cost floor
   independent of data volume.** Bulk writes use contention-free lanes in cross-tenant
   bundles; the tenant is the CAS unit.
5. **Nodes own nothing**, so scaling needs no rebalancing, S3 is our free cross-AZ
   replication, and an AZ loss is a cold-start event rather than a data event.

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
| [`tdd`](.agents/skills/tdd/SKILL.md) | Writing any code — covers the red-green cycle, what to assert, and why coverage alone does not answer the question |
<!-- index:skills:end -->

**Progressive disclosure.** This file is layer 0. Skill *descriptions* are layer 1 (a
few hundred words). A skill's *body* is layer 2. The corpus and standards are layer 3,
loaded only when a skill says to read one — never wholesale.

## Non-negotiables

1. **Never claim a test passes, a gate ran, or a number was measured, without having
   done it.** **No script enforces this and none can.** Every other rule rests on it.
   A green gate reported by someone who did not run it is worth less than no gate.
2. **Never move a threshold in the weakening direction, and never delete or weaken a
   test, to make a check pass.** Coverage and mutation floors are thresholds.
3. **The test is written first and observed to fail.** A test never seen red is not
   known to test anything.
4. **`unsafe` exists in exactly one crate**, `pstore-kernel`. Enforced by
   `unsafe_code = "forbid"` at the workspace root — a stray `unsafe` block is a compile
   error, and `git diff Cargo.toml` is the complete audit.
5. **A new check is deterministic by default.** Work down the ladder in
   [`gate-design`](.agents/skills/gate-design/SKILL.md). **If the rule can be stated as
   a predicate over files in the tree, an agent must not be asked to check it.**
6. **Numbers measured on WSL2 or against an emulator are `provisional`** and say so.
   They are relative, never absolute.

## Gates

⚠️ **This is the honest answer to "what actually runs".** A skill may name a script
that does not exist; `scripts/` is the truth on the day you read it.

<!-- index:gates:start -->
| Command | Enforces |
|---|---|
| `cargo fmt --check` | formatting |
| `cargo clippy --all-targets -- -D warnings` | the workspace lint set, including `unsafe_code = "forbid"` |
| `cargo test` / `cargo nextest run` | tests |
| `cargo llvm-cov --fail-under-lines 95` | engineering-standards D-110 |
| `cargo mutants` | D-111 — the gate that makes coverage mean something |
| `cargo deny check` | licences, advisories, and the `object_store` ban outside `pstore-blob` |
| `scripts/check-links.sh` | every relative markdown link resolves |
| `scripts/build-index.sh --check` | the generated regions in `AGENTS.md` are current |
<!-- index:gates:end -->

**Named by skills but absent today:** `scripts/review.sh` runs, but the reviewer
subagents it prepares context for are invoked by hand. There is no backlog, so there
is no `next-task` or `milestone` skill — deliberately, because a skill that cannot run
is worse than no skill.

## Never

- Never read the research corpus wholesale. Route through `INDEX.md`.
- Never push unless asked.
- Never commit a tree you know is broken, including "I will fix it next commit".
- Never widen scope silently. Doing more than asked breaks one-change-one-commit as
  surely as doing less.
- Never add a blob request that scales with records, documents, or indexes.
- Never LIST on a read, write, or startup path.
