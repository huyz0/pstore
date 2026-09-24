# M8h — `pstore-cluster`: the gray-failure detector's edges and placement's dead code

**Serves:** **D-111**, and D-84/D-85, whose detector these guards are. A clean sweep
(`./scripts/mutants.sh --check "^crates/pstore-cluster/src/"`, 144 mutants, 14 min) missed **15**
in `pstore-cluster`: 11 in `gray.rs`, 4 in `placement.rs`.

## Delta

**`gray.rs` — every miss is a boundary or a branch the tests never reach.**
- **`median`** (7 mutants): tests reach it only through three-zone fleets, whose peer baseline
  is always two values, so the odd branch, the index arithmetic and the midpoint are never told
  apart. Pinned directly, in an inline test module (it is private): `[3,1,2] → 2`,
  `[4,1,3,2] → 2.5`, `[5] → 5`, `[5,1,4,2,3] → 3`, `[] → 0`.
- **`outliers`' `samples.len() < 3`** → `>`: every test uses exactly three zones. Pinned: a
  four-zone fleet with one degraded zone names it; a two-zone fleet **with one clearly degraded
  zone** names nobody — otherwise "names nobody" would pass with the guard removed.
- **The three edge comparisons** — `latency > peer * (1 + margin)`, `success < peer - margin`,
  survivor `utilization < SURVIVOR_HEADROOM` — each `→` its non-strict twin. Pinned **at the
  edge**, with values chosen so the arithmetic is exact: a zone exactly at the latency
  threshold (`100 × 1.5 = 150`) is not an outlier; one exactly `0.05` below equal peers of
  `0.999` (`0.949`, exact in f64; `SUCCESS_MARGIN` is private, so the literal is repeated) is
  not; a survivor exactly at `0.80` blocks a drain (one bad zone of three, utilization 0.20, so
  the survivor check is reached).

**`placement.rs` — two equivalent mutants removed, one excluded with its reason, one real gap.**
- **`% ring.len()` on `start`** → `+`: equivalent, because `start` is used only as
  `(start + i) % ring.len()`. Rung 1: the redundant `%` is deleted, and the "wrapping to 0"
  comment moves to where the wrap now happens.
- **`if out.len() < r.min(window)`** → `<=`: equivalent, because the loop inside re-checks the
  same bound and breaks at once. Rung 1: the redundant outer `if` is deleted.
- **`*p < pos`** → `<=` in the ring search: differs only when a key's 64-bit hash **equals** a
  node's ring position exactly. ⚠️ **Not provably unreachable** — spec review showed FNV-1a
  collides between ordinary UTF-8 strings, so such a key exists and a ~2^32-evaluation birthday
  search could construct one; a first draft of this spec claimed otherwise. **Excluded by
  name, with the reason stated honestly**: the difference needs a deliberately constructed
  collision, and what a test would pin — which node a key sitting exactly on a ring position
  starts its window at — is a tie-break no real key reaches, bought with a 2^32 search and a
  roster over 32 nodes. Declined in favour of an exclusion, as M8c declined rung 1. The
  regex names the mutant's line and column, so an edit that moves it makes it reappear as
  MISSED rather than hiding a new `<`→`<=` in `place`.
- **`hash`'s last finaliser step** (`h ^= h >> 33` → `|=`, line 156 — **not** the part
  separator at line 138, which the golden placement test already catches): it changes only the
  low 31 bits, and every comparison `place` makes is decided by the high bits, which is why the
  golden test passes under it. Pinned against an independent model for three inputs, compared
  on **all 64 bits**.

⚠️ **Amended during implementation: a third redundancy, created by the first deletion.** The
confirming sweep found `if ring.is_empty() || r == 0` → `&&` surviving. It had been caught only
because an empty ring reached the deleted `% ring.len()` and divided by zero; without that, an
empty ring falls out as an empty list (`window(0)` is 0) and so does `r == 0`, so the guard is
redundant too. Deleted, with `place(key, 0)` now asserted empty beside the existing empty-ring
assertion. The ring search moved to line 77, so the exclusion names `77:54`.

**Does not change:** any verdict, placement or hash value; any threshold.

## Acceptance criteria

1. `median` returns 2, 2.5, 5, 3 and 0 for the five inputs above.
2. A four-zone fleet with one degraded zone names it; a two-zone fleet names none.
3. At each of the three edges, the edge value does not flip the outcome, and one step past it
   does.
4. `place` has no `% ring.len()` on `start`, no outer `if` around the relax loop and no early
   return for an empty ring or `r == 0`, and every existing placement test passes.
5. `hash` matches an independent model on all 64 bits for `["ring","n1"]`,
   `["idx0/s0","10.0.0.1:7946"]`, `["a"]`.
6. The ring-search mutant is excluded by one `exclude_re` line naming its line and column;
   measured on the same code with and without that line, `cargo mutants --list` differs by
   exactly that mutant.
7. `./scripts/mutants.sh --check "^crates/pstore-cluster/src/"`: **0 missed** in `pstore-cluster`.
8. `./scripts/gates.sh` passes in the dev container.

## Test plan

| # | Fails first | Mutation it catches |
|---|---|---|
| 1 | each of the seven `median` mutants, by hand | the arithmetic |
| 2 | `<` → `>`, by hand | the three-zone floor |
| 3 | each non-strict twin, by hand; the one-step-past half with the comparison inverted | the edges |
| 5 | the last `h ^= h >> 33` → `\|=`, by hand | the finaliser's low bits |
| 6 | the list without the exclusion line | an exclusion broader than one mutant |

## RA budget

Unchanged: tests, three deleted redundancies, one exclusion.

## Risks

- The edge tests depend on exact float arithmetic; values are chosen so the thresholds are
  exact (`100 * 1.5 = 150`, and the success edge is computed by the same expression the code
  uses).

## Tasks

- **M8h.1** — the tests, the three deletions, the exclusion, this ledger.
