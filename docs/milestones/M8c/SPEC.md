# M8c — the fault injector's own thresholds are pinned

**Serves:** **D-111** (a mutation that survives is a test that does not constrain) and
**D-99**, which makes `Faulty` the primary correctness vehicle — so a fault injector whose
rates are unconstrained weakens every test built on it.

## Delta

The nightly sweep on `25089c7` (CI run 35702526909, shard 0) reported **8 missed mutants,
all in `crates/pstore-blob/src/faulty.rs`**. Three kinds, three answers:

- **Six boundary flips** — `r < rate` → `r <= rate` in `read_fault`, `write_fault` and
  `cas_fault`, two each. They differ only when the draw lands *exactly* on the rate, which
  a seeded draw essentially never does — so no existing test could see them. The contract
  they express is real: a rate `p` fires on `[0, p)` — which is also why a rate of 0.0 can
  never fire. Pinned by a unit test that computes the first draw `r` for a seed and sets each
  rate to exactly `r` (must not fire) and to the next float above it (must fire, with the
  right error). ⚠️ A **fresh** `Faulty` per case: `set_faults` does not reset the seed, so a
  second call on one instance would draw the second output, not `r`.
- **`z ^= z >> 31` → `|=` in `split_mix`** — every output changes and nothing noticed. The
  module's own comment says the generator is written out so determinism "cannot drift";
  nothing pinned it. Pinned against SplitMix64's **published** first outputs from state 0
  (`0xE220A8397B1DCDAF`, `0x6E789E6AA1B965F4`), computed independently of this code — on the
  **53 bits** `split_mix` returns (`z >> 11`), compared with `to_bits()`, since the low 11 bits
  never leave the function.
- **`Faults::none` → `Default::default()`** — **equivalent**: `none()` *is*
  `Self::default()`. No test can kill it. Excluded by name in `.cargo/mutants.toml`'s
  `exclude_re`, with the reason beside it, under D-111's exclusion policy
  (`engineering-standards.md`: an exclusion carries its *why* and is reviewed like code).
  ⚠️ **Its premise is put under test**: `assert_eq!(Faults::none(), Faults::default())`. The
  regex keys on the *name*, so if `none()` ever stops being the default the exclusion would
  silently hide a killable mutant — this assertion is what fails instead.
  ⚠️ **Rung 1 was considered and declined**: deleting `none()` for `Faults::default()` makes
  the mutant unrepresentable, but touches 59 call sites in 7 files across three crates and
  loses the doc comment that states the decorator must be transparent at this setting.

The tests are an inline `#[cfg(test)]` module, because `split_mix` and the `*_fault`
methods are private and the contract is at that level.

**Does not change:** `faulty.rs`'s behaviour, any threshold, or any other crate.
⚠️ **Out of scope, and named rather than hidden:** the same nightly missed **172** mutants
across the six shards that finished (shards 1 and 3 were cancelled by a runner shutdown).
The other 164 are in `pstore-format`, `pstore-gossip`, `pstore-index`, `pstore-node` and
`pstore-testkit`. This milestone takes the eight in `faulty.rs`; the rest is its own decision.

## Acceptance criteria

1. `split_mix` reproduces SplitMix64's first two published outputs from state 0, on the 53
   bits it returns; and `Faults::none() == Faults::default()`, the exclusion's premise.
2. For each of the six comparisons, a rate equal to the draw does not fire and the next float
   above it does, with the matching error (`SlowDown`, `Other`, `Lost`, `Contended`).
3. `./scripts/mutants.sh --file crates/pstore-blob/src/faulty.rs` reports **0 missed**.
4. The exclusion removes exactly one mutant: `cargo mutants --list` goes from 3,401 to 3,400,
   and the one missing is `Faults::none -> Self with Default::default()`.
5. `./scripts/gates.sh` passes in the Linux dev container.

## Test plan

| # | Fails first | Mutation it catches |
|---|---|---|
| 1 | the `|=` mutant applied by hand: the bits differ | `^=` → `|=` (and any drift in the constants) |
| 2 | each `<` → `<=` applied by hand: the at-rate half fails. The above-rate half is seen red with `<` → `>` | the six boundary flips; a comparison that never fires |
| 3 | the sweep on the file before the tests: 8 missed | all of the above |
| 4 | `--list` before the exclusion | an exclusion broad enough to hide a real mutant |

## RA budget

Unchanged: test code and mutation configuration only.

## Risks

- `exclude_re` is a regex over mutant names. A rename of `Faults::none` stops it matching and
  the mutant reappears as MISSED — loud, and safe. A change to `none()`'s *body* would keep
  the name and hide a real mutant — which is what criterion 1's premise assertion catches.

## Tasks

- **M8c.1** — the tests, the exclusion, this ledger.
