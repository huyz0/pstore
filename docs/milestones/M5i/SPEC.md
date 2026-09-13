# M5i — MS MARCO, the evaluation M5 could not run

**Serves:** M5's exit criterion, recorded as `NOT-RUN` since
[M5c](../M5c/VERIFIED.md): *"MS MARCO is `NOT-RUN`, blocked on data rather than effort"*, and
in [`roadmap.md`](../../research/11-design/roadmap.md) as *"no network and no dataset here"*.

⚠️ **Both halves of that blocker are false on this machine.** A range request to the official
corpus returns `206`, and the archive is **1.06 GB** against 680 GB free. The entry was written
when it was true and never re-tested; finding that out cost one `curl -I`.

## ⚠️ Why this matters more than the gate that exists

`scripts/ndcg.sh` reports **NDCG@10 = 1.0000**, and M5c's own ledger says why that is not a
quality claim: relevance is *planted* — each query owns a marker term in exactly five
documents — so any IDF-weighted scorer finds them. What the gate demonstrates is that the
corpus **discriminates**, because the control with IDF removed falls to 0.5441.

So the project has never measured whether its BM25 ranks *real* text well. MS MARCO is the
standard answer, and it comes with a published reference: **BM25 scores ≈0.18 MRR@10** on the
passage-ranking dev set. That makes the criterion checkable against something we did not
choose.

## ⚠️ Memory is the design constraint, and this machine has died of it twice

`AGENTS.md` records it: the build ceilings exist because "a mutation sweep on top of that
killed the WSL2 VM twice". The collection is **8.8M passages**, ~3.2 GB of text, and
`text::build` holds every posting in a `BTreeMap` — a single-segment index would be tens of
gigabytes against 19 GB available.

> **Sharded into segments, which is what the engine does anyway.**

Each shard is built, sealed, and dropped before the next begins, so peak memory is one shard's
postings rather than the corpus's. ⚠️ That also makes this an evaluation of the **multi-segment
path** [M5f](../M5f/SPEC.md) built and [M5h](../M5h/SPEC.md) extended — global statistics
merged across segments, one fusion over the union — rather than of a single index. Which is
the path a real deployment uses.

## Delta

- `crates/pstore-query/examples/msmarco.rs` — reads the corpus, shards it into segments,
  queries the dev set across all of them, and reports **MRR@10** and **NDCG@10** against the
  official qrels.
- `scripts/msmarco.sh` — fetches the archive if absent, then runs the example. ⚠️ **Not a
  gate**, and not in `gates.sh`: it needs a 1 GB download and minutes of indexing, which is the
  same argument that keeps `recall.sh` and `depth.sh` out of `cargo test`.

**Does not add** — **a CI gate on the number.** The roadmap wants "NDCG/MRR as CI gates"; that
needs the corpus present in CI, which is a different problem from measuring it once.
**Reranking, learned sparse, or query expansion** — this measures the BM25 that exists.
**A claim about absolute quality beyond the reference.** ⚠️ Our tokenizer is deliberately
minimal — `analyze` splits on non-alphanumerics and lowercases, with no stemming and no
stopwords, and M5c says why — so a gap against a stemmed baseline is expected and is a
statement about the analyzer, not the scorer.

## Acceptance criteria

1. **The evaluation runs end to end** on the full 8.8M-passage collection, sharded, and
   reports MRR@10 and NDCG@10 over the dev queries with a stated query count.
2. ⚠️ **Peak resident memory stays under 8 GB**, measured and reported. A run that needs the
   machine's whole memory is one that kills it, and the shard size is the knob.
3. ⚠️ **The number is compared against the published BM25 reference (~0.18 MRR@10)**, and the
   comparison is reported whichever way it falls. A result far *above* it would mean the
   harness is scoring against its own qrels; far below means the analyzer, and both are
   findings.
4. **Every query is answered from the multi-segment path** — `pstore_query::query` over every
   shard, with statistics merged across them, not a per-shard best.
5. ⚠️ **The statistics actually span the shards.** The same evaluation with per-segment
   statistics is reported alongside, because M5c measured that global IDF changes the top-1
   and this is the first corpus where the size of that effect can be seen on real text.
6. **`provisional`**, and said so: WSL2, and a single run.
7. Gates green, and `scripts/ndcg.sh` and `scripts/recall.sh` unaffected.

## Test plan

| # | Test | The mutation it kills |
|---|---|---|
| 1 | `scripts/msmarco.sh`, reported in the ledger | — |
| 2 | the harness prints peak RSS | a shard size that only works on a machine with more memory |
| 3 | the reference comparison, in the ledger | a harness scoring against qrels it derived, which lands suspiciously high |
| 5 | the two-arm run, in the ledger | statistics gathered per shard, which is M5c's defect at corpus scale |

⚠️ This milestone's artifact is a **measurement**, not a behaviour, so its evidence is a
reported number and not a passing test. The ledger says so per criterion rather than implying
a test exists.

## RA budget

Unchanged — nothing in the read or write path changes. The harness uses a `MemoryStore`.

## Risks

- ⚠️ **8.8M passages through `text::build` may still not fit even sharded**, depending on how
  small the shards have to be; too many shards makes each query a wide fan-out and the run
  slow. The shard count is the tuning knob and criterion 2 is the bound it answers to.
- **A single run on WSL2 is one sample.** MRR over ~7k queries is stable, but the timing
  figures are not, and only the ranking numbers are claimed.
- ⚠️ **The comparison could embarrass the analyzer**, and that is the point: no stemming and no
  stopwords against a reference that has both. Reporting a gap is the honest outcome, and
  hiding it by quietly adding a stemmer would make the number meaningless.

## Tasks

| Id | Commit |
|---|---|
| **M5i.1** | The harness, sharded, with the corpus fetched rather than vendored |
