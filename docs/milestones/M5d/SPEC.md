# M5d — The block-max **format**, and the version rule the reader does not follow

**Serves:** **D-13** ("block-max/skip metadata goes in the **index section**; posting payloads
go in the **data section** … the single most important FTS layout decision for object
storage"), **OQ-45**, and **C-11**, which recorded the omission rather than leaving it open.

**Depends on** [M5c](../M5c/SPEC.md), which built BM25 and said what it had not built.

## ⚠️ Why now, and not later

Every other item carried forward costs the same next month. This one does not: segments are
immutable, so **every segment written before this lands is permanently unprunable**, and the
retrofit is a compaction pass whose size is the corpus's. `modalities-and-sequencing.md` §6
lists block-max as a **prerequisite** for deferring FTS safely, and C-11 accepted the debt
explicitly rather than by omission.

## ⚠️ The failure this milestone exists to prevent — and it is not "slow queries"

Pruning that drops a document which belonged in the top-k is **a wrong answer, returned
confidently, at a lower cost**. Every performance number improves. Nothing fails. The whole
milestone is therefore built around one criterion — *pruned and unpruned agree* — and the
speed is only worth having because that criterion holds.

The specific way to get it wrong here is subtle enough to name in advance: **storing a
per-block score**. BM25's block bound depends on `avg_len`, which under D-30's two-pass IDF is
a **global** statistic the writer does not have. A bound computed from this segment's average
is too low on a corpus whose average is higher, and pruning against a too-low bound silently
drops real hits. So what is stored is what the writer actually knows — `tf` and `fieldnorm`
extrema, all exact local facts — and the bound is computed at query time from the query's own
`(k1, b, avg_len)`.

## ⚠️ AMENDED AFTER IMPLEMENTATION — the sidecar is the wrong place, and it is 22× wrong

Criterion 6 said the block table would add "under **25%**" to the dictionary sidecar. Built and
measured at gate scale — 20,000 documents, 120 tokens each, Zipf-ish over a 30,000-term
vocabulary — it adds **2,100%**:

| | bytes |
|---|---|
| dictionary, v1 | **28,030** |
| dictionary, v2 with the block table | **616,994** |

The reason is structural and should have been obvious from the two numbers the design already
had. A dictionary scales with **terms**; a block table scales with **postings**, at 24 bytes
per 128 of them. 4.5M postings is ~850 KB of metadata against a 28 KB dictionary — and the
sidecar is fetched `Class::Pinned` and **whole, on every text query**. D-13 says "index
section (cached, **tiny**)", and tiny is the word this design lost.

⚠️ An existing test was already pinning exactly this property —
`the_term_dictionary_scales_with_terms_not_documents` — and it went red. **It is right and the
spec was wrong**; weakening it to land this would have been the "never weaken a test to make a
check pass" non-negotiable, violated to buy a feature whose benefit two review rounds could not
establish.

**So M5d is not implemented, and it is not ready to be.** What it needs first is a place for
per-posting metadata that is *not* fetched whole on every query — a separate object, or a
section fetched by range against the blocks a query actually wants. That is a layout decision,
it interacts with M5e's unanswered questions, and inventing it here would be the third
unreviewed design in a row.

The revert is deliberate: **no segment is worse off**, because the metadata that was never
written is metadata a compaction pass adds later, and the same argument that made writing it
urgent also makes writing the *wrong* one costly — it would be in every segment from now on.

## ⚠️ This milestone writes the metadata and prunes nothing. That is the split, and why

The first two drafts specified the pruning too, and **spec review found a blocking defect in
each**. They are recorded here because the next spec has to start from them, not rediscover
them:

1. **Upper bounds alone cannot prune before a fetch.** MaxScore compares a block's upper bound
   against **θ**, the k-th best score so far — and θ comes from scoring documents, which here
   costs a round trip. Decide everything before the one fetch and θ is **zero**; nothing is
   below zero; nothing is prunable.
2. **A lower bound fixes that for one term and breaks on two.** The second draft stored a
   lower bound so a full block of `BLOCK` postings would witness θ pre-fetch. But
   `TextIndex::search` is **disjunctive** — it sums each term's contribution over the union of
   rows — so a block's upper bound bounds *one term's addend*, never a document's score. Skip
   it and a document that reaches the top-k through another term silently loses that addend.
   The sound condition is `upper_t(β) + Σ_{i≠t} U_i^max < θ`, and once that is written down
   the gain mostly evaporates: the witness term's own `U^max ≥ θ`, so **no block of any other
   term is ever prunable**, and the long low-idf lists — where the bytes are — are exactly the
   ones that can never be skipped.

⚠️ **So the pruning is not deferred out of caution; it is deferred because nobody has shown it
works in this shape.** Pre-fetch-only decisions plus exact-score preservation is a tight box,
and the layout that escapes it is impact-ordered postings — the other half of OQ-45, which
this milestone explicitly does not build.

**What is not deferred is the part that gets more expensive every week.** Segments are
immutable: metadata not written today is metadata a compaction pass has to add later, over a
corpus that only grows. M5d writes it. **M5e**, unwritten, spends it — and cannot be specified
honestly until someone shows a byte reduction on a corpus nobody arranged for it.

## The numbers this milestone is pinned to

| Name | Value | Why this value |
|---|---|---|
| `BLOCK` | **128** postings | Tantivy's skip threshold, and the corpus cites its format as the reference. Below it, skip metadata costs more than it saves — which is the long tail of rare terms, where a whole list is smaller than one range request. |
| Metadata per block | **24 bytes**: `first_row`, `byte_offset`, `max_tf`, `min_fieldnorm`, `min_tf`, `max_fieldnorm` (u32 each) | 24 bytes per 128 postings — 3/16 of a byte per posting. Scores are **not** stored, and the *lower* pair is what makes pre-fetch pruning possible at all. |
| Pruning applies when | **`top_k <= BLOCK`** | Above it one full block no longer witnesses `k` documents, so the pre-fetch θ is not a bound on the k-th best. Checked, and pruning disabled rather than approximated. |
| Where it lives | The **term-dictionary sidecar** | ⚠️ D-13 says "index section"; the sidecar is a separate object, for C-10's reason — a 30,000-term dictionary is 44× `INDEX_BUDGET` and `try_finish` refuses an over-wide segment. The sidecar keeps D-13's *property* (cached, fetched beside the footer, so pruning costs no round trip) at a place the budget allows. Recorded as **C-15**, not assumed. |
| Dictionary version | **2**, and **1 still reads** | key-layout's versioning rule: "Readers must handle N and N−1." ⚠️ The current reader does not — `text.rs:209` refuses any version but its own — so every segment already written would become unreadable, not merely unpruned. Fixing that is in scope. |

## Delta

**Adds**
- `pstore-format::text`: dictionary **version 2**, carrying a per-term block table for terms
  whose `df >= BLOCK`. Terms below the threshold carry none and are scored whole.
- `BlockMeta { first_row, byte_offset, max_tf, min_fieldnorm, min_tf, max_fieldnorm }`, and
  `BlockMeta::upper(p, idf)` / `BlockMeta::lower(p, idf)` where `p` carries `(k1, b, avg_len)` —
  computed at query time, never stored. ⚠️ `K1` and `B` are `pub const` in
  `pstore-index::text` today; the bound takes them as parameters so the property test can
  sweep BM25's domain rather than one point in it.
- Posting lists for those terms are written **in blocks of `BLOCK`**, so a block is a
  fetchable byte range. Below the threshold the encoding is unchanged.
- ⚠️ **A matching reader change, which is not optional.** `sparse::write_list` writes every row
  delta and *then* every impact, and `read_list(raw, count, …)` reads `count` varints from the
  front. A v2 list is a sequence of per-block runs, so decoding one with the term's *total*
  count reads block 1's impacts as row deltas — and returns a `Vec` rather than an error, which
  is silently wrong rows. `decode_list` learns the block layout, and criterion 9 pins that a v2
  list decoded in full equals what v1 produced for the same postings.
- `pstore-index::text`: a block-level **MaxScore** loop that decides which blocks can enter the
  top-k *before* issuing the fetch, and fetches only those ranges.

**Changes**
- **C-15**, on [`full-text-search.md`](../../research/06-indexing/full-text-search.md): D-13
  puts block-max metadata in the **index section**, and it goes in the dictionary **sidecar**
  instead, for C-10's reason. D-13's property — cached, tiny, fetched in a round that was
  happening anyway — survives; its location does not. The same banner records that C-11's
  "new section id plus a compaction pass over the whole corpus" is **cheaper than stated**: a
  v1 segment stays readable and correct, so nothing has to be rewritten on a schedule.
- ⚠️ **The dictionary reader accepts version 1 and version 2.** A v1 dictionary has no block
  table, and a segment with no block table is scored exactly as it is today — **unpruned, and
  correct**. That is what makes this landable against an existing corpus without a migration:
  old segments keep working, compaction upgrades them when it rewrites them anyway, and
  nothing has to be rewritten on a schedule.

**Does not add** — **impact-ordered postings** (the other half of OQ-45): doc-ordered plus
block-max is Block-Max WAND's shape and is what D-13 describes; impact ordering is a different
layout with a different intersection algorithm, and choosing between them needs the eval set
M5's MS MARCO exit is blocked on. **Block-Max WAND itself** — MaxScore is the simpler
pruning rule, and with the two- and three-term queries this system serves it prunes the same
blocks; BMW's pivot selection earns its complexity at higher term counts. **A compaction pass
over the corpus** — old segments are readable and correct; upgrading them is compaction's
existing job. **Positions, phrase queries, trigram** — still M5c's list, still not this.

## Acceptance criteria

1. `upper` and `lower` **bracket every document in the block**, over 10,000 random blocks and
   random parameters drawn from **BM25's domain — `k1 >= 0`, `b` in `[0, 1]`, `avg_len > 0`,
   `idf >= 0`**. ⚠️ `idf` is in the list because it is a free parameter of the function the
   test sweeps: at `idf < 0` the `(max_tf, min_fieldnorm)` corner becomes the *minimum* and the
   bracket inverts. The scorer's Lucene-form idf is never negative, which makes this a
   statement about the domain rather than about the code. At `b > 1` the length norm goes
   negative, the score stops being monotone in `tf` and is unbounded near a pole; a property
   test drawing `b` uniformly would fail, and the tempting fix is to weaken the bound rather
   than the domain.
2. **Search results are unchanged by the format**: the existing text-query suite and
   `scripts/ndcg.sh` are green, and a v2 segment returns the same rows and scores as the v1
   segment built from the same documents. Nothing prunes, so nothing may move.
3. Sequential depth and bytes fetched are **unchanged** — the block table rides in the sidecar
   that was already being fetched, and no query reads it yet.
4. A dictionary written at **version 1** still reads, and its segment scores correctly and
   unpruned. ⚠️ Today it does not read at all.
5. A term with `df < BLOCK` carries **no** block table, and **its posting-list bytes** are
   identical to what version 1 wrote for it. (Its offset within `TextPostings` may move, and
   its dictionary entry gains a block-table pointer; the claim is about the list.)
6. The sidecar's growth is **bounded and measured**: at gate scale (20,000 rows) the block
   table adds under **25%** to the sidecar's bytes, reported as bytes per 1,000 rows. ⚠️ Not
   `INDEX_BUDGET` — that bounds the segment's index *section*, and the sidecar is a separate
   object precisely because C-10 found a real dictionary is 44× it. The sidecar is fetched
   `Pinned` and whole, so its size is a per-query cost and grows with **postings**, not with
   vocabulary.
7. A v2 posting list decoded **in full** equals what v1 produced for the same postings — rows
   and impacts, exactly. The pruned path is not the only reader of these bytes.
8. Region coverage ≥95% on the changed crates, mutation ≥80% on the changed modules, and the
   full gate set green.

## Test plan

| # | Test | The mutation it kills |
|---|---|---|
| 1 | `the_block_bounds_bracket_every_document_in_it` | `min_fieldnorm` recorded as the max, or `max_tf` as the min — either makes the upper bound too low and the pruning unsound; the same swap on the lower pair makes θ too high, which prunes blocks that mattered |
| 2 | `a_v2_segment_scores_like_its_v1_twin` | any off-by-one in block boundaries — which changes the postings and therefore the scores, silently, while every bound test still passes |
| 3 | `the_block_table_costs_no_round_trip` | the block table written as its own object instead of into the sidecar |
| 4 | `a_version_1_dictionary_still_reads` | the version check left as `!= VERSION` |
| 5 | `a_short_lists_posting_bytes_are_unchanged` | block metadata written for every term, spending sidecar bytes on the long tail it is supposed to skip |
| 6 | `the_block_table_costs_under_a_quarter_of_the_sidecar` | 24 bytes written per *posting* rather than per block, or a block table emitted for the long tail it is supposed to skip |
| 7 | `a_v2_list_decodes_to_what_v1_encoded` | `decode_list` given the total count on a blocked list: block 1's impacts read as row deltas, silently, with no error |

⚠️ Criterion 2 is the one carrying the weight now. This milestone changes the bytes of every
common term's posting list while claiming every score stays identical — so the failure mode is
a silent re-encoding, not a wrong bound, and the bound tests cannot see it.

## RA budget

*t* is the query's term count. ⚠️ **Nothing here changes the request count**, and that is the
point of the split: M5e will have to argue a request bound that does not scale with blocks —
a 20,000-row term is 157 of them — and it does not get to inherit this row.

| Operation | W | Rseq depth | Rpar | List |
|---|---|---|---|---|
| Text query, cold | 0 | **3, unchanged** | **1 + 2 + *t*, unchanged** — nothing reads the block table yet | 0 |
| Text query, warm | 0 | **1, unchanged** | ***t*, unchanged** | 0 |
| Sealing a segment | **unchanged** | unchanged | unchanged | 0 |
| Reading a v1 segment | 0 | unchanged | 1 + 2 + *t* | 0 |

## Risks

- **The bound is only as safe as its monotonicity argument, and that argument has a domain.**
  BM25's tf-part rises with `tf` and falls with `fieldnorm` **for `k1 >= 0` and `b` in
  `[0, 1]`**, where the length norm is positive. Outside it the function is not monotone and
  the metadata bounds nothing. If a scorer is ever added that is not monotone in both — a
  proximity or phrase score — this metadata does not bound it either. Criterion 1 tests the
  bound against the scorer that exists; it cannot test one that does not.
- **Metadata written and never read is a cost with no benefit until M5e lands**, and M5e may
  conclude the shape does not work. That is a real risk and it is taken deliberately: the
  alternative is writing more unprunable segments while the question is settled, and the
  metadata is 3/16 of a byte per posting either way.
- **Nothing exercises the mixed-version path in production**, only criterion 4's test. It is
  the first mixed-version read in this project, which key-layout says is the normal state.
- **The eval set is still missing.** `scripts/ndcg.sh` guards ranking quality against a
  generated judged set, and criterion 2 makes pruning answer-preserving, so quality cannot move
  — but the *value* of pruning at real term distributions is not measured here, and MS MARCO
  is blocked on data (M5's exit).
- **Blocking changes the posting bytes for common terms**, so a v2 segment is not byte-comparable
  to a v1 one. Criterion 6 pins that the *short* lists are unchanged, which is where the
  vocabulary lives; the long lists are meant to change.
- **Two formats now exist in the corpus at once**, which is the state key-layout says is normal
  and nothing had exercised. Criterion 5 is the first test in this project of a mixed-version
  read.

## Tasks

| Id | Commit |
|---|---|
| **M5d.1** | The version rule the reader does not follow: v1 dictionaries read, and a mixed corpus scores |
| **M5d.2** | Blocked postings and the v2 block table, measured against the sidecar it grows |
| **M5d.3** | `upper` and `lower`, and the property test that brackets every document on BM25's domain |

## What M5e must answer before it can be specified

Not a task list — the questions the two blocking findings leave open.

1. **What is the sound skip condition, written in full?** `upper_t(β) + Σ_{i≠t} U_i^max < θ`
   is the shape; what supplies `U_i^max` for a term with `df < BLOCK`, which carries no block
   table? (`idf·(k1+1)` is available and loose.) And it rests on every other term's
   contribution being non-negative, which holds for this scorer's Lucene-form idf and is
   load-bearing enough to state.
2. **Which blocks can actually be pruned?** With θ witnessed by term A's block, `U_A^max ≥ θ`,
   so no block of any other term qualifies. If only the witness term's own weak blocks are
   prunable, the byte saving needs measuring before it is promised.
3. **On what corpus?** A block's `lower` is a min over 128 doc-ordered postings and its `upper`
   a max over the same 128; on an i.i.d. corpus every block spans nearly the whole contribution
   range and no block's max falls below another's min. A fixture built to make pruning fire is
   the failure this project's own spec preamble warns about.
4. **What bounds the request count?** Surviving blocks have gaps punched in them by definition,
   so "the coalescer merges them" is not an argument — `coalesce.rs` fetches the whole merged
   span *including* the skipped bytes, which trades the byte saving away. One range per term,
   first survivor to last, is the structural bound worth considering.
5. **Or is the answer impact-ordered postings?** The other half of OQ-45. Pre-fetch-only
   decisions plus exact-score preservation is a tight box, and impact ordering is the layout
   that escapes it.
