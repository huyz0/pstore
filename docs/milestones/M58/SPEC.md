# M58 — `sum` fusion splits too

**Serves:** [BACKLOG](../BACKLOG.md) row 60, which the project owner asked on 2026-10-06 to
close with the rest of the backlog.

## What is true today

Read from the tree at M57, nothing measured:

- **`sum` fusion runs on one server** (M55 rule 9; `shares` returns no share under
  `Fusion::Sum`).
- **Why:** under `sum` every leg is whole, not cut at its limit (M9g.2, `run`). A leg cut at
  its limit drops a row's contribution, and the best sum can be a row no leg ranks first. A
  share's reply in M55's form would therefore be every matching row of every segment.
- **`sum` is per row.** `fuse` keys on `(segment, row)` and adds `weight_j × score` for each
  leg j that scored the row, in leg order. It sorts by score descending, then `(segment, row)`
  ascending. Only text legs are allowed under `sum` (the server refuses a dense leg), so every
  `sum` query has a text leg and splits in M55's two phases.
- **`summed`** fuses the whole legs, keeps the top `top_k + |shadow|`, drops the shadowed, and
  cuts to `top_k`.

## Delta

**A `sum` query splits in M55's two phases. In phase 2 a peer runs its share's legs whole,
masked and rid of deleted rows as `candidates` does, then fuses them all by the query's weights.
It keeps the share's top `keep = top_k + |shadow|` rows and returns every leg's hit for those
rows only, with the scores unchanged. The coordinator fuses what came back with its own whole
legs, exactly as before.**

1. **Exact.**
   - A row lives in one segment, so its sum is computed from that segment's legs alone.
   - The peer returns every leg's hit for each row it keeps, so the coordinator's `fuse`
     recomputes the same sum, in the same leg order, to the same bits.
   - A peer's segment ordinals are the coordinator's (`WireTarget.i`). The peer ranks by
     `fuse`'s own order: score descending, then `(segment, row)`. So the share's order is the
     global order restricted to the share, and any row in the global top `keep` is in its
     share's top `keep`. The cut is **per share**, which bounds a reply at `keep` rows × legs
     (spec review: per segment would be up to 4,096 times that).
   - The peer's fuse indexes the weights by the global leg number `j` carried in `Part.legs`,
     never by position. The mask and the delete filter apply before the fuse, as in
     `candidates`.
   - `summed` then proceeds unchanged.
2. **The part carries the cut.**
   - `Part` gains `sum: Option<Sum>`, where `Sum` holds the weights and `keep`. `shares` gains
     the query's `top_k` to compute it.
   - `pstore_query::scan_part` and `part` take it. When it is set, legs run whole, and the
     share is fused and cut as above.
   - It travels in the `open` message, since the held part is what phase 2 scans, so
     `WireScan` is unchanged.
   - ⚠️ **The coordinator's fallback carries it too** (spec review). `run` runs a failed
     phase-2 share through `part`, which today cuts legs at their limit. Under `sum` it passes
     the query's `Sum`, so that fallback runs whole.
3. **Protocol 3.** A `sum` part travels as protocol 3, phases `open` and `scan`, carrying the
   weights as `f32` bits and `keep`.
   - A server of M55's build refuses protocol 3 with `409`, and the coordinator runs that
     share itself.
   - Protocol 2 stays as it is, so a peer of M55's build is never handed a whole-leg part it
     would cut by limit.
   - A server of this build refuses a protocol-2 part carrying `sum`, with `400`, by an
     explicit check. `WirePart` ignores unknown fields, so nothing else would refuse it.
   - `serve_scan` accepts protocol 3 as it does 2.
4. **`shares`** splits `Sum` only when a text leg makes the share phased. A `sum` query with no
   text leg, which the server refuses but the engine does not, is not split (spec review).
   Still not split: `order_by`, aggregations, `as_of`,
   an index of one segment, and a share past 4,096 segments.
5. **Failure** is as M55's: a share failing in either phase is run here, scored with the
   global statistics, its legs whole.

**Not changed:** `fuse`, `summed`, the answers, the depth, and RRF and `max` parts.

**Not covered, and the ledger says so:** a peer's reply is up to `keep` rows × legs per share,
not `top_k` overall.

## Acceptance criteria

1. **Exact, in the engine.** `a_split_sum_query_equals_the_unsplit_one`: `query_split_as` under
   `Fusion::Sum` against `query_filtered_as`, hit for hit in score bits.
   - Cases:
     - two text legs with unequal weights;
     - with a filter;
     - with unfolded writes shadowing folded rows that sit in a peer's share and rank inside
       its top `top_k`;
     - a `top_k` larger than any share's matches;
     - a share failing phase 2, whose fallback must run whole.
   - The corpus has rows that rank first in no leg but first by sum, in a peer's share.
   - Every case asserts a phased call per share, carrying `sum`.
   - Seen red with:
     - the peer cutting each leg at its limit instead of fusing;
     - the cut at `top_k` instead of `keep`;
     - the phase-2 fallback passing no `Sum`.
2. **Bounded replies.** In AC1's runs, no share returns more than `keep` distinct rows. Seen
   red with whole legs returned uncut.
2b. **No text leg, no split.** `shares` under `Sum` with dense legs only returns no share.
   This is a unit test in the engine, seen red with the guard removed.
3. **Through the API.** `a_split_query_answers_exactly_as_one_server_does` gains a `sum` query
   with weights. It is split and answers equal to the unpeered server.
   `queries_that_cannot_be_split_run_on_one_server` drops `sum`.
4. **Protocol.** A `sum` part is protocol 3. A peer answering `409` to it has its share run here,
   with an equal answer. A server of this build refuses a protocol-2 part carrying `sum`, with
   `400`. Test: `a_sum_part_is_protocol_three`.
5. **Gates:**
   - `./scripts/gates.sh` passes;
   - the incremental mutation sweep misses 0, or each miss is killed by a test named in the
     ledger.

## Test plan

| AC | Seen red first under |
|---|---|
| 1 | per-leg cut at limit; cut at `top_k`; fallback without `Sum` |
| 2b | the text-leg guard removed |
| 2 | uncut whole legs |
| 3 | `shares` still refusing `Sum` |
| 4 | `sum` sent at protocol 2 |

## RA budget

Unchanged: W, Rseq, Rpar, List and depth as at M55. A `sum` query that was one server's is now
split, with M55's two server-to-server exchanges per peer.

## Risks

- **Bit-exactness of the sum.** It holds only if peer and coordinator add in the same leg
  order. Both use `fuse`, and AC1 compares bits.
- **Tie at the cut.** Ties are ordered by `(segment, row)` on both sides, over the same
  ordinals, so a tie at a share's cut keeps the same rows the global order would.

## Tasks

- **M58.1** `Part::sum`, the per-share cut, protocol 3, `shares`: AC1–AC5.
