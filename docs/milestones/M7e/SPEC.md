# M7e — Time travel, and what it costs (nothing)

**Serves:** **OQ-82** (*should epochs be public — real time travel via `as_of` — or internal?
Leaning public*), `11-design/api-design.md` §Query (every tradeoff is a client parameter), and
M7's second bullet, of which this is the half that can be built without a new tier.

**Depends on** [M6d](../M6d/SPEC.md) (the graveyard and its retention), [M7c](../M7c/SPEC.md)
(the caller), and the segment key, which has encoded its own epoch since M1.

## ⚠️ The finding this milestone is built on

Time travel usually costs a version store. Here it costs **nothing**, because the two things it
needs are already in HEAD:

- A segment's key **contains the epoch that created it** — `…/idx/{index}/seg/L0/{epoch:020}-{lane:016x}.seg`.
- The graveyard records, **per epoch, which keys stopped being referenced** — that is how GC
  runs with zero LIST (M6d).

So the manifest as of epoch `E` is a **function of the current manifest**: every live **segment
of this index** whose key epoch is `≤ E`, plus every graveyard entry that is a segment key of
this index, buried *after* `E`, whose key epoch is `≤ E`. No archive object, no extra PUT on the
commit path, no read beyond the HEAD a query already fetches.

⚠️ **The graveyard is not segments-only**, which spec review caught: `fold` buries the WAL
bundles it consumed, and a bundle key is `…/wal/{tenant}/{lane}/{seq:016}.bundle` — a
zero-padded **sequence number** sitting exactly where a loose parser would read an epoch. The
filter is therefore on the **`…/idx/{index}/seg/` prefix**, not on "an entry in the graveyard",
and the history a test loops over must contain a fold-buried bundle whose seq is numerically
below the epoch being reconstructed.

⚠️ **The formula is only sound while a segment's key epoch IS the epoch it went live at**, and
spec review found the one place that was false: `compact` derives its output key **once**, from
`at.head.epoch.next()`, and deliberately does not re-derive it on a retry — so a compaction that
loses a CAS (to a `gc`, or to a fold on any other index) writes a key stamped `N+1` and commits
it at `N+2`. Reconstructing `N+1` would then return the merged segment **and** both its inputs:
every merged row twice. **This milestone makes the invariant true rather than working around
it** — see the Delta — because a self-describing key is worth more than one avoided PUT on a
contended path.

## Delta

**Adds**
- `Head::as_of(epoch) -> Result<Head, TimeTravel>` — the reconstruction above, plus the bound.
- `Head.reaped_before: u64` — the epoch below which reconstruction is **unsound**, set by `gc`
  when it deletes. A third optional trailing section, by the mechanism M7d established and
  tested: absence is zero, and every HEAD ever written still decodes.
- `Engine::query_as_of(index, epoch, …)`, which is `query` over a reconstructed manifest.
- `"as_of": <epoch>` on `POST /v1/indexes/{id}/query`; `meta.epoch` already reports what was
  served, so a caller can tell what it got.
- ⚠️ **Amended during implementation: `POST /v1/admin/gc?retention=N`.** The server had a fold
  endpoint and no reap, so nothing in it could ever move the horizon: the graveyard would grow
  without bound and `reaped_before` would stay zero forever. A time-travel milestone whose
  bound no deployment can reach has not built the thing it describes — and the horizon arm of
  criterion 6 could not be tested end to end, which is how this was noticed.

**Changes**
- ⚠️ **`compact` re-derives its output key when a retry moves the epoch** and re-PUTs the bytes
  it already has at the new key. The cost is one extra PUT on a *contended* compaction — the
  rows are not rebuilt, only re-addressed — and what it buys is the invariant the whole
  reconstruction rests on: **a segment's key epoch is the epoch it became live at**.
- ⚠️ **The stale key is buried under ITS OWN key epoch, not the committing one**, and spec
  review round 2 caught the difference: burying `K(N+1)` at `N+2` puts it back into exactly the
  arm the fix removed it from — "buried after `N+1`, key epoch ≤ `N+1`" is true, so `as_of(N+1)`
  returns the merge *and* its inputs again. The graveyard's meaning is "**was live**, and
  stopped being referenced at this epoch"; an object that was never live has no such epoch, so
  it is buried at its own, where "buried after `E`" is false for every `E` that could name it.
  GC reaps it either way, since it prunes `range(..=horizon)`.
- ⚠️ **The discard path buries nothing**, because there is no commit on it: a compaction whose
  inputs vanished returns `Ok(None)` with its sealed object unreferenced, which is M6e's orphan
  sweeper's job and always has been.
- `Engine::gc` records the horizon through `Head::record_reap(horizon)`, which takes the
  **maximum** of what is there and what it is given.
  ⚠️ **The `max` is defensive, and spec review round 2 showed the backwards case is unreachable
  today** — which is worth writing down rather than pretending a test covers it. `horizon` is
  derived from this call's `retention`, so `gc(2)` then `gc(50)` *computes* a lower number; but
  a `gc` with nothing due returns before it commits, and after the first pass there is nothing
  below the marker left to be due. So the write never happens on the path that would move it
  back. The `max` lives in a **function with its own test** rather than inside `gc`, because
  that is the form in which a defensive invariant can be asserted at all.
- ⚠️ **`query_as_of` does not fuse the freshness layer.** `query` adds this process's unfolded
  rows to every answer, and those rows are by definition newer than any past epoch. A test
  whose memtable happens to be empty passes either way, so the history criterion 1 loops over
  must have unfolded rows present at assert time.

**Does not add** — **branching**, which needs a copy-on-write manifest and a name that is not a
tenant's HEAD, and is a milestone; **the warm API**, which needs the NVMe/`foyer` tier that
[D-23](../../research/07-caching/) calls mandatory and M1.13 blocks on a real device — a warm
endpoint over a store with no cache is a lie with a 200; **streaming responses** (OQ-83), which
is protocol work whose value is RAG UX rather than correctness, and which would be the third
subject in a milestone that already has two.

⚠️ **`as_of` is not a snapshot isolation level.** It answers "what did this index look like at
epoch E", where E is a number the writer saw. It does not pin anything: a concurrent `gc` can
move the horizon under a reader, and the reader is then **refused** rather than served a partial
index. That refusal is the feature.

## Acceptance criteria

1. **Reconstruction is exact.** Over a sequence of folds and a compaction, a HEAD captured at
   every epoch is compared against `as_of` of that epoch reconstructed from the final HEAD:
   the **set of segment keys matches exactly**, at every epoch, for every index. Asserted as a
   loop over the history, not on one hand-picked epoch.
2. **A query at an old epoch returns the old answer**: a document written, folded, then deleted
   by a compaction that drops it is **found** by `as_of` at the earlier epoch and **absent**
   from a query at the current one.
3. **Costs nothing extra on the paths that matter**: `query_as_of` reads HEAD **exactly once**
   and issues **zero LISTs** — the reconstruction is arithmetic over the HEAD the query already
   fetched. ⚠️ Stated this way because "the same number of requests as `query`" is vacuous:
   against the same manifest the two cannot differ, and the implementation worth refusing is
   the one that re-reads HEAD or lists for old keys.
4. **The bound is enforced and its boundary is pinned.** After `gc(retention)`, an `as_of`
   **strictly below** `reaped_before` is `TimeTravel::Reaped { asked, horizon }`, and
   `asked == reaped_before` is **served** — a key buried *at* epoch `D` is already absent from
   the manifest *at* `D`, so nothing that manifest names has been deleted.
   ⚠️ An epoch above the current one is refused too, because answering it reports the present
   as the past. ⚠️ And `Head::record_reap` never lowers the marker, asserted **directly** on the
   function: the `gc` sequence that would exercise it cannot occur, so a test through `gc` would
   pass without running the code it names.
5. **`gc` records what it reaped**, and an older HEAD decodes with `reaped_before: 0` — the
   third optional section, on M7d's mechanism, with the same byte-level test: exactly the
   **three** section boundaries decode and no fourth.
6. Through the API: `{"as_of": N}` answers at epoch N with `meta.epoch == N`; **below the
   horizon** — reached by `POST /v1/admin/gc`, which this milestone adds — it is
   `400 time_travel_horizon` naming **both** numbers, so a caller learns the bound rather than
   guessing it; above the current epoch it is `400`; and an index that did not exist then is
   `404`, by the same predicate a live query uses.
7. Region coverage ≥95% on the changed crates; every mutant in the new code caught or named
   equivalent with the reason on the line.

## Test plan

| # | Test | The mutation it kills |
|---|---|---|
| 1 | `every_epoch_reconstructs_exactly` — over a history containing a **contended compaction**, a fold-buried **bundle**, and **unfolded rows present at assert time** | `<=` for `<` on the key epoch, which shifts the whole history by one fold; the graveyard arm dropped, which silently returns the *current* manifest for every past epoch; a bundle key parsed as a segment; a compaction output whose key epoch is not its commit epoch, which returns the merge AND its inputs |
| 2 | `a_query_as_of_sees_what_that_epoch_saw`, `time_travel_does_not_fuse_the_memtable` | reconstruction that keeps segments created *after* the epoch, which is the present wearing a date; the freshness layer fused into a past answer, which is the present wearing a date one layer down |
| 3 | `time_travel_costs_no_extra_requests` | an implementation that re-reads HEAD, or one that lists to find old segments — the obvious way to do this, and the one the architecture forbids |
| 4 | `an_epoch_below_the_horizon_is_refused`, `an_epoch_in_the_future_is_refused`, `the_reap_marker_never_moves_backwards` | the boundary inverted, which either refuses an epoch that is perfectly reconstructible or serves one whose objects are gone; a missing future check, which answers "as of tomorrow" with today; `record_reap` assigning rather than taking the maximum — killable on the function, and **not** through `gc`, where the sequence cannot arise |
| 5 | `a_head_without_a_reaped_marker_decodes_as_zero`, `a_truncated_head_is_refused_at_every_length_but_a_section_boundary` | absence decoded as an error (every existing HEAD unreadable) or as a *nonzero* horizon (every time-travel query refused) |
| 6 | `the_api_answers_as_of_and_refuses_past_the_horizon` | a `500` for a client-caused refusal; `meta.epoch` reporting the current epoch rather than the one served, which makes a stale answer indistinguishable from a fresh one |

⚠️ **Criterion 1 is the one that makes this honest.** Everything else tests that the feature
answers; only comparing against HEADs captured *at the time* tests that it answers **correctly**.

## RA budget

| Operation | W | Rseq depth | Rpar | List |
|---|---|---|---|---|
| `fold`, `flush`, `write` | **unchanged** — nothing is archived | unchanged | 0 | 0 |
| `query` with `as_of` | 0 | **same depth as a live query** (4, M7c) | ⚠️ **wider**: an old manifest names the **pre-compaction** segments, so fan-out at an old epoch is larger — up to every L0 segment folded inside the retention window. Depth is the budget; width is the cost of looking backwards, and it is stated rather than claimed unchanged | 0 |
| `gc` | unchanged — one more field in a HEAD it already writes | unchanged | 0 | 0 |
| Reconstruction itself | **0** | **0** — arithmetic over the HEAD already in hand | 0 | 0 |

## Risks

- **Retention is the horizon, and it is short.** `gc(retention)` is called by whoever calls it;
  a deployment that reaps aggressively has hours of history, not days. The refusal names both
  numbers so a caller learns the bound rather than guessing it. Making the window a policy is a
  later milestone's, and it belongs with quotas.
- **A reconstructed HEAD is exact in `indexes` and in nothing else.** The graveyard records
  keys, not `SegmentRef.rows`, so a reconstructed ref carries `rows: 0`; `schemas` and
  `schema_rejects` are today's, not that epoch's. Queries read none of them — the segment
  footer has the truth — but it is why `as_of` is **query-only** here and why the stats route
  does not take it.
- ⚠️ **The horizon is advisory at exactly one moment, and the backstop is loud.** `gc` deletes
  **before** it commits (M6d's deliberate order), so a `gc` whose commit then fails has removed
  objects without recording the horizon. An `as_of` in that window passes the bound and fails
  at the segment open with `NotFound` instead. Named because criterion 4 promises "never a
  partial index" and this is the one case where that promise rests on a blob error rather than
  on the bound.
- **An index that did not exist at `E`** reconstructs as absent, and the query route answers
  `404 index_not_found` — the same predicate a live query uses, applied to the reconstructed
  manifest. ⚠️ There is **no delete-index path in the engine at all** today, so "an index that
  existed and was removed" is unreachable rather than untested.
- **A compaction between the read and the query** can delete a segment the reconstruction
  names. That is the same race a live query has with GC, bounded by the same retention, and the
  answer is the same: the read fails loudly rather than returning a short index.
- **`reaped_before` is written by whoever runs `gc`**, and a deployment that never runs it has
  an unbounded window and an ever-growing graveyard. That is M6d's trade, unchanged.

## Tasks

| Id | Commit |
|---|---|
| M7e.1 | `Head::as_of`, `reaped_before`, and the bound |
| M7e.2 | `Engine::query_as_of`, and `gc` recording the horizon |
| M7e.3 | The API parameter, the refusals, and the ledger |
