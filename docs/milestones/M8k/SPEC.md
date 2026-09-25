# M8k — `pstore-index`'s RaBitQ: a vector on its centroid scores NaN, and 21 survivors

**Serves:** **D-111**, and D-11 (the 1-bit code with its error bound). M8j's committed
measurement ([`../M8j/sweep/workspace-recheck.txt`](../M8j/sweep/workspace-recheck.txt))
leaves **21** survivors in `rabitq.rs`, re-run against the whole workspace.

## Delta

**⚠️ A defect, found through `norm > 0.0`'s mutants: a vector exactly on its centroid scores
NaN.** The module says such a vector's reconstruction is "`<c,q>` with a zero scale, which is
exactly right". It is not: its rotated residual is all zeros, so its alignment is `0`,
`estimate_prepared` is `raw / 0 = ±∞`, and `estimate_residual` is `<c,q> + 0 × ∞ = NaN`.
Observed red: `a_vector_on_its_centroid_scores_the_centroid_not_nan` scores NaN. Measured on
this x86-64 container, `0 × ∞` is `0xffc00000`, **negative** NaN, which `total_cmp` sorts
**last**: the rung-0 ranking in `VecIndex::search` drops the row before any rerank, even when
it is the exact match. (On aarch64 the default NaN is positive and would sort **first** — not
measured here.) It is not rare: a single-row posting list's centroid **is** its row, and so is a
list of duplicates.
**Fix, two guards:** `estimate_residual` returns `<c,q>` for a zero `residual_norm`, as
documented; and `estimate_prepared` returns `0.0` for a code whose alignment is `0` — the zero
vector's code, whose inner product with anything is `0`, reachable through `encode` too.
⚠️ **Amended at spec review:** the first draft had only the alignment guard, claiming alignment
is `0` only for the zero vector. Not so near underflow: a residual with components around
`1e-39` squares to a norm of `0` but keeps a **nonzero subnormal** alignment, so `raw /
alignment` overflows and `0 × ∞` is still NaN. Observed red:
`a_residual_too_small_to_have_a_norm_scores_the_centroid_not_nan`. Hence the norm guard.

**Tests (19 mutants), each value exact:**
- `Display` (92): the message is `"expected 4 dimensions, got 3"`.
- `residual_norm` (139, three constants) and the residual's `-` (324:29 `→ +`): `v − c = (3, 4, 0…)`
  gives `residual_norm() == 5.0`.
- `dim` (264, two constants): `Quantizer::new(5).dim() == 5` (padded to 8).
- **The unit residual** (329:38 `>`→`==`, `<`; 330:39 `/`→`%`, `*`): `encode_residual(v, c)`
  equals `encode((v − c)/5)` in bits and alignment — the mutants skip or botch the normalising
  division, and alignment scales with the norm. 329:38 `>`→`>=` is caught by the on-centroid
  test's **stored-code** assertions (alignment `0`, no bits): ⚠️ **amended at code review** —
  the norm guard returns before the alignment is read, so without them the mutant's NaN
  alignment reached only the persisted code, which nothing asserted.
- **An exact zero coordinate is not positive** (353:19 `>`→`>=`). Codes are **persisted**
  (`read_code`), so the bit for an exact `0` is part of the format. `without_rotation(2)`:
  `encode([1, 0])` scored against `[0, 1]` is `−1/√2`, not `+1/√2`.
- **The query's int4 path** (228:15 `<=`→`>`) and `estimate_unnormalized_for_test` (421, two
  constants): `without_rotation(2)`, query `[1.0, 0.3]` quantized to `step = 1/7` scores exactly
  what the same arithmetic gives in the test, against `encode([1, 1])`; the mutant returns the
  query unquantized.
- **The decomposition** (401:43 `*`→`+`): `estimate_residual(code, q, c·q) − c·q ==
  residual_norm × estimate_prepared(code, q)`, the identity the doc states, at a norm of 5.
- **The bound's `D − 1`** (432:37 `-`→`+`, `/` — the `-` in `padded.max(2) - 1`):
  `without_rotation(4).encode([1, 0, 0, 0])` has alignment exactly `0.5`, and the bound is
  `ε · √0.25 / 0.5`, exactly `BOUND_EPSILON`; the mutants give 3.02 and 3.38. ⚠️ **Amended at
  spec review:** the first draft used alignment `1`, where the numerator is `0` whatever
  divides it — it killed neither mutant. That case stays as a property, catching nothing.

**Rung 1 — equivalent (2):**
- 296:17 `while h < padded` → `<=`: the extra round splits each chunk at `padded` and zips it
  with an empty half. Rewritten as `for k in 0..padded.trailing_zeros()`, `h = 1 << k`.
- 293:23 `x * flip` → `x / flip`: every flip is exactly `±1`. **Excluded by line and column**
  in `.cargo/mutants.toml`, with its premise tested inline: every flip of several dimensions is
  exactly `1.0` or `-1.0`. Rung 1 (flips as signs, `if neg { -x } else { x }`) was declined:
  it trades one equivalent mutant for a negation with mutants of its own, to express the same
  arithmetic.

**Does not change:** any code's bits or alignment, or any estimate, except the NaN above.

## Acceptance criteria

1. The on-centroid and underflowed-residual tests score `<c,q>` exactly; the zero vector's
   own code estimates `0.0`.
2. Each test above passes, and each named mutant was seen failing it.
3. `rotate` has no `while h < padded`; the flip exclusion names one line and column, and its
   premise test passes; `cargo mutants --list` over the file drops by exactly that one.
4. Every `rabitq.rs` mutant against `pstore-index`'s tests, then any survivor against the
   workspace: **0 missed**. Timeouts count as caught.
5. `./scripts/recall.sh` and `./scripts/gates.sh` pass.

## Test plan

| # | Fails first | Mutation it catches |
|---|---|---|
| 1 | the defect itself, on the code as it is | a zero vector scored as NaN |
| 2 | each named mutant, one at a time | the accessor, the norm, the sign of 0, the int4 step, the decomposition, the bound |
| 3 | the list without the exclusion | an exclusion broader than one mutant |
| 4 | the M8j measurement: 21 missed | all of the above |

## RA budget

Unchanged: in-memory scoring.

## Risks

- The fix changes an estimate from NaN to `<c,q>`, which can change a ranking. That is the
  fix; `recall.sh` is in criterion 5 to show nothing else moved.

## Tasks

- **M8k.1** — the fix, the tests, the rotate rewrite, the exclusion, this ledger.
