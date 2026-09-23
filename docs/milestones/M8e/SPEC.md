# M8e — `pstore-node`'s library: one generator, one hash, and the rest pinned

**Serves:** **D-111**. The nightly on `25089c7` missed **25** mutants in `pstore-node`'s library
(`swim.rs` 20, `gossip.rs` 2, `policy.rs` 2, `transport.rs` 1) — a lower bound, since two
shards were cancelled; none of the unmeasured files is in this crate.

## Delta

Most of the 25 are not missing tests. They are **duplicated code** that nothing tests twice:

- **Two loss generators.** `transport.rs` has `Bernoulli`, a seeded xorshift whose comment
  records *why* it seeds through splitmix64: `seed | CONST` is lossy, and measured seeds 42 and
  43 produced byte-identical drop sequences. `swim.rs`'s `send_all` has a hand-rolled copy of
  the same xorshift, seeded with `seed | 1`. Its **14** mutants (nine in the three xorshift
  steps, four in `seed | 1` across two tasks, one in `draw < loss`) have no test. **Change:**
  `swim.rs` uses `transport::Bernoulli`, with the struct, `new` and `fires` made `pub(crate)`.
  Rung 1 of `gate-design`: the duplicate is deleted rather than tested twice. (SWIM's seed is a
  per-address hash, so `| 1` merges only seeds differing in bit 0 — the reason is the
  duplication, not the collision.)
  ⚠️ **Behaviour changes, deliberately:** a seed now drops a *different* sequence of SWIM
  datagrams. The rate is unchanged. **M4c**'s `VERIFIED.md` criterion 6 measured SWIM under
  10% loss (31 periods; 100 of 100 stable) with the old sequence; those outcomes are **not**
  re-measured or claimed to carry over here. No test pins a SWIM drop sequence —
  `tests/swim.rs` uses loss 0.0 and 1.0 only. The receiver and ticker stay seeded identically.
- **Three FNV-1a hashes.** `swim::derive_id`, `gossip::start`'s seed and `policy::jitter` each
  write out FNV-1a 64. `jitter`'s copy is tested; the other two are not. **Change:** one
  `pub fn fnv1a(bytes) -> u64` in the crate root, used by all three. The algorithm is identical,
  so no hash value changes. Pinned against FNV-1a's published values (`""`, `"a"`, `"foobar"`).
- **`Bernoulli::fires`'s boundary**, `u < p` → `u <= p`. The same shape as M8c: a rate `p` fires
  on `[0, p)`. Pinned by computing the first draw `u` for a seed and setting `p` to exactly `u`
  (must not fire) and to the next float above it (must fire).
- **`Member::members_zoned`** (5 mutants replacing its result). Nothing asserts a zone.
  Pinned through the member's **own** entry: a fresh member in `az-q` includes
  `(its address, "az-q")` — deterministic, no gossip, no polling.
  ⚠️ **Why not two members, and what that found.** Spec review simulated two members in
  different zones: after 200 rounds **neither learns the other's zone**. A peer first learned
  from a seed, a dial or a bare probe is stored with an empty zone, and its own record (same
  incarnation, Alive) never outranks that entry, so it is never replaced — and the two views'
  fingerprints never agree, so they exchange Sync datagrams every round forever. `main.rs`
  keeps only same-zone peers when it publishes a cell's roster, so such peers are **dropped
  from the roster** until an incarnation bump. That is a production defect in
  `pstore-gossip`'s `absorb`, and fixing it is its own milestone — not this one. It is
  recorded as open in this milestone's `VERIFIED.md` and carried to `BACKLOG.md`.
- **`policy::zone_from_env`** (2 mutants returning `Ok(..)`). The decision it wraps, `zone`, is
  tested; the wrapper is not, because setting `PSTORE_AZ` in-process is `unsafe` in this
  edition and `unsafe_code = "forbid"`. Pinned **without** setting it in-process: the test
  re-runs its own binary as two children, marked by a dedicated role variable: one with
  `Command::env("PSTORE_AZ", "az-q")` asserting `Ok("az-q")`, one with
  `Command::env_remove("PSTORE_AZ")` asserting `Err` — both safe, and neither dependent on
  whatever the parent's environment holds. The parent requires each child's
  `status.success()` **and** `1 passed` in its output: libtest prints a test's name on failure
  too, so the name alone proves nothing. A wrapper that read the wrong variable fails, not only
  one that returned a constant.

**Does not change:** the gossip protocol, `transport::Metered`'s drop sequence, any hash value,
any threshold. `pstore-node/src/main.rs` is not mutated since M8d.

## Acceptance criteria

1. `fnv1a` returns FNV-1a 64's published values for `""`, `"a"` and `"foobar"`, and
   `swim::derive_id`, `gossip::start` and `policy::jitter` call it (no other FNV loop remains
   in the crate: `grep -r 0x100_0000_01b3 crates/pstore-node/src | wc -l` goes from 3 to 1).
2. `Bernoulli` at a rate equal to its first draw does not fire, and one float above does.
3. `swim.rs` contains no generator of its own: `grep -nE '<< 13|>> 7|<< 17' crates/pstore-node/src/swim.rs`
   prints nothing, and the existing `total_loss_stops_the_bytes_but_not_the_node` still passes.
4. A member started in `az-q` includes `(its advertised address, "az-q")` in `members_zoned`.
   (`contains`, not equality: a reused test port can deliver a stray probe, and containment
   still kills all five mutants.)
5. A child with `PSTORE_AZ=az-q` gets `Ok("az-q")` from `zone_from_env()`, a child with it
   removed gets `Err`; the parent requires success and `1 passed` from each.
6. `./scripts/mutants.sh --file` over `crates/pstore-node/src/{swim,gossip,policy,transport,lib}.rs`:
   **0 missed**.
7. `./scripts/gates.sh` passes in the Linux dev container.
8. The zone-propagation defect is recorded as open in `VERIFIED.md` and as a `BACKLOG.md` row.

## Test plan

| # | Fails first | Mutation it catches |
|---|---|---|
| 1 | written before `fnv1a` exists: does not compile, then a wrong constant by hand | `^=` → `\|=`/`&=` in every copy at once |
| 2 | `<` → `<=` in `fires`, by hand | the boundary flip |
| 3 | the grep before the change prints three lines | a generator reintroduced in `swim.rs` |
| 4 | `members_zoned` returning `vec![]`, by hand | the five result replacements |
| 5 | `zone_from_env` returning `Ok("xyzzy")`, and reading `PSTORE_ZONE`, by hand | both `Ok(..)` replacements; the wrong variable |

## RA budget

Unchanged: no blob request moves.

## Risks

- The child-process test depends on libtest's `--exact` name filter; a rename that forgets the
  argument runs zero tests and exits 0 — which the parent's `1 passed` requirement refuses.

## Tasks

- **M8e.1** — `fnv1a`, `Bernoulli` shared, the tests, this ledger.
