# M66 — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ **Where this ran:** a Linux x86-64 cloud container with 4 cores. No latency is claimed.

Commands:
- `cargo test -p pstore-format --test geo`: 2 tests.
- `cargo test -p pstore-query --test geo`: 2 tests.
- `cargo test -p pstore-server --test geo`: 3 tests.

1. **The distance.** `distance_is_haversine_on_the_sphere`: Paris to London within 1 m of the
   test's own formula; the same point 0; antipodes half the circumference; across the
   antimeridian the short way; a pole independent of longitude; symmetric.
   `location_is_a_finite_in_range_pair`: ints accepted, bounds inclusive, 12 non-locations
   refused.
2. **Filters.** `geo_clauses_admit_what_they_define`: a radius inclusive at exactly its
   distance and not a meter short; a box's edges; the antimeridian box admits both sides and
   nothing between; `west == east` is one meridian; seven non-locations are false and `Not`
   admits them.
3. **Zones and encoding.** `geo_clauses_prune_nothing_and_round_trip`: `could_admit` true, an
   `And` with a numeric clause still prunes, every shape round-trips and every truncation
   decodes to `None`.
4. **Through the API.** `geo_filters_through_the_api`: 190 live rows (200 folded in two
   segments, ten moved and twenty deleted unfolded); a ranked query, an order and an
   aggregation return exactly the brute-force haversine's rows for a radius, a box, a box
   across the antimeridian, an `And` and a `Not`.
5. **The geo decay.** `geo_decay_scores_by_distance`: boosted score less unboosted equals the
   test's formula for `exp`, `gauss` and `linear`, per row, within `1e-4`; at exactly `scale`
   meters every shape adds one half; a row with no location adds nothing.
6. **Refusals.** `geo_clauses_are_refused_at_the_door`: 13 filter refusals, each naming the
   clause (or the reserved name), 4 boost refusals naming `origin` or `scale`, and the limits
   accepted.
7. **Gates.**
   - `./scripts/gates.sh`: green by the pre-commit hook on this commit.
   - **Mutation:** `cargo mutants --in-diff` over the changed lines, `--no-config`, each
     against its crate's tests:
     - `pstore-format/src/geo.rs` (`--test geo`): 41 mutants, 40 caught, **1 missed**:
       `number`'s match guard `f.is_finite()` as `true`. ⚠️ Equivalent: `location`'s range
       check rejects NaN and ±∞ itself.
     - `pstore-query` `filter.rs` and `condition.rs` (`--lib --test geo`): 21 mutants, 19
       caught, 1 unviable, **1 missed**: `GeoBox`'s `west > east` as `>=`, which made
       `west == east` the whole world. Killed by the `west == east` case added to
       `geo_clauses_admit_what_they_define` (seen red by applying the mutant).
     - `pstore-server` `lib.rs` and `boost.rs` (`--test geo --test boost`): 33 mutants, 31
       caught, 2 unviable, 0 missed.

**Code review**, one round: no blocking or major findings. **Noted, minor:** `-180` and `180`
are the same meridian but different numbers to a box, and `west == east` is one meridian; the
docs say both. A geo decay's `scale` is in meters whatever the term's other uses.

**Not covered**, as the spec states: sorting by distance, polygons, cell indexes, an
ellipsoidal distance (the sphere's error is up to about 0.5%).
