# M8l — `pstore-index`'s last survivors: a replica coded against the wrong centroid, and 17

**Serves:** **D-111**, and D-8 / D-11 (balanced lists with replication, residual codes).
M8j's committed measurement ([`../M8j/sweep/workspace-recheck.txt`](../M8j/sweep/workspace-recheck.txt))
leaves **17** survivors outside `lire.rs` (M8j) and `rabitq.rs` (M8k): `cluster.rs` 9,
`search.rs` 3, `sq8.rs` 2, `vec_index.rs` 3. Line numbers are the measurement's.

## Delta

**⚠️ A defect, found through `363:40` (`*first + *len` → `*`): a replicated document's copy
is coded against the wrong centroid.** `assemble` recorded each row's `home` list keyed by
**document**, so a document replicated into lists A then B got home B, and **both** copies
were coded as `v − c_B`. Search scores each copy with **its own** list's `<c, q>`, so the copy
in A estimated `<v, q> + <c_A − c_B, q>`, too high or too low. Dedup keeps the higher copy,
which is then not the accurate one. Every rung reranks only what rung 0 kept, so a copy
scored too low can lose its row the rerank would have found. Default params replicate:
`replicas: 1, boundary: 0.05`, index 1.72× (`Params::default`).
Observed red: `every_rung_zero_score_is_the_rows_code_against_its_own_lists_centroid` —
`d2 (2 copies) scored 0.15838747, its codes give 0.18224691`.
**Fix:** `home` is built beside `order`, one entry per **index row**, as the lists are laid
out (`repeat_n(li, list.len())`). The span arithmetic that held the mutant is gone.

**Tests (16 mutants), each value exact:**
- **Rung 0** (`363:40`): every hit of a query probing every list at `k = n` scores exactly
  the best of its copies' `estimate_residual(encode_residual(v, c_list), q, <c_list, q>)`,
  recomputed from `Clustering::build` over the same corpus (asserted to be the build's).
- **Rung 1's span lookup** (`704:59` `<`→`<=`): the same query at `Rerank::Fast` scores every
  hit exactly `sq8::estimate(encode(v), q)`. The mutant finds a row at `first + len` in the
  previous span's buffer, past its end, and scores `-inf`.
- **No vector, no codes** (`380:16` `>`→`>=`): a segment over a field no document carries
  has empty `RaBitQ` and `Sq8` sections; the mutant writes a width-error fill per row.
- **The slack** (`search.rs` `50:25` `*`→`/`, `50:32` `+`→`-`, `*`): centroids at 1, 1.5, 3
  from the query, slack `1.0`: `probe_pruned` keeps exactly `[0, 1]`.
- **The cost** (`124:9`, three constants): a hand-built clustering's `assignment_cost` is
  exactly `1 + 1 + 4 = 6`.
- **The seeding tie** (`247:23` `>`→`>=`): rows `[0, −1, 1]` in 1-D, two lists; rows 1 and
  2 tie, and the first row wins: lists `[[0, 2], [1]]`. The mutant mirrors it. The doc
  now states "ties to the lowest row", as `assign` and `augment` already do.
- **The cap** (`346:66` `<`→`<=`): rows `[0, 10, 0.1, 0.2]`, cap 2: `[[0, 2], [1, 3]]`.
- **`boundary: 0.0` replicates nothing** (`271:29`, `271:55` `||`→`&&`): rows `[−1, 1, 0, 0]`
  give centroids `±0.5`, and the two rows at `0` tie **exactly**. The `≤ (1 + 0) ×` rule would
  admit a tie, so only the early return makes `0.0` mean what `Params::boundary` says.
- **Two lists replicate** (`271:74` `<`→`==`, `<=`): the same rows at `boundary: 0.05` give
  `[[0, 2], [1, 2, 3]]`.
- **A range too narrow for a step** (`sq8.rs` `41:21` `>`→`>=`): `encode([0, 2⁻¹⁴⁹])` has a
  zero step (underflow). Its wire bytes equal `encode([0, 0])`'s; the mutant codes `[0, 255]`.
  Codes are persisted, so this is format, as M8k's zero-coordinate bit was.

**Rung 1 — equivalent (1):** `sq8.rs` `37:22` `hi > lo` → `>=` differs only for an
all-infinite vector (`inf − inf = NaN`). Rewritten as `(hi − lo).max(0.0) / 255.0`: bit-for-bit
the same step on empty, constant, `±inf`, NaN-bearing, subnormal-range and overflowing inputs
(checked by a throwaway program; not a test, since the branch it compares against is gone).

**Does not change:** any code, estimate or clustering except the replica codes above. Built
row order, spans, sq8 bytes and each list's membership stay the same.

## Acceptance criteria

1. The rung-0 test was observed red on the defect and passes, on a fixture that replicates.
2. Each test above passes, and each named mutant was seen failing it.
3. `sq8::encode`'s step is `(hi - lo).max(0.0) / 255.0` (`hi > lo` survives only in its comment);
   `vec_index.rs`'s one `*first + *len` is search's span lookup, none in `assemble`.
4. Every mutant in `cluster.rs`, `search.rs`, `sq8.rs` and `vec_index.rs` against
   `pstore-index`'s tests, then any survivor against the workspace: **0 missed**. Timeouts
   count as caught.
5. `./scripts/recall.sh` and `./scripts/gates.sh` pass.

## Test plan

| # | Fails first | Mutation it catches |
|---|---|---|
| 1 | the defect itself, on the code as it is | a replica coded against another list's centroid |
| 2 | each named mutant, one at a time | span end, empty field, slack, cost, seed tie, cap, boundary, sq8 step |
| 3 | — (a grep) | the rewritten code coming back |
| 4 | the M8j measurement: 17 missed | all of the above, and anything new the rewrites made |

## RA budget

Unchanged: build-time coding and in-memory scoring. The fix adds no request.

## Risks

- The fix changes rung-0 scores of replicated rows, so rankings move. That is the fix;
  `recall.sh` in criterion 5 shows it did not cost recall.
- Codes already persisted keep the wrong centroid until their segment is rebuilt, and nothing
  marks them. There is no deployed data yet.

## Tasks

- **M8l.1** — the fix, the tests, the sq8 rewrite, this ledger.
