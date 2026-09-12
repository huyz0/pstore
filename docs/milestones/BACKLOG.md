# Backlog

⚠️ **The ledgers are the source, this is the index.** Every row below is carried forward from
a `VERIFIED.md`, and each links to the ledger that stated it. Nothing new is invented here; if
a row and its ledger disagree, the ledger wins.

⚠️ **This file is not a plan for the whole system.** It is what previous milestones said they
had not finished. Work not yet specified — M7's remaining three bullets, and anything past
them — is in [`roadmap.md`](../research/11-design/roadmap.md), not here.

## Ordering, and why

Three principles, applied in this order:

1. **A criterion someone already wrote outranks a criterion nobody has written.** M0a's
   criterion 4 says "412, 409, 503 **and latency**" and the ledger says `NOT-RUN` for latency.
   That is a milestone reporting itself incomplete, and closing it costs less than arguing
   about what to do next.
2. **A cost that rises with time outranks one that does not.** Block-max pruning was the only
   item here that got *more* expensive every week: segments are immutable, so every segment
   written before the retrofit is permanently unprunable ([C-11](../research/06-indexing/full-text-search.md)).
   ⚠️ **Overturned by measurement** — [C-15](../research/06-indexing/full-text-search.md): the
   pruning skips zero bytes, so the rising cost was the cost of a *mistake*, and the principle
   ranked first the one item that should never have been built. The principle itself survives;
   what it needed was a number before it was applied, which is exactly what item 6b now has.
3. **A gate that measures less than it claims outranks the code it fails to measure.** If
   `scripts/coverage.sh` cannot see `pstore-engine`, every coverage number quoted about the
   correctness core has been meaningless, including this session's.

## Phase 1 — close what a ledger already calls unfinished

| # | Task | From | Size |
|---|---|---|---|
| ~~1~~ | ~~**M0a.10** — latency injection in `Faults`~~ **DONE.** M0a criterion 4 is now met as written, and the second random stream is what makes "independently" true rather than merely intended. | [M0a](M0a/VERIFIED.md) | S |
| ~~2~~ | ~~**`Engine::gc` chunks its delete batch**~~ **DONE.** Chunked at the capability, not a constant; a cap of zero errors rather than panicking. | [M7a](M7a/VERIFIED.md) | S |
| ~~3~~ | ~~**The roster CAS**~~ **DONE — stays unguarded, and now says why in the code.** The roster only unions, so a CAS that fails to fence delays convergence rather than losing a member; pinned by a test against a store that ignores the precondition. | [M7a](M7a/VERIFIED.md) | S |
| ~~4~~ | ~~**M0a.11 + `pstore-blob` coverage**~~ **DONE.** Coverage 94.31% → **97.99%**, mutation 76.8% → **95.8%** (206/215 viable). The coverage gap turned out to be the class-forwarding methods, whose own comments say dropping the class silently disables D-21 — and nothing tested it. | [M0a](M0a/VERIFIED.md), [M7a](M7a/VERIFIED.md) | M |

## Phase 2 — the gate that cannot see the correctness core

| # | Task | From | Size |
|---|---|---|---|
| ~~5~~ | ~~**`scripts/coverage.sh` excludes `pstore-engine`**~~ **DONE.** A crate declares `[package.metadata.pstore] ships = false` instead of the gate inferring it, so the default is *measured* and opting out shows up in a diff. The gate still passes with the correctness core in scope: **95.41%**. | [M7a](M7a/VERIFIED.md) | M |

## Phase 3 — ~~the format decision that gets dearer every week~~, closed by measuring it

⚠️ **This phase's premise was wrong, and the measurement is in item 6b.** Block-max metadata
was urgent because segments are immutable, so every week's segments were permanently
unprunable. Measured, the pruning it enables skips **zero bytes** at every query width and
every `top_k` — so what was rising with time was the cost of writing the wrong metadata into
every segment forever, not the cost of waiting.

| # | Task | From | Size |
|---|---|---|---|
| ~~6~~ | **[M5d](M5d/SPEC.md) — the block-max *format*. CLOSED as not-to-be-built.** Specced, built, **reverted, amended twice**. ⚠️ Measured at gate scale, the block table grows the dictionary sidecar from 28,030 to 616,994 bytes — **22×** — and that object is fetched `Pinned` and whole on every text query. A dictionary scales with terms; a block table scales with postings. The existing test `the_term_dictionary_scales_with_terms_not_documents` caught it and is right. ⚠️ The layout question it was blocked on turned out not to matter: item 6b's measurement shows the pruning that metadata exists for skips **zero bytes** in this system's disjunctive scorer, at every width and every `top_k`. The metadata would have gone into every segment forever to enable a skip that never fires — so "every segment written before this lands is permanently unprunable", which was the entire urgency argument and this file's ordering principle 2, was **the cost of a mistake rather than of a delay.** The revert avoided it. | [M5c](M5c/VERIFIED.md), C-11, [C-15](../research/06-indexing/full-text-search.md) | M |
| ~~6b~~ | **M5e — the pruning. CLOSED by measurement, and it was never written.** ⚠️ Two spec-review rounds found a blocking defect in each draft of the pruning half: an upper bound alone cannot prune before a fetch (θ = 0), and a lower bound fixes that for one term but is **unsound for disjunctive multi-term queries**, where a block bounds one term's addend and not a document's score. Corrected, the gain mostly evaporates — the witness term's own maximum exceeds θ, so no other term's blocks are prunable. ⚠️ Questions 2, 3 and 5 are now **answered with a number**: the sound condition skips **0 bytes in all 18 measured configurations** — widths 1/2/3 × `top_k` 1/10/100 × mixed and all-tabled. At one term the constraint is θ (pre-fetch 3.997 against a true 5.028, and that 20% gap costs the whole 62.78% an oracle would reach); at two or more it is `Σ_{i≠t} U_i^max`, which is **10.3** and **31.7** against a true θ of 8.06 and 9.39 — the other terms' bounds alone exceed the k-th best score, so the oracle collapses too and no better witness helps. It gets worse as terms are added. Recorded as [C-15](../research/06-indexing/full-text-search.md); harness in [`blockmax.rs`](../../crates/pstore-index/examples/blockmax.rs). What replaces it is OQ-45's other half, impact-ordered postings, which needs the eval set M5's MS MARCO exit is blocked on. | [M5d](M5d/SPEC.md) | L |

## Phase 4 — the handoffs named by three milestones each

| # | Task | From | Size |
|---|---|---|---|
| ~~7~~ | **A stable cross-segment identity — [M5f](M5f/SPEC.md), DONE** ([VERIFIED](M5f/VERIFIED.md)). The row read as one thing and was two: fusion needs a **query-lifetime** identity, and nothing in the tree needs a durable one — there are no deletes or updates by id, and `Engine::search` already returns `Document.id`. So `Hit` carries `(segment, row)`, `fuse` keys on the pair, and `query` takes N `Target`s: one open round, `Stats::merge` before any leg runs, one fusion over the union. ⚠️ The defect was live: `fuse` keyed on `row` alone, so two segments' row 5 summed into one hit — two unrelated documents merged and ranked above either. ⚠️ **Two of the four load-bearing mutations survived their first fixture** (per-segment statistics, and the union re-sort), which is recorded in the ledger rather than fixed quietly. Final sweep **16 of 16 viable mutants caught**, run on the third fixture rather than the first. | [M3b](M3b/VERIFIED.md), [M5a](M5a/VERIFIED.md), [M5b](M5b/VERIFIED.md), [M5c](M5c/VERIFIED.md) | L |
| ~~12~~ | **An ANN index at fold time — [M5g](M5g/SPEC.md), DONE** ([VERIFIED](M5g/VERIFIED.md)).** ⚠️ Found by M5f, and it is why `Engine::query` does not exist: `Engine::seal` writes no centroid table, so a folded segment carries **no dense index at all** and `Engine::search` is exact brute force over a full `scan` — every document of every segment, on every query. `vec_index::build_all` is the builder; running it at fold time is the milestone. Now built: `seal` goes through `try_build_all`, writes the centroid table at a derived `.cen` key, and `Engine::query` reads HEAD and fuses across every segment. ⚠️ Three findings the spec did not have — boundary replication writes a row **twice** (400 documents merged back as **431**, compounding every merge), so `seal` clamps `replicas: 0` and loses r@10 p=2 **0.961 → 0.844**; the layering comment saying `pstore-index` may depend downward on `pstore-engine` is overturned, because a fold that builds an index forces engine → index either way; and a **wrong centroid key changes no result at all**, since a missing table is D-10's "scan me exactly", so it is pinned on bytes instead. | [M5f](M5f/VERIFIED.md) | L |
| ~~8~~ | **A schema in HEAD.** ⚠️ **Split, and the silent half is [M6c](M6c/SPEC.md) — DONE** ([VERIFIED](M6c/VERIFIED.md)). The task read as one thing and was two. `DEFAULT_FIELD` was already replaced for vectors by M3b's `Fields` table; `DEFAULT_TEXT_FIELD` was not, so a segment did not record which attribute its text index was built over — a corpus with prose in `body` folded to **no postings, no error**, and a compaction through a default handle would have **destroyed** an index that was there, all rows intact. `Section::TextFields = 14` records it, the query asks the segment instead of a constant, and `compact` takes the name from its inputs. What is left is the tenant-facing half: who sets the field, and what happens to segments written under the old setting. That needs the server, so it is not effort — it is blocked, and moved to the table below. | [M5c](M5c/VERIFIED.md), [M6a](M6a/VERIFIED.md) | M |

## Phase 5 — the milestone that was split off and never written

| # | Task | From | Size |
|---|---|---|---|
| ~~9~~ | **[M6b](M6b/SPEC.md) — quotas and metering. DONE** ([VERIFIED](M6b/VERIFIED.md)): `pstore-meter`, 96.59% regions and 100% functions, **77 of 77 viable mutants caught**. Specced over two review rounds. ⚠️ Three blocking findings in round one, all about shape rather than absence: a reservation counting ranges where `Accounted` counts *coalesced fetches*; a byte quota that cannot be reserved before a transfer whose size is unknowable without a forbidden `head`; and `Usage` in `TenantRecord`, which is unsound in both directions and is now out of scope. The billing rollup and the 1M workload are named as not-this. ⚠️ Code review then **measured** two more: it billed the slices handed back rather than the merged buffers (64 bytes against 456, a 7× undercharge), and reserved requests before checking byte credit. Both survived a 73-of-73 sweep, because the agreement test ran at a gap where the two byte figures are equal by construction. | [M6a](M6a/VERIFIED.md) | L |
| ~~11~~ | **Three mutation survivors that predate M6c — DONE, and it turned out to be five.** ⚠️ `writer.rs:214` — `*s == Section::SparsePostings && !b.is_empty()` survives both `== -> !=` and `&& -> ||`, from [M3b](M3b/VERIFIED.md) (`3d7cd14`). That is the **same** guard shape M6c's `has_text` had, and the same weakness: with only one section in `extra`, the mutants are indistinguishable from the original, so the fixture needs a second, non-sparse section. `a_name_without_postings_writes_no_section` is the pattern to copy. ⚠️ `Engine<S>::compact:811` — the retry guard `attempt < MAX_COMMIT_ATTEMPTS - 1` survives four mutations including `-> true`, from [M2.8](M2/VERIFIED.md) (`76091fb`): nothing drives a compaction to its retry ceiling, so the loop's bound is untested. ⚠️ `INDEX_BUDGET`'s `-` survives, which would silently enlarge the budget the whole depth argument rests on. **Closed by `crates/pstore-format/tests/guards.rs` and `crates/pstore-engine/tests/retry_ceiling.rs`.** ⚠️ **The retry guard is in three functions, not one** — `fold`, `gc` and `compact` carry it identically, and the sweep only examined `compact` because that is where M6c's regex happened to look. Each of the three was observed to survive `-> true` independently, so covering only the measured one would have fixed the site and left the shape. The property they now pin is that a loop out of attempts reports **`Contended`, not `Lost`** *and that it retried at all*: `Lost` tells the caller to rebase against a HEAD that never moved. ⚠️ **The confirming sweep found the first fix half-done**, which is the value of re-sweeping rather than declaring: asserting the error *kind* left `gc`'s guard surviving `-> false`, `< -> ==` and `< -> >`, because a loop that gives up on the first contention returns the same `Contended` the test was checking for. The fixture counts refusals now. ⚠️ One survivor at `fold`'s watermark guard is **provably equivalent** — both read sites treat an absent watermark as zero, so writing `lane -> 0` is indistinguishable from writing nothing; it is a HEAD-size guard, and the reasoning is in a comment on the line so the next sweep does not chase it. | sweeps run for [M6c](M6c/VERIFIED.md) | S |
| ~~10a~~ | **Run reaping — [M6d](M6d/SPEC.md), DONE** ([VERIFIED](M6d/VERIFIED.md)). A bounded graveyard of `(run_epoch, digest)` in the bucket head, because a run's key is **derived** and the head carries only the digest of the run it names — garbage nobody can name is garbage forever. `reap(store, bucket, retention)` deletes before it commits, refuses a window wider than the record, and is a guarded door. ⚠️ Reaping too early is **loud** (`MissingRun`), which is what makes retention a safe knob. ⚠️ The region floor forced out a separate finding: `read_root` returned no tag, so `write_root`'s conditional arm had no reachable caller and the root was **write-once** — the width could be set and never changed. Final sweep **37 of 37 viable mutants caught**; the first found two real defects in `reap`, both recorded in the ledger. | [M6a](M6a/VERIFIED.md) | M |
| ~~10b~~ | **Bucket splitting (OQ-8) — [M6g](M6g/SPEC.md), DONE** ([VERIFIED](M6g/VERIFIED.md)).** It needs a protocol keeping an old-width reader **stale rather than wrong**, and `{bucket:04x}` caps at 65,536 buckets, so it is a key-format change too. ⚠️ M6d removed the write-once blocker underneath it; the protocol question is untouched. ⚠️ **Doubling is the only split**, and that is what made it tractable: `h % 2w` is either `h % w` or `h % w + w`, so bucket `b` splits into exactly `b` and `b + w` and no record moves between two old buckets. The new buckets are written before the root flips and the old ones are left alone — so an old-width reader stays **complete**, and it is *pruning*, which is not built, that would make it wrong. ⚠️ The key format is not the blocker it looked like: `{bucket:04x}` allows exactly two doublings from the default, and `Width::new` already refuses the fifth digit. ⚠️ It also found a defect: `enumerate` merged per bucket and concatenated, so a tenant in two buckets was counted **twice**. | [M6a](M6a/VERIFIED.md), [M6d](M6d/VERIFIED.md) | L |
| ~~13~~ | **Reaping an orphan run — [M6e](M6e/SPEC.md), DONE** ([VERIFIED](M6e/VERIFIED.md)).** ⚠️ Named by M6d as the half a graveyard cannot reach: a fold that writes its run and loses the head CAS leaves an object **no head and no graveyard names**, and its key cannot be derived from anything that survives. Reachable only by LIST — which the corpus forbids on read, write and startup paths, and a reaper is none of those, so a LIST-based sweeper is *permitted* and needs a cost model. Carrying the orphan into the retry's graveyard only helps when the writer survives to retry; a tombstone needs its own reaper. ⚠️ The epoch in the key settles it with **no clock**: a run in flight was written against the head its writer read, so its epoch is strictly *greater* than the head's, and only what the head has moved past is garbage. `sweep` is one LIST and one batched delete **per bucket** — 16,384 for a deployment-wide pass at the default width, and never per tenant. It records nothing, because an orphan is defined by absence from the head. ⚠️ The default is **keep what I cannot parse**: the bucket's own `HEAD` shares the prefix, and the opposite default deletes the pointer. | [M6d](M6d/VERIFIED.md) | M |
| ~~16~~ | **The mutex poison branches in the appender — [M6f](M6f/SPEC.md), DONE** ([VERIFIED](M6f/VERIFIED.md)).** ⚠️ Found by M6e's coverage criterion, which measured `pstore-catalog` at **94.90%** against the 95% it asked for. `append.rs` is 88.75%, and the gap is the lock-poison arms that degrade to "record unconditionally" rather than panicking — reachable only by a thread that panics while holding a private lock. ⚠️ **It was not a coverage row.** Reading the branches turned up behaviour: `if let Ok(mut seen)` **skipped the insert** on a poisoned lock, leaving the appender with no memory of that tenant — so the lifecycle-rate append C-12's bounded write depends on became a **commit-rate** one, silently. It was also the only shipping module of seven that dropped the `Result` instead of recovering it. Fixed, and `scripts/check-poison.sh` now refuses the form so the next instance is caught rather than found by a coverage number two milestones later. ⚠️ And the gate that keeps the gate lists honest had a hole of its own: `build-index.py` compared `ci.yml` against AGENTS.md and **nothing against `gates.sh`**. Coverage moved 94.90% → **94.94%**, still short — the residue is `?` arms and the recovery closures, uncovered in all six modules using the idiom, so it is a property of the idiom and not a hole here. | [M6e](M6e/VERIFIED.md) | S |

## Where this stands

⚠️ **Every row the original audit carried forward is closed or explicitly blocked, and so is
every row the work opened along the way.** What is left is the table below.

| # | Found by | Why it is not done |
|---|---|---|
| ~~10b~~ | [M6a](M6a/VERIFIED.md), narrowed by [M6d](M6d/VERIFIED.md) | **Done** — [M6g](M6g/VERIFIED.md). |
| **18** | a sweep run for [M6g](M6g/VERIFIED.md) whose regex over-matched | ⚠️ **`pstore-index/src/lire.rs` has almost no mutation coverage**: 8 missed and 2 timeouts in `split_pass` and `Bounds::with_split_factor`, nearly every arithmetic operator among them. It is explicitly *"a spike for OQ-51, not a shipped path"* and deliberately unwired from `vec_index`, so this is not the correctness core — but a spike whose arithmetic nothing constrains can give a **wrong answer to the open question it exists to settle**, which misinforms a design decision rather than a query. S. |
| **17** | [M6g](M6g/VERIFIED.md) | Pruning a split's old buckets. The only step that makes a reader or a stale-width writer wrong, so it needs a bound on how long such a process may live — which nothing provides. Until then a split doubles the bucket objects and reclaims nothing. |
| ~~14~~ | [M5g](M5g/VERIFIED.md) | **Done** — [M3c](M3c/VERIFIED.md). ⚠️ **Premise corrected by measurement first.** It is **not** a recall loss — that 0.961 → 0.844 compared two different probe widths. Measured on both corpora (`cargo run --release -p pstore-index --example recall -- --replicas`), at the engine's default **p=8 replication buys 0.0000 recall** (0.9810 either way) and *costs* bytes (0.841 vs 0.769 MB). What it actually buys is **query bytes at small p**: 0.9610 @ **0.288 MB** at p=2 against 0.9680 @ 0.429 MB unreplicated at p=4 — ~33% fewer bytes at equal-ish recall, for 1.65× stored codes. `cost-model.md` prices a node on scan bytes and `Params::default()`'s own comment states the principle — *"storage is the cheap resource and query bytes are the scarce one"* — so the layout change was worth building: `Section::IndexRows` splits the code rows from the document rows, and the engine's clamp is now a *measured* `replicas: 0` default rather than a correctness workaround. ⚠️ It also fixed a defect live since M3 — the sidecars were built over the expanded rows, so a replicated document was counted twice in `doc_count` and in every term's `df`. ⚠️ **Nothing is turned on**: the byte saving needs a lower `p`, which is its own decision. |
| ~~15~~ | [M5g](M5g/VERIFIED.md) | **Done** — [M5h](M5h/VERIFIED.md). The memtable became a segment, so there is one BM25, one dense path and one fusion rather than a second scorer for each. |
| ~~16~~ | [M6e](M6e/VERIFIED.md) | **Done** — [M6f](M6f/VERIFIED.md). It was a behaviour bug, not a coverage row. |

⚠️ **Items 12, 13, 15 and 16 were opened and closed inside the same run of work** — the ANN index at
fold time ([M5g](M5g/VERIFIED.md)), the orphan sweeper ([M6e](M6e/VERIFIED.md)), the freshness
layer in the indexed path ([M5h](M5h/VERIFIED.md)) and the poisoned lock
([M6f](M6f/VERIFIED.md)). What each left behind is above, and each is smaller than what it
replaced. ⚠️ **Nothing unblocked is left.** 10b is blocked on a protocol; what remains of 14 is the
`p` decision, which needs a measurement on a corpus that is not this one.

⚠️ **Two items closed by measuring instead of building** — 6 and 6b, where the pruning the
format existed for skips **zero bytes**, and this file's own ordering principle 2 had ranked
them first. A backlog is only as good as its willingness to delete a row.

## Blocked, and by what

⚠️ **Named rather than omitted, and none of these is blocked on effort.**

| Task | Blocked on |
|---|---|
| **M0b** — real-cloud capability profiles, latency, CAS contention, cost | Cloud accounts. Every emulator profile says so in its own header (D-99). |
| **M3's exit** — 90–95% recall@10 on **100M vectors** (measured at 20,000 × 384d) | ~300 GB and a machine that is not WSL2; brute-force ground truth alone exceeds the gate budget. |
| **M5's exit** — MS MARCO evaluation | No network, no dataset. |
| **M1.13 / M4d** — the NVMe tier and `foyer` | A real device. ⚠️ D-23 calls persistence *mandatory*: until it lands a rolling restart flushes every cache, ~10 h to refill per node. The tier could be *built* against a temp directory; the numbers that justify it cannot. |
| **GCS bulk delete** | `object_store` 0.14.1 maps GCS's `delete_stream` to one request per location; the JSON API it uses has no bulk delete. Upstream, not ours. |
| **OQ-153** — `fake-gcs-server`'s `ifGenerationMatch` fidelity | The emulator routes the XML upload path to its JSON handler. Needs a newer emulator, a different client, or the JSON upload path. |
| **M4e / OQ-59** — the balance cost of AZ-aware LRH | Not effort: one sample per configuration measures ring-position luck. Needs an experiment design, which is a task, not a measurement. |
| **The tenant-facing half of the schema** (item 8's remainder) | A server. The format and engine now carry a named text field ([M6c](M6c/VERIFIED.md)); what is missing is *who sets it*, whether it may change on an index that already has segments, and what a fan-out does when a tenant's segments disagree — which M6c refuses per segment rather than resolving. Every one of those is a policy question with no caller to ask. |
