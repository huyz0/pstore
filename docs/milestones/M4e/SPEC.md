# M4e — Per-AZ cells, and gray failure

**Serves:** **D-79** (each AZ runs its own placement ring), **D-84–D-87** (gray failure), and
**OQ-59** — *"balance cost of AZ-aware LRH"*.

**Depends on:** [M4a](../M4a/SPEC.md) (roster, placement) and [M4c](../M4c/SPEC.md)
(membership).

## ⚠️ Scope: three phases, and phase 1 is not what it first looked like

A first draft made `Cell` the roster **address** and stopped. Review found that incoherent, and
the reason is the shape of the whole milestone:

> A cell's roster *address* being per-AZ is worthless while its *contents* come from the
> fleet-wide gossip view. A node in AZ-a would write AZ-b's members into AZ-a's roster and
> place across AZs anyway — with every test in the plan passing.

And it could not be fixed as drafted: `pstore_gossip::Member` carries id, addr, incarnation and
state, and **no zone**. A node cannot filter peers it cannot identify. So:

| Phase | What | State |
|---|---|---|
| **1 — this spec** | Zone identity: members carry a zone, cells are addressable without collision, a cell's roster holds only its own members | specified |
| **2** | Per-AZ placement and OQ-59, once a cell's membership is real | specified below |
| 3 | Gray failure (D-82–D-87): probe mesh, blob bulletin, peer-relative detection, self-eviction | named only |

## ⚠️ A pre-existing collision, found by this review

`roster.rs:36` addresses a roster as `format!("{:04x}/clu/ROSTER", fnv(cluster) as u16)`. The
**cluster name never appears in the path** — sixteen bits of hash are the entire identity, so
two clusters colliding in 65,536 share one roster object and one ring. `head.rs:55` does it
correctly: `{:04x}/tnt/{}/HEAD`, hash prefix *and* the id.

This is a bug in M4a, not a consequence of cells; cells make it worse by tripling the
population drawn against the same 65,536 buckets. Fixing it here rekeys existing rosters,
which is free at this stage — there is no deployment — and is said rather than discovered.

## Delta

**Adds**
- `Zone`, carried on `pstore_gossip::Member` and on the wire, so a node can tell which cell a
  peer belongs to. ⚠️ This is the thing phases 2 and 3 both need and neither can fake.
- `Cell { cluster, zone }`, and `Roster::{key, read, refold}` taking one. The key carries both
  in the **path**, not only in a hash prefix.
- `pstore_node::policy::zone_from_env` — required, no default. ⚠️ A node with no zone would
  join the wrong cell; a default here is the bug. In the **library**, not `main.rs`, because
  the coverage gate excludes binary entry points and an untestable guard is not a guard.
- `Roster::from_members`, which builds a cell's roster from a gossip view **filtered to that
  cell**.

**Does not add** — per-AZ placement and OQ-59 (phase 2: they need this first, and OQ-59 needs
a measurement design this spec does not have); gray failure (phase 3); cross-AZ gossip relays,
which C-8 leaves open; multi-region (OQ-135).

## Phase 2 — placement within the cell, and what that costs

Phase 1 made a cell's *roster* hold only its own members. Placement then follows — but only
where the caller uses the cell's roster, and `main.rs:302` still builds one from the whole
gossip view for its own reporting. A per-cell roster that some call sites bypass is the same
bug in a smaller place.

### ⚠️ OQ-59, redesigned after the first attempt was shown unmeasurable

The first draft asked for the imbalance of one 900-node ring against three 300-node rings, and
review measured the effect at **0.026** on the mean against a spread of **0.28** across
node-naming trials — an order of magnitude smaller than the noise. It also moved two variables
at once: the AZ constraint *and* per-ring `N`, and therefore `window()`, which pulls the other
way.

So the question is split, because the answer is:

* **At fixed ring size, constraining to an AZ costs nothing.** A 300-node cell and a 300-node
  global ring are the same ring; there is no AZ term in `place`.
* **And splitting a fleet costs nothing measurable either** — ⚠️ which is the *opposite* of
  what this section assumed before it was measured. `window(n) = max(32, 3√n)` covers ~32% of
  a 100-node ring but only ~10% of a 900-node one, and a relatively wider window balances
  better: it offsets the law of large numbers rather than compounding with it. The correction
  is left visible because the wrong intuition is why the first criterion was unsatisfiable.

That makes OQ-59 a measurement of imbalance against `N`, with enough trials to see past the
spread — not a single ratio between two configurations.

## Acceptance criteria

1. ⚠️ **Cell addresses do not collide.** Over **10,000** generated `(cluster, zone)` pairs,
   every key is distinct. The current scheme fails this — sixteen bits of hash with the names
   absent — and the test is written against a generated set, not two hand-picked inequalities
   that cannot see a collision.
2. **A zone round-trips through gossip.** `Member`'s zone survives encode/decode, and a frame
   whose zone is absent or malformed is **refused**, not defaulted — a guessed zone puts a
   node in the wrong cell.
3. ⚠️ **A cell's roster contains only that cell's members.** Given a gossip view mixing three
   zones, `Roster::from_members` for zone *a* yields exactly the zone-*a* members. This is the
   criterion the first draft lacked, and the one that catches the bug it would have shipped.
4. **A node without a zone refuses to start**, as a library function returning an error —
   testable, and inside the coverage gate.
5. **No new blob requests.** Building a cell's roster from a view is a pure function: **0**
   requests, asserted by the counter.
6. **Placement never leaves the cell**, end to end: with a gossip view spanning three zones,
   every node a nodeplaces on is in its own zone — asserted over 1,000 keys, and including the
   node's own `OWNS` reporting path, which built its ring from the unfiltered view.
7. ⚠️ **OQ-59 answered as a curve, with its spread.** Imbalance (max node load ÷ mean) over
   100,000 keys at **N = 100, 300, 900**, each over **≥8 node-naming trials**, reporting mean
   and range. The committed conclusion is which of the two terms — the constraint or the ring
   size — the cost belongs to. ⚠️ `provisional`: measured on WSL2.
8. **A regression guard, set from the measurement rather than before it**: imbalance at N=300
   stays under **1.6×**, which is above the observed maximum and below anything a broken hash
   would produce. ⚠️ Not 1.25×: that was M4a's bound at **N=100**, and requiring it at 300 is
   what made the first draft of this criterion unsatisfiable.
9. Region coverage ≥95% on shipped crates, mutation ≥80%, full gate set green.

## Test plan

| # | Test that must fail first | Mutation it catches |
|---|---|---|
| 1 | `cell_keys_do_not_collide_across_ten_thousand_cells` | a key that hashes to 16 bits and drops the names — today's behaviour |
| 1 | `a_cell_key_names_both_its_cluster_and_its_zone` | a key that includes one and not the other |
| 2 | `a_zone_survives_the_wire` | a decoder that drops the zone and leaves an empty string |
| 2 | `a_frame_without_a_zone_is_refused` | a default zone, which silently merges every cell |
| 3 | `a_cells_roster_holds_only_its_own_members` | a filter that returns the whole view |
| 3 | `a_cell_with_no_members_is_empty_not_everyone` | an empty-filter fallback that widens to the fleet |
| 4 | `a_node_without_a_zone_refuses_to_start` | a default zone in the node |
| 6 | `placement_stays_inside_its_cell` | a call site that builds its ring from the unfiltered view |
| 8 | `imbalance_at_cell_scale_stays_bounded` | a hash that clusters at the smaller per-cell node count |

## RA budget

| Operation | W | Rseq | Rpar | List | Depth |
|---|---|---|---|---|---|
| Build a cell roster from a view | 0 | **0** | 0 | 0 | **0** — a pure function |
| Roster read (join, refresh) | 0 | 1 | 0 | 0 | 1 — unchanged, but **per cell** |
| Gossip | — | — | — | — | unchanged: still fleet-wide |

⚠️ Gossip stays fleet-wide, so **cross-AZ bytes after this milestone are non-zero**. Saying
"0 by construction" would contradict `az-topology.md` §4, which puts zone-sharded gossip at
"a few cross-AZ relays — tiny", i.e. small and not zero. The query path that would cross an AZ
does not exist yet; relays are C-8's open item.

## Risks

- ⚠️ **A wider wire format is a compatibility event.** Adding a zone to `Member` changes every
  datagram. There is no deployment to break, and the decoder refusing an absent zone
  (criterion 2) is what stops a mixed fleet silently agreeing on the wrong thing.
- ⚠️ **Zone is self-declared.** A node claiming the wrong zone joins the wrong cell, and
  nothing here can detect it — the blob store has no zone truth to check against. Named
  because it is a real limit, and it is what D-83's bulletin would later cross-check.
- **Rekeying rosters** invalidates any existing object. Free now; would not be later.

## Tasks

| ID | Task |
|---|---|
| M4e.1 | Fix the roster key: names in the path, not only a hash prefix |
| M4e.2 | `Zone` on `Member`, on the wire, and refused when absent |
| M4e.3 | `Cell`, and `Roster::{key, read, refold}` taking one |
| M4e.4 | `Roster::from_members`, filtered to a cell |
| M4e.5 | `zone_from_env` in the node library, required |
| M4e.6 | Every placement call site uses the cell's roster, including `OWNS` |
| M4e.7 | OQ-59: the imbalance curve, its spread, and its conclusion |
