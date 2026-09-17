# M7e — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

1. **Reconstruction is exact, at every epoch** — `every_epoch_reconstructs_exactly`
   (`cargo test -p pstore-engine --test time_travel`): a HEAD captured after every fold and
   after a compaction, each compared against `as_of` of that epoch rebuilt from the **final**
   HEAD, segment-key set by segment-key set, for two indexes. The history carries the three
   things spec review said a lazy fixture would miss — a compaction, fold-buried **WAL
   bundles** whose sequence numbers sit exactly where a loose parser reads an epoch, and
   **unfolded rows still in the memtable** when the comparison is made.
   ⚠️ Observed red twice, both times on the fixture rather than the code: pairing a captured
   HEAD with a separately-read `Engine::epoch()` lies about its own history, because `compact`
   advances the committed epoch without going through `fold`.
2. **A past query sees the past** — `a_query_as_of_sees_what_that_epoch_saw`: `["old"]` at the
   old epoch against `["new", "old", "unfolded"]` now, with the unfolded row present at assert
   time and absent from the past answer.
   ⚠️ **This test found a real defect in the implementation**: `unfolded_at` was set to `0`,
   and `0` is a real segment ordinal — so every hit in the first segment resolved against the
   (empty) unfolded rows and vanished. It is `refs.len()` now, which is what the live path has
   always used and for the same reason.
3. **One HEAD read, zero LISTs** — `time_travel_costs_one_head_read_and_no_lists`, asserted at
   **exactly one** through a decorator that counts reads of the HEAD key specifically.
   ⚠️ **Code review found the first version vacuous and proved it**: `OpClass::Read` counts
   every GET — segment opens, sidecars, blocks — so the `> 0` it asserted was satisfied by any
   query at all, and two extra `head::read` calls inserted into `query_as_of` left the suite
   green. A number worth asserting needs a meter that can see it; observed red against that
   same mutation now.
4. **The bound, and its boundary** — `an_epoch_below_the_horizon_is_refused` (strictly below is
   `Reaped`, **the horizon itself is served**, because a key buried *at* epoch `D` is already
   absent from the manifest at `D`), `an_epoch_in_the_future_is_refused`, and
   `the_reap_marker_never_moves_backwards`. ⚠️ The last is asserted **on `Head::record_reap`
   and deliberately not through `gc`**: a `gc` pass with nothing due returns before it commits,
   so the sequence that would move the marker backwards cannot occur — and a test through `gc`
   would have passed without ever running the line it names. Spec review round 2 found that
   vacuity before it was written.
5. **`gc` records what it reaped, and an older HEAD still decodes** —
   `a_head_without_a_reaped_marker_decodes_as_zero` (absence is zero; a tail cut one to seven
   bytes short is `CorruptHead`, asserted for every one of those seven cuts), and
   `a_truncated_head_is_refused_at_every_length_but_a_section_boundary`, which enumerates
   **every** truncation and requires exactly **three** to decode — one per optional section. A
   fourth means a section stopped being checked.
6. **Through the API** — `the_api_answers_as_of_and_refuses_past_the_horizon`
   (`cargo test -p pstore-server --test time_travel`): `{"as_of": N}` returns the one document
   that existed then against two now, `meta.epoch` reports **N** rather than the present, an
   impossible epoch is `400 time_travel_horizon` naming the number asked for, and an index that
   did not exist then is `404`.
   ⚠️ **The horizon arm is now exercised end to end, and getting there added a route.** Code
   review pointed out the fixture had no reap in it, so the half that actually costs a caller
   their history was untested — and it could not be tested, because the server had a fold
   endpoint and **no gc endpoint**. Nothing in it could move the horizon: the graveyard would
   grow without bound and `reaped_before` would stay zero forever. `POST /v1/admin/gc` is the
   amendment, the spec says so, and the refusal now names **both** numbers.
7. **Coverage and mutation** — `./scripts/coverage.sh --fail-under-regions 95` passes at
   **95.07%** regions, 96.85% functions, 96.98% lines; it read **94.99%** first, and the gap
   was `query_as_of`'s two uncovered arms — an index that did not exist at that epoch, and a
   wrong-dimension query — both of which are behaviour worth a test rather than lines worth
   touching: `an_index_that_did_not_exist_then_answers_nothing` and
   `a_past_query_refuses_a_wrong_dimension_like_a_present_one`. Mutation:
   `scripts/mutants.sh --file crates/pstore-engine/src/head.rs` — **44 of 44 viable mutants
   caught**, 2 unviable, no survivors.

## The invariant this milestone had to create before it could rely on it

⚠️ **A compaction that lost a CAS wrote a key stamped with the wrong epoch.** `compact` derived
its output key once, before the commit loop, and deliberately never re-derived it — so a
compaction that lost to a `gc` or to a fold on another index wrote `N+1` and committed at
`N+2`. Reconstructing `N+1` then returned the merged segment **and** both its inputs: every
merged row twice. Spec review found it by reading; `a_contended_compaction_still_reconstructs`
pins it, and the fixture had to be built twice:

- interfering *before* `compact` proves nothing — the compaction reads the newer HEAD and
  derives the right key first time. The interference now runs **between the seal and the first
  commit attempt**, which is the window that existed.
- asserting only at the epoch before the compaction started **passes with the defect present**,
  measured. The defect shows at the epoch the stale key is stamped with, so the fixture
  captures the interloper's HEAD at exactly that epoch and compares against it.

With both corrections the test fails against the old behaviour and passes against the new.

⚠️ **The stale key is buried under its own key epoch, not the committing one** — spec review
round 2 caught that burying it at the commit epoch puts it straight back into the arm the fix
removed it from. The graveyard means "was live, and stopped being referenced here"; an object
that was never live has no such epoch.

## Code review round 2 found one more, in the delta that answered round 1

⚠️ **A reap reported the wrong epoch, and so did every query after it.** `Engine::epoch()` reads
a cached number that only `fold` wrote, so `gc` — which commits a new HEAD like any other
writer — left it a full epoch behind; measured through the API, a reap that committed epoch 6
answered `"epoch": 5`, and the next query said 5 too. `compact` had the same gap. Both now go
through one `record_commit`, and `every_commit_advances_the_epoch_this_engine_reports` pins it
for both, observed red against the missing call. ⚠️ In a milestone whose subject is telling a
caller **where its history stands**, this was the wrong number in the field that matters.

⚠️ **One minor left unfixed and recorded**: the compaction discard path now leaks one orphaned
object per retry rather than one, because a retry seals at a new key and a discard buries
nothing. It is M6e's orphan sweeper's job and always was; what changed is the size, not the
kind.

## Stated non-properties

- **`as_of` is not snapshot isolation.** It answers "what did this index look like at epoch E".
  It pins nothing: a concurrent `gc` can move the horizon under a reader, who is then refused.
- **A reconstructed HEAD is exact in `indexes` and nothing else.** Resurrected refs carry
  `rows: 0` — the graveyard records keys, not counts — and `schemas`, `schema_rejects` and the
  watermarks are the present's. Queries read none of them; it is why `as_of` is query-only.
- **A past query is wider than a present one.** An old manifest names the pre-compaction
  segments, so fan-out grows with the retention window. Depth is unchanged; width is the cost
  of looking backwards.
- ⚠️ **The horizon is advisory for exactly one window.** `gc` deletes before it commits (M6d's
  deliberate order), so a `gc` whose commit then fails has removed objects without recording
  the bound. An `as_of` in that window passes the check and fails loudly at the segment open
  instead.
