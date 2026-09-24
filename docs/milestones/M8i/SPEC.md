# M8i — `pstore-testkit`: the injectors' own draws, edges and counters are pinned

**Serves:** **D-111**, and D-99/D-32, whose simulation and fault injection these stores are.
`pstore-testkit` does not ship but is mutated (M8d), because mutation is the only check that
its probes can tell right from wrong. The nightly on `c4b1eb9` (run 35972083065) missed **36**
of its 303 mutants; re-measured on `0337112` before any change: see VERIFIED.md, criterion 1.

## Delta

Four kinds of miss, four answers. All tests, bar the two deletions.

**Two generators nobody pinned** — `Sim::next` (6) and `Flaky::refuse`'s draw (8): every
`^`→`|`/`&` and `>>`→`<<` in the SplitMix64 finaliser survived, because the existing tests
assert *determinism* (same seed, same schedule) and *rough rates*, and a mutated mixer is
still deterministic and still roughly uniform. Pinned in inline test modules (both are
private) against SplitMix64's published outputs from state 0 (`0xE220A8397B1DCDAF`,
`0x6E789E6AA1B965F4`, as M8c) and further outputs from an independent Python model: `Sim` on
all 64 bits; `Flaky` on the 32 bits its draw keeps (`>> 32`), which is all that escapes.

**Four strict edges** — `Sim::chance`'s `< p`, `Flaky::refuse`'s `draw < rate`,
`Gated::put_conditional`'s `arrived < n` (→ `==`), and `render`'s flag
`abandoned > 0 || apc >= MAX_CAS_ATTEMPTS` (`>`→`==`, `>=`→`<`, `||`→`&&`). Pinned at the edge:
- `chance(r)` is false and `chance(r.next_up())` true, `r` the first draw; fresh `Sim` each.
- `Flaky` with `rate = d` does not refuse and `rate = d + 1` does, `d` each pinned draw — two
  instances in lockstep, because `rate` is fixed per instance and the stream is shared.
- A **lone** writer at an armed `Gated::new(2)` waits out `BARRIER_TIMEOUT` (paused clock) and
  `raced()` is then **false**. Kills `raced -> true` too: no existing test ever breaks the gate.
- `render`: a row with `abandoned = 1` at 1.0 attempts/commit is flagged; one at exactly
  `MAX_CAS_ATTEMPTS` attempts/commit with none abandoned is flagged; a clean row is not.

**Counters and accessors asserted at the one value their mutant returns** —
`Auditing::keys_written` is asserted only `== 1`, which `-> 1` also returns: two distinct
keys must count 2. `Sim::steps` (`-> 1`, `+=`→`*=`): 0 before any draw, 3 after three.
`Claims::store` (`-> Default::default()`): an object put through the `Claims` is read back
through `store()`, which is its documented purpose (a re-probe over the same objects).
`contention_point`'s two `abandoned += 1` exits (`-=`, `*=`): the `Io` exit via
`Flaky::refusing(&[1])` (ordinal 0 is the seed, so the first CAS is refused); the `Ok(None)`
exit via a test-local store whose `get_tag` answers the seed probe and then `None`. Each point
asserts `abandoned == 1` and the exit's other counters exactly.

**Forwarding and configuration that no test observed** —
- `Broken::get_range_as` / `get_suffix_as` `==`→`!=`: the `_as` paths carry the defect too.
  Only the non-`_as` paths were exercised. Asserted with `ShortReadsPastTheEnd` (truncated
  `Ok`, not an error) and `SuffixReturnsEverything` (the whole object).
- `latency_sweep`'s `latency_min`/`latency_max` and `cas_error_sweep`'s `cas_lost` field
  deletions. ⚠️ `latency_max` deleted is **invisible when `lo == hi`**: `Faulty` clamps `hi` up
  to `lo`, so the delay is `lo` either way. Pinned with a real spread `(10 ms, 11 ms)`, one
  writer, four commits, paused clock: 8 delayed calls, so elapsed is in **(80, 88] ms** —
  exactly 80 with `max` deleted, far below 80 with `min` deleted (48 for seed 5). ⚠️ Closed at
  88, found by spec review: tokio's paused clock rounds every timer up to a whole millisecond,
  so each 10.x ms delay costs exactly 11 and the correct code lands **on** 88. `cas_lost`: at rate 1.0
  nothing lands and the budget is spent (`lost == MAX_CAS_ATTEMPTS`).

**Two equivalent mutants removed at rung 1** — `Flaky::refusing_reads` sets `reads_fail_at`
and `reads_seen` to exactly what `..Self::refusing(&[])` supplies, so deleting either field
changes nothing. The two lines are deleted; no exclusion.

⚠️ **Amended during implementation: criterion 1 found 64, not 36.** The sweep on `0337112`
confirmed all 36 and found **28 more** -- the nightly's shards 1 and 3 never finished, so it
had never measured them. Four share lines with the 36 and the tests above already kill them
(`Gated` `<`→`>`, `steps -> 0`, `render`'s `>`→`<` and `>`→`>=`). The other 24:
- **17 forwarding methods replaced by a constant `Ok`** across all six doubles: `Auditing`'s
  `head` and `list_unrestricted`; `Broken`'s `get_range_as`, `get_suffix_as`, `get_immutable`;
  `Claims`' `get_range`, `get_suffix`, `get_tag`, `delete_batch`, `list_unrestricted`;
  `DepthCounting`'s and `Flaky`'s `list_unrestricted`; `Flaky`'s and `Gated`'s `get_range_as`
  and `get_immutable`. `class_forwarding.rs` unwraps the answer and checks only the class, and
  the conformance suite never calls these. One generic check, run over all six doubles, puts
  one object and asserts every read, the listing and the delete answer with the store's bytes.
- **4 conformance guards replaced by `true`** -- `ranged_read`'s bytes, `coalesced_read`'s
  slice lengths, `suffix_read`'s oversized length, `get_tag`'s match. ⚠️ The serious ones: a
  probe whose comparison is `true` reports `Supported` for a backend that answers wrongly, and
  telling right from wrong is why this crate is mutated at all. `Broken`'s defects fail each
  probe on something coarser, so none reached the comparison. A test-local lying store, one
  lie per probe (a range off by one byte, slices one byte long, a suffix clamped at 8, a put
  returning a tag from before a hidden second write), must be `Divergent` on that probe.
- **`Gated::arm -> ()`**: killed by the lone-writer test, which an unarmed gate does not hold.
- **`Sim::seed -> 0` and `-> 1`**: `seed()` after a draw returns `0xDEADBEEF`.

**Does not change:** any injector's behaviour, draw, rate or timeout; any threshold; any
other crate.
⚠️ **Out of scope, named:** the same nightly missed **43** in `pstore-index`, **4** in
`pstore-format` and **1** in `pstore-gossip/src/cluster.rs`; shards 1 and 3 were killed by a
runner shutdown signal, as on `25089c7`, so part of the sweep is unmeasured.

## Acceptance criteria

1. Measured on `0337112` before any change, the testkit sweep's missed list is recorded, and
   every miss is one of the 36 above or is named and handled here (the 28 of the amendment).
2. `Sim::next` equals SplitMix64 from seed 0 on all 64 bits for its first 8 outputs; `steps()`
   is 0, then 3 after three draws; `chance(r)` false and `chance(r.next_up())` true.
3. `Flaky` from seed 0 refuses at `rate = d + 1` and not at `rate = d` for each of the first 8
   draws `d` of the independent model's `>> 32`.
4. A lone writer at an armed `Gated::new(2)` completes after the timeout and `raced()` is false.
5. `render` flags the two budget rows and not the clean one.
6. `keys_written() == 2` after two distinct keys; `Claims::store()` reads what `Claims` wrote.
7. `contention_point` counts `abandoned == 1` on each of its `Io` and `Ok(None)` exits.
8. `Broken`'s `_as` reads show `ShortReadsPastTheEnd` and `SuffixReturnsEverything`.
9. `latency_sweep` at `(10, 11)` ms: elapsed in (80, 88] ms; `cas_error_sweep` at 1.0: no
   commit, `lost == MAX_CAS_ATTEMPTS`.
10. Each of the six doubles answers `head`, `get_tag`, both `_as` reads, `get_immutable`,
    `list_unrestricted` and `delete_batch` with the store's own answer.
11. Each of the four lies is `Divergent` on its own probe; `Sim::seed()` survives a draw.
12. `refusing_reads` has no `reads_fail_at` or `reads_seen` line; its tests still pass.
13. `./scripts/mutants.sh --check pstore-testkit --file 'crates/pstore-testkit/src/*.rs'`:
    **0 missed**.
14. `./scripts/gates.sh` passes.

## Test plan

| # | Fails first | Mutation it catches |
|---|---|---|
| 2 | each of `Sim::next`'s 6 finaliser mutants, `steps -> 1`, `+=`→`*=`, `<`→`<=`, by hand | the generator, the counter, the edge |
| 3 | each of `refuse`'s 8 mixing mutants and `<`→`<=`, by hand | the draw and the edge |
| 4 | `raced -> true`; `<`→`==` | a gate that never breaks; one that never holds |
| 5 | the three `render` mutants | the row flag |
| 6 | `keys_written -> 1`; `store -> Default::default()` | the denominator; the re-probe |
| 7 | `+=`→`-=` and `*=` on each exit | the abandoned count |
| 8 | `==`→`!=` on each `_as` path | a defect that skips the class-carrying reads |
| 9 | each of the three field deletions | a sweep that runs a configuration it does not record |
| 10 | each of the 17 constant-`Ok` mutants | a double that stops forwarding |
| 11 | each guard → `true`; `seed -> 0`, `-> 1` | a probe that does not compare |
| 13 | the sweep before the tests: criterion 1's list | all of the above |

## RA budget

Unchanged: test code, and two redundant field initialisers deleted in a test double.

## Risks

- The pinned draws are only as independent as the model. Python's `int` arithmetic, masked to
  64 bits, is written from the published algorithm, not from this code, and its first two
  outputs must match the published ones before the rest are trusted.
- The latency bound rests on tokio's paused clock rounding each timer up to a whole
  millisecond (`runtime/time/source.rs`); a tokio that stopped rounding would land inside the
  bound anyway, since the unrounded total is in [80, 88).
- The latency bound assumes 8 delayed calls. A change to how many calls `contention_point`
  makes moves it — loudly, as a failure, not silently.

## Tasks

- **M8i.1** — the tests, the two deletions, this ledger.
