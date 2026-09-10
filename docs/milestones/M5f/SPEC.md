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
    ⚠️ **A segment that carries `TextPostings` and whose dictionary did not arrive must fail
    the query.** `open` fetches sidecars through `maybe`, which swallows the failure because a
    missing *centroid* table legitimately means "scan exactly" (D-10); applied to a term
    dictionary across N segments that would drop the segment out of `doc_count` and every
    `df`, scoring the whole query against a smaller corpus.
    ⚠️ **AMENDED after implementation: no new guard is needed, and the one written first was
    removed.** That segment's own text leg already fails on the same missing sidecar and
    `try_join_all` fails the query with it, so no wrong answer can be returned. A guard in
    `open` was added, and **no mutation of it could be caught** — the gate said so. Criterion 5
    pins the property; the leg-level refusal is what enforces it.
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
2. **Global statistics, computed once.** Over a two-segment corpus whose halves disagree about
   **both** corpus-wide inputs to BM25 — how common a term is, and how long a document is —
   the multi-segment query's ranked **document ids** equal those of the same documents held in
   a single segment.
   ⚠️ Compared by **document id, not by row**: the two layouts number rows differently by
   construction, so a row comparison would fail where nothing is wrong.
   ⚠️ **And the fixture must be able to fail.** Equality alone passes wherever per-segment
   statistics happen to agree with the global ones — the trap M5c's ledger records finding, and
   the trap this milestone fell into: the first fixture disagreed only about `df`, and the
   per-segment mutation **survived**. The rebuilt fixture disagrees about `avgdl` too, so the
   global order puts a short document of segment A first and segment B's single hit last, and
   the per-segment order puts B's first. Both positions are asserted.
3. ⚠️ **Fusion happens once, over the union.** Two retrievers over two segments: the fused
   scores are not all equal, which is what fusing per segment and merging produces — RRF is
   blind to score magnitude, so every segment's top hit would earn the same `1/(k+1)`.
4. ⚠️ **Each retriever's union is re-ranked before it is fused.** A fixture whose **best** hit
   is in the second segment: the top hit is that document. `fuse` reads a leg's position as its
   rank, so an unsorted concatenation ranks segment 0's hits above segment 1's — and with one
   retriever the final order *is* the leg's input order, so nothing downstream repairs it.
   ⚠️ Added after the mutation sweep, which is where it belongs in the record: on a fixture
   where segment A holds every good hit, score order and concatenation order agree and the
   missing sort is invisible.
5. **Depth is 2 over N segments, not 2N.** Asserted with `DepthCounting` at N = 4: the open
   round and the legs round, **and equal to the depth the same query costs over one segment**,
   which is the form that does not depend on the fixture. ⚠️ The constant 2 rests on every
   segment's meta region fitting its suffix read — a segment over `INDEX_BUDGET` costs a second
   read at open, and `try_finish` refuses one, so the fixture cannot silently violate it.
   Requests scale with segments and bytes, which is what the rule permits; **depth does not**.
6. **A missing term dictionary fails the query**, rather than scoring against a corpus one
   segment too small. ⚠️ **No new code enforces this**, and that is the finding: the text leg
   already refuses on the same missing sidecar and `try_join_all` fails the query with it. A
   guard added in `open` could be mutated away with every test still green, so it was removed
   rather than left as code nothing constrains.
7. **Zero LIST**, asserted on the request-class counter.
8. **One segment is unchanged.** The existing `hybrid` and text suites are green with no
   fixture edits beyond `Hit`'s new field and the paired `Target`, `scripts/ndcg.sh` and
   `scripts/depth.sh` hold, and every hit of a one-segment query carries `segment == 0`.
9. Region coverage ≥95% on the changed crates, mutation ≥80% on the changed modules, the full
   gate set green.

## Test plan

| # | Test | The mutation it kills |
|---|---|---|
| 1 | `two_segments_with_the_same_row_are_two_hits`, `ties_break_on_the_pair_and_the_segment_comes_first` | `fuse` keyed on `row` alone — the defect that exists today, and every single-segment test passes with it. Both **observed red**. |
| 2 | `global_statistics_change_the_ranking_across_segments` | each segment scored against `idx.summary()` instead of the merged statistics. ⚠️ **Survived the first fixture** and is killed by the rebuilt one. |
| 3 | `fusion_happens_once_over_the_union` | per-segment fusion, which flattens every segment's top hit to the same RRF credit |
| 4 | `each_retrievers_union_is_ranked_before_it_is_fused` | the union re-sort removed. ⚠️ **Survived every earlier fixture**, because they all put the good hits in segment A. |
| 5 | `four_segments_cost_one_open_round_and_one_leg_round` | the segment loop awaited in sequence, which is functionally identical and 4× the depth. **Observed red.** |
| 6 | `a_missing_term_dictionary_is_an_error_not_a_smaller_corpus` | ⚠️ **nothing** — the leg's existing refusal covers it, and the guard written for it was removable with every test green. Recorded, not hidden. |
| 7 | `a_multi_segment_query_does_not_list` | a LIST introduced by the fan-out |
| 8 | `a_one_segment_query_is_unchanged` | `segment` defaulted to something other than 0 |
| 9 | the existing `hybrid`, text, `fusion`, `ndcg` and `depth` suites | a tie-break that now depends on segment ordinal before score, which reorders every existing answer |

⚠️ Criteria 1–4 are the milestone, and **two of the four mutations survived their first
fixture**. Both are recorded above rather than quietly fixed: a fixture that cannot fail is the
failure mode this project's own spec preamble warns about, and it caught this milestone twice.

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
