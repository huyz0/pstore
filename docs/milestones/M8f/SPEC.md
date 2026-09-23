# M8f — `pstore-gossip`'s protocol: exact contracts where the tests had bounds

**Serves:** **D-111**. The nightly on `25089c7` missed **17** mutants in `pstore-gossip`
(`protocol.rs` 15, `wire.rs` 2). `cluster.rs` was in a cancelled shard and is still being
measured; if it has misses they are a later milestone.

## Delta

The existing tests assert **bounds** — "a change is carried at most 24 times in 40 rounds",
"60 probes reach at least 5 of 11 peers" — and a mutant that stays inside the bound survives.
The comments on both say mutation testing reached the code and every variant survived. Each
contract below is pinned **exactly**, by an inline `#[cfg(test)]` module in `protocol.rs`,
because the functions are private and the contracts are at that level.

- **`suspect_timeout`** (`3 * log2` → `3 + log2`): the floor `SUSPECT_TIMEOUT_MIN = 6` hides the
  difference at small fleets. Pinned at 1 member (the floor, 6) and at 2^20 members (63).
- **`piggyback`'s retirement** (`sent < RETRANSMITS` → `==`, `>`): one noted change is carried
  by **exactly** `RETRANSMITS` consecutive calls, then never again.
- **`note_update`'s replace** (`u.id != m.id` → `==`): noting A, then B, then A again leaves
  **both** pending — the mutant drops B.
- **`pick_peer`'s revisit rule** (four mutants in the `&&`/`||`/`!` of the candidate choice):
  with live and dead peers, a non-revisit tick picks only live ones and a revisit tick
  (`tick % REVISIT_DEAD_EVERY == 0`) only dead ones; with no live peer it picks a dead one on
  any tick; it never picks itself; with neither, `None`. Each over many seeds.
- **`pick_peer`'s mixing** (seven mutants in the xor/shift/multiply): extracted as
  `fn mix(seed, tick) -> u64` and pinned against values computed independently of this code.
  Nothing else about selection changes.
- **`wire::Reader::members`'s length guard** (`n > buf.len()` → `>=`, `==`) is **equivalent**
  in outcome: a count the buffer cannot hold fails on the first member it cannot read either
  way, and collecting into `Option<Vec<_>>` does not pre-reserve `n` slots — so the guard
  neither changes an answer nor prevents an allocation. **Change: the guard is deleted**
  (rung 1: the mutants cease to exist) and the property it was meant to protect is pinned —
  a frame claiming `u32::MAX` members is refused. ⚠️ That pin can fail only where a ~343 GB
  reservation fails: it aborts on Windows and on Linux with `vm.overcommit_memory=0`, and can
  succeed lazily on macOS or with overcommit=1 — where it cannot see the hazard. It is observed
  red by hand with `Vec::with_capacity(n)` inserted. ⚠️ **Amended during implementation**: the
  dev container turned out to run with `overcommit_memory=1`, so the pin *survived* there, and
  the red was observed natively on Windows instead. The deleted guard's comment is replaced by
  one saying why there is none.

⚠️ **Amended during implementation: four more, found by this milestone's own sweep** and not
in the nightly's list — each a gap of the same kind, closed the same way:
- **`mark_alive`** emptied: a suspect that sends a message must not then be buried when a
  window that started before it spoke runs out. Pinned by
  `a_suspect_that_speaks_is_not_buried_on_the_old_timer`.
- **`helpers`** returning a constant (two variants): indirect probes must go to live peers other
  than this node and the target, `INDIRECT_PROBES` of them, each once.
- **`wire::put_member`'s `if len > 0`** → `>=`: equivalent for every field a test can build,
  differing only past 4 GiB. **Restructured** so the length and the bytes it describes come
  from one `match` — the comparison no longer exists. The encoding is byte-identical.

**Does not change:** the protocol's behaviour, the wire format, any constant or threshold.
`mix` is the same arithmetic moved into a function. The existing bound tests —
`a_change_is_retransmitted_a_bounded_number_of_times` and `probes_rotate_across_peers` —
**stay**; the exact tests are added beside them, not in place of them.

## Acceptance criteria

1. `suspect_timeout(1) == 6` and `suspect_timeout(1 << 20) == 63`.
2. A noted change appears in exactly `RETRANSMITS` consecutive `piggyback` results, then none.
3. After noting A, B, A, the pending list is exactly `[B, A]` — B kept, A once.
4. With `tick` **set directly** (driving `tick()` would suspect and bury the fixture's peers)
   and over 64 seeds each: on ticks {1, 2, 3, 4, 6} only non-dead peers are picked; on ticks
   {5, 10} only dead ones; with no non-dead peer, dead ones on every tick; never itself;
   `None` when there is no one. ("Live" is `Cluster::alive()`, which includes suspects.)
5. `mix` matches values computed by an independent Python model, recorded in the test, for
   `(1, 1)`, `(42, 5)` and `(0xDEADBEEF, 3)` — **no zero seed or tick**: at `(0, 0)` all seven
   mixing mutants survive, and at `(0, 1)` one does.
6. A frame whose member count is `u32::MAX` decodes to `None`, observed red with
   `Vec::with_capacity(n)` inserted; and `grep -n 'buf.len()' crates/pstore-gossip/src/wire.rs`
   no longer shows the guard in `members`.
7. `./scripts/mutants.sh --check "pstore-gossip/src/(protocol|wire)\.rs"`: **0 missed** in those
   two files. ⚠️ Amended: `--file` was named first, and cannot run on this crate — the mutation
   config's `pstore-blob` feature flag is rejected for a test scope that includes
   `pstore-gossip`; flagged separately. `--check` tests the whole workspace instead.
8. `./scripts/gates.sh` passes in the Linux dev container.

## Test plan

| # | Fails first | Mutation it catches |
|---|---|---|
| 1 | `*` → `+`, by hand | the log-scaled timeout |
| 2 | `<` → `==` and `<` → `>`, by hand | retirement |
| 3 | `!=` → `==`, by hand | the replace dropping others |
| 4 | each of the four candidate-choice mutants, by hand | the revisit rule |
| 5 | a written-down wrong constant; `^=` → `\|=`, by hand | the mixing |
| 6 | `Vec::with_capacity(n)` inserted by hand; overcommit recorded wherever it is run | a future pre-reservation |

## RA budget

Unchanged: no blob request moves.

## Risks

- The mixing values pin `pick_peer`'s exact choice per seed. A deliberate change to the
  selection hash must update them — that is the point, and the test says so.

## Tasks

- **M8f.1** — the inline tests, `mix`, the deleted guard, this ledger.
