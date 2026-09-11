# M5d — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ **This milestone was specced, built, measured, and REVERTED. Nothing of it ships.** The
ledger exists because a spec with no ledger is indistinguishable from work in progress — the
gate said `skip M5d: no VERIFIED.md yet (in progress, not drift)` on every run for as long as
that was untrue. What is recorded here is what was learned, not what was delivered.

The two findings, both in [SPEC.md](SPEC.md)'s amendment banners and in
[C-15](../../research/06-indexing/full-text-search.md):

1. **The sidecar is the wrong place, by 22×.** Built and measured at gate scale, the block
   table grows the term dictionary from **28,030 to 616,994 bytes** — and that object is
   fetched `Pinned` and **whole, on every text query**. A dictionary scales with *terms*; a
   block table scales with *postings*. ⚠️ The existing test
   `the_term_dictionary_scales_with_terms_not_documents` went red and **was right**; weakening
   it to land this would have broken the non-negotiable about never weakening a test to make a
   check pass.
2. **The pruning it was written for skips zero bytes**, measured afterwards in **18
   configurations** — widths 1/2/3 × `top_k` 1/10/100 × mixed and all-tabled. So the metadata
   would have gone into every segment forever to enable a skip that never fires, which is what
   closed backlog items 6 and 6b and overturned that file's ordering principle 2.

## Criteria

1. **The bracket over BM25's domain** — `NOT-RUN`. The property test was written and passed
   during the reverted implementation; nothing of it survives in the tree, so this ledger does
   not claim it. ⚠️ What the domain analysis in the spec established is kept, because it is the
   reusable part: at `idf < 0` the bracket inverts and at `b > 1` the score stops being
   monotone in `tf`, so a property test drawing uniformly would fail and the tempting fix is to
   weaken the bound rather than the domain.
2. **Search results unchanged by the format** — `NOT-RUN`. No format changed, so nothing could
   move. `scripts/ndcg.sh` and the text suite are green on the reverted tree, which is a
   statement about M5c, not about this.
3. **Depth and bytes unchanged** — `OBSERVED-NOT`, and it is the criterion the milestone failed.
   Depth was unchanged; **bytes were not**, and criterion 6 is where that was caught. A block
   table riding in an object fetched whole is not free just because it costs no round trip.
4. **A version 1 dictionary still reads** — `NOT-RUN`. The reader still refuses any version but
   its own, exactly as the spec found. ⚠️ That defect is **real and still present**: `key-layout`'s
   rule is "readers must handle N and N−1", and today a dictionary version bump would make every
   existing segment unreadable rather than merely unpruned. It is not a backlog row because
   nothing is bumping the version — and it will bite whatever does.
5. **A short term's posting bytes identical** — `NOT-RUN`.
6. **The sidecar's growth bounded and measured** — `OBSERVED-NOT`, and it is the only criterion
   this milestone settled: **measured at 28,030 → 616,994 bytes, +2,100% against a budget of
   under 25%.** ⚠️ Marked `OBSERVED-NOT` rather than claimed, because the measurement was made
   by the reverted implementation's own test and **nothing in the tree reproduces it today**.
   The number is the amendment's in [SPEC.md](SPEC.md), and it settled the milestone.
7. **A v2 list decoded in full equals v1** — `NOT-RUN`.
8. **Gates** — `NOT-RUN` for this milestone's code, which does not exist. The tree the revert
   left behind is green and has stayed green through M5f, M5g, M6c–M6f.

## What replaces it

Nothing, and that is the finding. Doc-ordered block-max is closed by measurement
([C-15](../../research/06-indexing/full-text-search.md)); the escape, if there is one, is
OQ-45's other half — **impact-ordered postings** — and choosing a layout needs the eval set
M5's MS MARCO exit is blocked on. The harness that produced the 18 configurations is
[`blockmax.rs`](../../../crates/pstore-index/examples/blockmax.rs) and it outlived the
milestone.
