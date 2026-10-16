# M94 — A `multi` leg splits across peers

**Serves:** backlog row 64, opened by [M61](../M61/VERIFIED.md): the part protocol carries no
`multi` leg, so `shares` refuses every query holding one and the whole query runs on the server
asked. M67 added `multi_fde` and kept the same refusal.

## What is true today

- `shares` (pstore-engine) returns no share when any leg is `Multi` or `MultiFde`.
- `WirePart::of` drops those legs, `splittable` says false, `decode` has no variant for them.
- ⚠️ `decode` hard-codes `fde_clusters: false` on every target. A peer therefore scans the flat
  encoding where the coordinator would read the clustered sidecar, so a split `multi_fde` (and
  today's split named-dense leg, M83) could answer differently from the unsplit query. The flag
  never crossed the wire because nothing needed it to be right.

## Delta

**A `multi` or `multi_fde` leg is split exactly as a dense leg is: per segment, scored by the
row alone, so hits over two segments compare wherever they were computed. Protocol numbers do
not change.**

1. `splittable` is true for `Multi` and `MultiFde` (one exchange; they score each row alone).
2. `shares` no longer returns early for them.
3. The wire carries both legs: query vectors, `limit`, and for `MultiFde` the `rerank` and
   `probes`, every float as its bits.
4. `WireTarget` carries `fde_clusters` (`serde(default)`, so an older coordinator's part decodes
   as before); `decode` uses it. This also corrects the named-dense split (M83).
5. A peer that does not know the new variants fails to decode the part; the coordinator counts
   a failed part and runs the share itself (M54), as it does for any failed share.

**Not changed:** the answer, the depth, the request count per segment, the protocol constants,
`rerank`'s reads (still outside the depth budget), HTTP API.

## Acceptance criteria

1. **A split `multi` query equals the unsplit one.** `a_split_multi_query_equals_the_unsplit_one`
   in `crates/pstore-engine/tests/split.rs`: `Prefetch::Multi` and `Prefetch::MultiFde` (with and
   without `rerank`), alone and beside a dense leg, over four folded segments on three servers:
   hits and scores equal in bits to the unsplit answer, and shares were served (parts > 0).
2. **A text leg beside a multi leg splits in two phases.** Same file,
   `a_split_multi_with_text_equals_the_unsplit_one`: `Multi` + `Text` through the phased path
   equals the unsplit answer, and phases were served.
3. **The wire is bit for bit.** `a_part_survives_the_wire_bit_for_bit` (pstore-server unit test)
   round-trips `Multi` and `MultiFde` legs (`-0.0`, `1/3`, `rerank`, `probes` both `Some` and
   `None`) and a target with `fde_clusters: true`; and `guard` refuses a repeated leg number across
   the new variants.
4. **Over HTTP, split equals unsplit, and the split happens.** `a_multi_query_is_split` in
   `crates/pstore-server/tests/peers.rs` (the old `a_multi_query_is_not_split`, inverted): the six
   query shapes of the old test answer as one server alone does, and `pstore_peer_parts_sent`
   rises for each.
5. **Clustered segments split too.** `a_split_clustered_fde_query_equals_the_unsplit_one`
   (split.rs): two folded segments large enough to carry the cluster sidecar, a `MultiFde` leg,
   equal to the unsplit answer in bits, with the share served and the part's targets flagged
   `fde_clusters`. Red if `decode` drops the flag, via criterion 3.
6. **Failures cost rounds, not answers.** `a_failed_peer_costs_rounds_not_answers_for_a_multi_leg`
   (`crates/pstore-server/tests/peers.rs`): a dead peer and a failing store, `multi` and `multi_fde`
   alone, beside a dense leg and beside a text leg: the answer is the one server's, and each
   failed share is counted once per part.
7. **Which legs split in one exchange is pinned.** `only_legs_that_score_a_row_alone_split_in_one_exchange`
   (`crates/pstore-query/tests/splittable.rs`): dense, sparse, `multi` and `multi_fde` are
   splittable; text and trigram are not. Added because the sweep's first run left
   `splittable -> true` MISSED.
8. **Gates.** `scripts/gates.sh`, the coverage floors, and the incremental mutation sweep over
   the diff: 0 missed.
