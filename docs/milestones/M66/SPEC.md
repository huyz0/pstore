# M66 — Geo: radius and box filters, and a distance decay

**Serves:** backlog row 70, from the project owner's comparison of 2026-10-08. Elasticsearch's
`geo_distance`/`geo_bounding_box`, Qdrant's `geo_radius`/`geo_bounding_box`, Milvus's geo
fields and Vespa's `geoLocation` filter by place and rank by nearness. pstore has no notion of
a location, so a "near me" search is a client-side post-filter over a page.

## What is true today

- An attribute is a `Value`: int, string, float, bool, datetime, or an **array of scalars**
  (M9h.2). A pair `[48.85, 2.35]` is already storable and returned as written.
- A filter is a `Predicate` (`pstore-query/src/filter.rs`), evaluated per row by `holds`,
  pruned per block by `could_admit` (zone maps), and serialized by `condition.rs` (tags 0-9,
  for the write-side conditions that carry predicates).
- A boost (M62) has `decay` with a numeric or datetime origin; `Origin` is the origin's type.

## Delta

**A location is a two-number array `[lat, lon]` in degrees, under any attribute.** No format
change: a row with the pair already exists. Two filter clauses read it, and the boost's
`decay` gains a place for an origin.

1. **A location.** An attribute value is a location when it is an `Array` of exactly two
   numbers (`Int` or `Float`), finite, `lat` in −90..=90 and `lon` in −180..=180. Anything else
   -- absent, a scalar, a longer array, NaN-free but out of range -- is **no location**, and
   every geo clause is false for it. (A write is not refused for a malformed pair: the
   attribute is free-form, as a `Tokens` clause's non-string is.)
2. **`GeoRadius`:** `["loc", "GeoRadius", [lat, lon, meters]]`. Admits a row whose location is
   within `meters` of the point, by the great-circle (haversine) distance on a sphere of
   radius **6,371,008.8 m**, **inclusive** (`<=`).
3. **`GeoBox`:** `["loc", "GeoBox", [south, west, north, east]]`. Admits `south <= lat <=
   north` and, for `west <= east`, `west <= lon <= east`; for `west > east` the box crosses
   the antimeridian and admits `lon >= west || lon <= east`. Edges are inclusive.
4. **Refused `400` at the door,** each message naming the clause:
   - a point `lat`/`lon` outside its range or not a number; `meters` not a finite number in
     `0..=20,015,087` (half the circumference);
   - a box with `south > north`, a coordinate out of range, or not four numbers;
   - the clause's argument not an array of the right length;
   - everything else about a filter as today (`$` names, nesting, `Not` and `And`/`Or`).
5. **Cost.** `could_admit` is `true`: a location has no zone map, so a geo clause prunes no
   block, as a `Tokens` clause prunes none. Under an `And` with a prunable clause the other
   clause still prunes. The docs say a geo clause is a scan over the blocks the rest of the
   filter admits. Distance is computed in `f64` per row; no blob request is added.
6. **Condition encoding:** tags 10 (`GeoRadius`: name, `lat`, `lon`, `meters` as `f64` bits)
   and 11 (`GeoBox`: name and four `f64`). `decode` returns `None` for a truncated one, as
   every tag does, and a decoded predicate equals the encoded.
7. **A geo decay.** A boost term `{"attr": "loc", "fn": "decay", "origin": [lat, lon],
   "scale": meters, "shape": "exp"|"gauss"|"linear", "weight": w}`: `origin` as a two-number
   array selects the geo form, and `scale` is in **meters**. The decay is M62's with
   `d = haversine(origin, location)` in meters. A row with no location contributes nothing.
   `origin` as a pair outside the ranges is refused, naming `origin`.
8. **One distance function** in `pstore-format` (`geo::distance_m`, `geo::location`) used by
   the filter and the boost, so the two cannot disagree.

**Not changed:** the format, the ranking, and every query without a geo clause.

**Not covered:** sorting by distance (`rank_by` over a geo origin), geo-polygon filters, geo
indexes (S2/H3 cell pruning), altitude, an ellipsoidal (WGS-84) distance -- the sphere's
error is up to 0.5%, and the docs say so.

## Acceptance criteria

1. **The distance.** `distance_is_haversine_on_the_sphere`, in
   `crates/pstore-format/tests/geo.rs`: by hand, Paris to London within 1 m of the value
   computed from the formula in `f64` in the test, the same point is 0, antipodes are half
   the circumference, a pair across the antimeridian is the short way, and a pole is
   independent of longitude. `location_is_a_finite_in_range_pair` covers the rule of 1.
2. **Filters.** `geo_clauses_admit_what_they_define`, in `crates/pstore-query/tests/geo.rs`:
   a radius is inclusive at exactly its distance and excludes 1 m beyond; a box includes its
   edges; a box crossing the antimeridian admits both sides and nothing between; rows with no
   location, one number, three numbers, a string, an out-of-range pair, absent, are false,
   and `Not` of a geo clause admits them (two-valued, as every non-token predicate).
3. **Zones and encoding.** `geo_clauses_prune_nothing_and_round_trip`: `could_admit` is true
   for any zones; an `And` with a numeric clause still prunes; every geo predicate encodes
   and decodes equal, and a truncated encoding decodes to `None`.
4. **Through the API.** `geo_filters_through_the_api`, in
   `crates/pstore-server/tests/geo.rs`: against 200 folded rows plus unfolded upserts and
   deletes, a ranked query, an `aggregate_by` and a `rank_by` order each return what a
   brute-force `f64` haversine over the written points gives, for a radius, a box, a box
   across the antimeridian, and `And` with another clause.
5. **The geo decay.** `geo_decay_scores_by_distance`: for each shape, the boosted scores equal
   the test's own formula over the written points; a row with no location is unchanged; the
   decay at `scale` meters is the shape's value at `d = scale`.
6. **Refusals.** `geo_clauses_are_refused_at_the_door`: every `400` of rule 4 and rule 7, each
   naming the clause, and the limits accepted.
7. **Gates:** `./scripts/gates.sh` passes; the mutation sweep of the changed lines misses 0,
   or each miss is killed by a named test or is equivalent.

## Test plan

| AC | Seen red first under |
|---|---|
| 1 | `geo` absent (compile) |
| 2, 4 | the clause as no-clause (admits all); `<` for `<=`; box edges exclusive |
| 2 | west/east swapped; the antimeridian case as a plain interval |
| 3 | pruning on a geo clause (a block dropped) |
| 5 | the distance as degrees; `scale` in kilometers |
| 6 | each refusal removed |

## RA budget

No request is added: geo clauses read attributes the row fetch already returns. CPU: one
haversine per candidate row.

## Tasks

- **M66.1** `pstore-format::geo`; `Predicate::GeoRadius/GeoBox`, `holds`, `could_admit`,
  `condition.rs` tags; the filter parser; the boost's geo origin; the docs; AC1-AC7.
