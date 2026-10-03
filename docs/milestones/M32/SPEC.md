# M32 — No dictionary read a query knows is absent

**Serves:** [BACKLOG](../BACKLOG.md) row 51, which [M27](../M27/SPEC.md) opened. A text or sparse leg
asks every segment for its dictionary, and a segment without one answers with a 404 that no
cache keeps. This is M27's cost for the other two legs.

## What is true today

- `pstore-query`'s open round fetches, for every target, a segment's footer and, beside it:
  - its sparse dictionary when the query has a sparse leg;
  - its term dictionary when it has a text leg (`run.rs` around line 736).
- A segment has a dictionary only when its rows carry that field: `seal` writes a sidecar only
  when it is non-empty (`lib.rs`, "Written only when there is something in it").
- HEAD's `SegmentRef` is `{ key, rows }`. Nothing records which dictionaries a segment has, so
  M27's trick (decide from HEAD) has no fact to decide on.
- **Measured** here (a probe on M27's counting store, not committed): a default engine folds
  8 vector-only segments, then one with text. A text query by another engine makes 21 reads,
  9 of them dictionary reads: the text segment's, and 8 that 404, one per vector-only
  segment. Both passes gave the same counts; the probe had no read cache, which keeps no 404
  anyway (M20). Sparse was not measured, and criterion 1 measures it.
  - Compaction merges them away. But an index whose text arrives late, or a sparse field used
    by some rows only, pays this on every query until then.
  - Since M30, such a text query answers rather than refusing, so this is a reachable path.
- ⚠️ **A sparse query over such an index fails** (spec review B1). A sparse leg over a segment
  with no sparse dictionary errors `Corrupt("a sparse leg over a segment with no dictionary
  sidecar")` (`run.rs` around line 873). Sparse has no exemption like M30's for text, so an
  index with any segment lacking the sparse field refuses every sparse query until a
  compaction merges that segment away.

## Delta

**HEAD records which dictionaries each segment has, and the open round asks for no other.**
1. **`SegmentRef` gains `dicts: Option<Dicts>`**, where `Dicts { sparse: bool, text: bool }`.
   `None` means unknown, which covers a HEAD from before M32 and a reconstructed past
   manifest.
2. **An optional trailing section, last in HEAD.** For each index in name order: its name, its
   segment count, and one byte per segment (bit 0 sparse, bit 1 text, `0xFF` unknown).
   - It is written whenever any segment's `dicts` is known. Every earlier optional section is
     then written, with a count of 0, by the rule M15.2 set: the encoder's `more` and the
     quarantine's condition widen to include it, about 20 bytes of zero counts.
   - A count that disagrees with the index's segments is refused as `CorruptHead`.
3. **Recorded where a segment is made.** `seal` returns which sidecars it wrote, and the fold
   and the compaction record that. A replica copies its source's `dicts`. A branch copies the
   refs whole, as it already does.
4. **`pstore_query::Target` gains `sparse_dict: bool` and `text_dict: bool`**, each true unless
   the engine knows the segment has none. The open round asks only for those. A segment
   without a term dictionary then reaches M30's exemption, which judges by the footer's text
   fields.
5. **A segment with no sparse field contributes nothing to a sparse leg**, judged by the
   footer (`sparse_field()` is none) before the dictionary check, as M30 did for text. A
   segment whose sparse field is another is still refused.
6. **The fresh segment's `Target`** sets both flags from the sidecars its own build wrote.

**Not changed:** the format of segments and sidecars; every request but the 404s; round-trip
depth; every answer but the sparse refusal; warm (it already reads only what exists, M21).
`as_of` keeps the live refs' `dicts`, which is correct because a segment's dictionaries never
change after it is sealed. Its resurrected refs are `None`, and ask.

## Acceptance criteria

1. **No 404 for a dictionary HEAD says is absent.** The measured sequence makes 1 dictionary
   read for a text query, the text segment's (parent: 9), and a sparse query likewise.
2. **A sparse query over a mixed index answers.** Parent: the `Corrupt` error above. A sparse
   leg naming another field than a segment's is still refused.
3. **A dictionary that exists is still read**, and dense, sparse, text and fused answers over a
   mixed index rank equal to the every-dictionary reader's.
4. **Unknown asks.** A HEAD without the section decodes with `dicts: None` and costs what it did,
   as does an `as_of` ref resurrected from the graveyard.
5. **The section round-trips and refuses malformed input**: a count mismatch, an index absent
   from `indexes`, or a flag byte other than 0–3 and `0xFF` is `CorruptHead`. With no known
   `dicts`, HEAD is byte-identical to the parent's. Truncated at the section's start, as a
   parent-format decoder re-encodes it, a HEAD decodes with every ref `None`, which is safe.
6. **Every maker records it.** After a fold, a compaction, including one retried after a lost
   CAS, a replica sync and a branch, each segment's `dicts` matches the store's sidecars.
7. **Gates.** `./scripts/gates.sh` is green, and the sweep over M32's source diff misses 0.

## Test plan

New `crates/pstore-engine/tests/dict_skip.rs` (criteria 1–4 and 6), plus HEAD unit tests beside
the existing decode tests (5, and 4's old-HEAD half). Each is red on the parent, where the API does not exist, and by
the mutation named. `VERIFIED.md` names the catching test.

| # | Test | Mutation it must catch |
|---|---|---|
| 1 | `vector_only_segments_are_asked_for_no_dictionary` | the open round ignoring the flags |
| 2 | `a_sparse_query_over_a_mixed_index_answers` | the exemption dropped (the parent's error) |
| 3 | `a_dictionary_that_exists_is_still_read`, `mixed_index_answers_are_unchanged` | a flag inverted; text from the sparse bit |
| 4 | `an_old_head_and_a_resurrected_ref_still_ask` | unknown read as "none" |
| 5 | `the_dicts_section_round_trips_and_refuses_malformed_input` | each refusal dropped; the section always written |
| 6 | `every_maker_records_its_dictionaries` | the compaction (first seal or retry) or the replica dropping it |

Criteria 1 and 2 are red on the parent by behaviour. Criteria 3, 5 and 6 hold on the parent
except that their API does not exist, so their red is the named mutation (spec review m8).

## RA budget

| Op | W | Rseq | Rpar | List | Depth |
|---|---|---|---|---|---|
| A text or sparse query | 0 | 0 | −1 per segment without that dictionary | 0 | unchanged |
| HEAD | 0 | 0 | 0 | 0 | unchanged; +1 byte per segment, + a name and a count per index |

## Risks

- **A flag that says absent when the sidecar exists** fails that query loudly: the footer names
  the field, so the exemptions do not apply, and the missing dictionary is `Corrupt`. Only
  `seal` and the fresh build set the flags, from the sidecars they wrote, and criterion 6
  checks them against the store.
- **A rolling upgrade.** A binary from before M32 decodes the new HEAD (its decoder stops after
  the quarantine) and re-encodes it without the section. Every ref then returns to unknown
  and asks, as today. No later commit recovers an old segment's flags; only segments sealed
  afterwards carry them, and the dropped ones ask until a compaction merges them away.
- **HEAD bytes.** At 1M tenants and ~50 indexes of a few segments each, a byte per segment is
  noise beside each key's ~80 bytes.

## Tasks

- **M32.1** — `Dicts`, the HEAD section, `seal`'s return, the makers, `Target`, the sparse
  exemption and the tests. Every other `Target` and `SegmentRef` construction site (the
  `pstore-query` tests, `examples/msmarco.rs`, `examples/open.rs`, `head_cost.rs`) gains the
  new fields, defaulted to today's behaviour.
- **M32.2** — The ledger, `BACKLOG.md` row 51 closed, and the roadmap row.
