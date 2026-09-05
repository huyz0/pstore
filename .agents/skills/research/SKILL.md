---
name: research
description: Navigate the research corpus instead of reading it. Use before any web search or design argument about object storage, cost, ANN indexes, or cluster topology — the answer is usually already here, with numbers.
---

# Using the corpus

`docs/research/` was compiled **before any code existed** and answers most design
questions with measured or cited numbers.

## ⚠️ Never read it wholesale

**Start at [`docs/research/INDEX.md`](../../../docs/research/INDEX.md).** Every document
has a one-line finding there, which is usually the whole answer. Read a full document
only when the one-liner is insufficient — and then read *that* document, not its
neighbours.

## Routing

| Question about | Go to |
|---|---|
| Cost, pricing, request economics | `02-object-storage/cost-and-latency.md`, `10-benchmarks-cost/` |
| What the blob store can and cannot do | `02-object-storage/api-semantics.md` |
| CAS, manifests, consistency | `03-metadata-consistency/` |
| Placement, membership, AZs, failure | `04-cluster/` |
| WAL, segments, compaction, batching | `05-storage-engine/` |
| ANN, quantization, BM25, filtering | `06-indexing/` |
| Cache, working set, S3:cache ratio | `07-caching/` |
| SIMD, memory, CPU, Rust technique | `09-rust-stack/` |
| Why a decision was made | search for `D-<n>` — decisions are numbered across all docs |
| What we do not know | `00-plan/open-questions.md` — risk-ranked, `OQ-<n>` |

## Three rules

1. ⚠️ **Correction banners win.** Several documents carry `⚠️ Corrected`, `C-1`, `M-1`
   banners where a later finding overturned an earlier one. When a section and a banner
   disagree, **the banner wins** — the section is left in place so the reasoning is
   auditable, not because it is still true.
2. ⚠️ **Do not re-derive what the corpus answers.** If a number is there, cite it. If
   you believe it is wrong, that is a correction banner and a new finding — not a
   silent recalculation.
3. **Provisional numbers are labelled.** Anything measured on WSL2 or against an
   emulator is relative, not absolute, and says so. Do not promote one to a fact.

## When the corpus does not answer it

Then it is genuinely new, and:

- Add the question to `00-plan/open-questions.md` with **why it matters** and **how to
  find out** — those two fields are what make it actionable rather than a wish.
- If the answer changes an existing conclusion, add a **correction banner** to the doc
  it overturns and cross-link both ways. Do not silently edit the old claim; the wrong
  reasoning is evidence about how we think.
