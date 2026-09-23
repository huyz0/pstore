# M8e — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

All runs are in the Linux dev container. Every "killed" below was a mutant applied by hand to
the tree, one at a time, and restored; each was confirmed to fail an **assertion**, not the
build.

1. **One FNV-1a, pinned to its published values** — `fnv1a_matches_the_published_values`
   (`cargo test -p pstore-node --test fnv`) asserts `""`, `"a"` and `"foobar"`. Red first: it
   did not compile before `fnv1a` existed. **Mutation verified killed**: `^=` → `|=` in
   `fnv1a`. `grep -r 0x100_0000_01b3 crates/pstore-node/src | wc -l` went from **3 to 1**, the
   remaining copy being `fnv1a` itself; `swim::derive_id`, `gossip::start` and `policy::jitter`
   call it.
2. **`Bernoulli`'s boundary** — `a_draw_equal_to_the_rate_does_not_fire_and_one_below_it_does`
   (`cargo test -p pstore-node --lib`), a fresh generator per case. **Mutation verified
   killed**: `self.draw() < p` → `<=`.
3. **`swim.rs` has no generator of its own** — `grep -nE '<< 13|>> 7|<< 17' crates/pstore-node/src/swim.rs`
   prints nothing (it printed three lines before). `total_loss_stops_the_bytes_but_not_the_node`
   still passes (`cargo test -p pstore-node --test swim`).
4. **A member reports its own zone** — `a_member_reports_its_own_zone`. **Mutation verified
   killed**: `members_zoned`'s body replaced by `vec![]`.
5. **`zone_from_env` reads `PSTORE_AZ`** — `zone_from_env_reads_pstore_az` runs its own binary
   twice, with `PSTORE_AZ=az-q` and with it removed, and requires success and `1 passed` from
   each. **Mutations verified killed**: the body replaced by `Ok("xyzzy".into())`, and the
   variable name changed to `PSTORE_ZONE`.
6. **No survivor in the five files** — `./scripts/mutants.sh --file` over
   `crates/pstore-node/src/{swim,gossip,policy,transport,lib}.rs`: 114 tested, 106 caught,
   7 unviable, **1 missed** — `Member::traffic` replaced by the constant `(1, 1, 0)` in
   `gossip.rs`, which the nightly had not reported. The chitchat path's only traffic test asked
   that sent and received be non-zero and dropped be zero, which that constant satisfies. Added
   `total_loss_stops_the_bytes_but_not_the_node` to `tests/gossip.rs`, the twin of `swim`'s;
   **mutation verified killed** by hand. Re-swept, `./scripts/mutants.sh --file crates/pstore-node/src/gossip.rs`:
   16 tested, 15 caught, 1 unviable, **0 missed**.
7. **The full gate** — `./scripts/gates.sh` in the dev container on this tree: all fifteen gates PASS.
8. **The zone defect is recorded** — open here, and carried to [`BACKLOG.md`](../BACKLOG.md)
   as row 33 (`grep -n 'Opened by M8e' docs/milestones/BACKLOG.md`): SWIM never learns a peer's zone after learning its address,
   so the peer drops out of its cell's roster. Not fixed by this milestone.
