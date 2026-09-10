# M5f — The cross-segment identity four ledgers handed forward

**Serves:** the gap [M3b](../M3b/VERIFIED.md), [M5a](../M5a/VERIFIED.md),
[M5b](../M5b/VERIFIED.md) and [M5c](../M5c/VERIFIED.md) each named and none built — *"the
caller needs a stable cross-segment identity, which M5a, M5b and M5c have all handed forward by
name"* — and **D-30**'s two-pass IDF, whose statistics half M5c built for a caller that does
not exist.

## ⚠️ The failure this milestone exists to prevent, and it is reachable today

`fuse` accumulates by `hit.row` into one map. `Hit`'s own doc says why: *"A **row**, not a
document id, and that is a stated limit rather than an oversight."* The limit is stated and
**not enforced**. A caller who fuses two segments' legs today gets segment 0's row 5 and
segment 1's row 5 **summed into one hit** — two unrelated documents merged, at an inflated
score, ranked above both. No error, no warning, and every leg is individually correct.

Two more, each a confidently wrong ranking rather than a failure:

1. ⚠️ **Per-segment IDF.** M5c measured this from the other side: global IDF *changes the
   top-1*, and per-segment IDF is wrong exactly when the query's discriminating term is the one
   whose frequency differs between segments. A multi-segment query that scored each segment
   against its own statistics would undo the whole of D-30.
2. ⚠️ **RRF fused per segment and then merged.** RRF is blind to score magnitude by design, so
   the top hit of *every* segment earns `1/(k+1)` regardless of how good it is. Merge those and
   a ten-segment index surfaces ten equal "winners". Fusion has to happen **once**, over the
   union.

## Delta

**`pstore-query`**
- `Hit` gains `segment: usize` — an index into the segment list the query was given. ⚠️ **One
  type, not a second `GlobalHit`.** `Hit`'s doc already names the hazard of inventing the
  identity twice: two types that must agree are how they come to disagree.
- `fuse` keys on `(segment, row)`.
- `query` takes `&[Target]` instead of one `Key` and one shared centroids `Key`, where
  `Target { segment: Key, centroids: Key }`. ⚠️ **Not one shared centroids key.** A centroid
  table records *this* segment's cluster assignments; pointing every segment's dense leg at one
  table returns another segment's clusters applied to these rows — a confidently wrong
  candidate set, and the same shape as the row collision above. `VecIndex::open` already takes
  segment and centroids as a pair, which is where the pairing belongs.
- With that list, `query`:
  - opens every segment and every sidecar in **one** round — N footers, N term
    dictionaries, N sparse dictionaries, concurrently. ⚠️ `Engine::scan`'s comment already
    makes this argument: a loop over segment refs "turns a ten-segment index into a
    twenty-one-hop query — measured, not guessed".
  - merges the term dictionaries' summaries with `Stats::merge` **before** any leg runs. The
    summaries arrive in the open round, so global statistics cost no round trip — which is what
    D-30 says two-pass IDF must not.
    ⚠️ **A segment that carries `TextPostings` and whose dictionary did not arrive is an
    error.** `open` fetches sidecars through `maybe`, which swallows the failure because a
    missing *centroid* table legitimately means "scan exactly" (D-10). Applied to a term
    dictionary across N segments that becomes silent corruption of a **global** number: the
    segment drops out of `doc_count` and every `df`, and the whole query is scored against
    statistics for a smaller corpus. One segment's absent sidecar must fail the query, not
    quietly change everyone's IDF.
    ⚠️ A segment with **no** text index contributes nothing to `Stats`, and that is correct
    rather than an omission: it holds no documents containing the field, so it is not part of
    BM25's corpus. Stated so it is not later "fixed".
  - runs every segment's legs in **one** further round, then fuses once over the union.

**Does not add** — **a durable identity.** `(segment, row)` is stable for the query's HEAD
snapshot, which is all fusion needs; a compaction moves rows, so it is not an external handle.
Nothing needs one: there are no deletes or updates by id, and `Engine::search` already returns
`Document.id`. **Resolving hits to documents** — that is a block fetch per segment and it is
already outside `query`'s budget, exactly as it is for a single segment.
**`Engine::query`** — ⚠️ blocked on something that is not identity: `Engine::seal` writes no
centroid table, so a folded segment has no ANN index and `Engine::search` is brute force over a
full `scan`. Building the index at fold time is a milestone of its own. Named as a backlog row.
**Weighted fusion** — still D-27's escape hatch, still not the default.

## Acceptance criteria

1. ⚠️ **Rows from different segments never merge.** Two segments, a hit at row 5 in each: the
   result carries **two** hits with their own scores, not one with the sum. Observed red
   against today's `fuse`, which merges them.
2. **Global statistics, computed once.** Over a two-segment corpus whose segments give a term
   different document frequencies, the multi-segment query's ranked **document ids** equal
   those of the same documents held in a single segment, and **differ** from the same query
   scored with per-segment statistics. ⚠️ Both halves: equality alone passes over a fixture
   where the statistics happen to agree, which is the trap M5c's own ledger records finding.
   ⚠️ Compared by **document id, not by row**, and the test asserts the top-k scores are
   pairwise distinct. Rows differ between the two layouts by construction, and a tie broken by
   `(segment, row)` in one arrangement and by `row` in the other would make the criterion fail
   on a fixture where nothing is wrong.
3. ⚠️ **Fusion happens once.** A fixture where fusing per segment and merging gives a
   different order from fusing the union: the query returns the union's order. The per-segment
   order is asserted to differ, so the test cannot pass by the two being equal.
4. **Depth is 2 over N segments, not 2N.** Asserted with `DepthCounting` at N = 4: the open
   round and the legs round, **and equal to the depth the same query costs over one segment**,
   which is the form that does not depend on the fixture. ⚠️ The constant 2 rests on every
   segment's meta region fitting its suffix read — a segment over `INDEX_BUDGET` costs a second
   read at open, and `try_finish` refuses one, so the fixture cannot silently violate it.
   Requests scale with segments and bytes, which is what the rule permits; **depth does not**.
5. ⚠️ **A missing term dictionary fails the query.** A two-segment fixture where one
   segment's sidecar is deleted returns an error naming the segment — not a ranking computed
   from the other segment's statistics. Observed red by letting `Stats::merge` run over the
   summaries that did arrive, which answers confidently with a corpus one segment too small.
6. **Zero LIST**, asserted on the request-class counter.
7. **One segment is unchanged.** The existing `hybrid` and text suites are green with no
   fixture edits beyond `Hit`'s new field, `scripts/ndcg.sh` and `scripts/depth.sh` hold, and
   every hit of a one-segment query carries `segment == 0`.
8. Region coverage ≥95% on the changed crates, mutation ≥80% on the changed modules, the full
   gate set green.

## Test plan

| # | Test | The mutation it kills |
|---|---|---|
| 1 | `two_segments_with_the_same_row_are_two_hits` | `fuse` keyed on `row` alone — the defect that exists today, and every single-segment test passes with it |
| 2 | `global_statistics_change_the_ranking_across_segments` | `Stats::merge` skipped and each segment scored against its own dictionary; also a merge that takes the first segment's summary rather than the sum |
| 3 | `fusion_happens_once_over_the_union` | fusing per segment and summing — which gives the top hit of every segment the same RRF credit |
| 4 | `four_segments_cost_one_open_round_and_one_leg_round` | the segment loop awaited in sequence, which is functionally identical and 4× the depth |
| 4b | `each_segments_dense_leg_reads_its_own_centroids` | one shared centroid table, which returns another segment's clusters applied to these rows and still answers |
| 5 | `a_missing_term_dictionary_is_an_error_not_a_smaller_corpus` | the `maybe`-shaped skip carried over to dictionaries, which changes a global statistic silently — the one failure mode this whole milestone is about, one level down |
| 7 | `a_one_segment_query_is_unchanged` | `segment` defaulted to something other than 0, or the union path taken for N = 1 with a different tie-break |
| 8 | the existing `hybrid`, text, `ndcg` and `depth` suites | a tie-break that now depends on segment ordinal before score, which reorders every existing answer |

⚠️ Criteria 1–3 are the milestone. Criterion 4 is what keeps it from being a loop.

## RA budget

| Operation | W | Rseq depth | Rpar | List |
|---|---|---|---|---|
| Query over N segments | 0 | **2** | N footers + the sidecars each leg needs, then one ranged fetch per segment per leg | 0 |
| Query over 1 segment | 0 | 2 — unchanged | unchanged | 0 |

⚠️ **Rpar scales with N and that is the trade.** Ten segments is ~30 requests in the open round.
Depth is what the budget bounds; request count is bounded by compaction keeping N small, which
is compaction's existing job and not this milestone's.

## Risks

- **`Hit` is a public breaking change.** Every caller reading `hit.row` still compiles; every
  caller *constructing* a `Hit` does not. That is deliberate — a caller who builds a `Hit`
  without saying which segment it came from is the defect.
- **N in the open round.** A pathological index with hundreds of segments makes one query
  hundreds of requests. Nothing here bounds N, and the risk is that this milestone reads as
  making many segments cheap. It makes them *shallow*, not cheap.
- **`Stats::merge` over N dictionaries is O(distinct terms).** CPU, not requests, and it runs
  on bytes already fetched — but it is per query, and a 30,000-term dictionary × N is the shape
  to watch if it ever shows up in a profile.

## Tasks

| Id | Commit |
|---|---|
| **M5f.1** | `Hit` names its segment, and `fuse` keys on the pair |
| **M5f.2** | `query` over N `Target`s: one open round, global statistics, one fusion |
