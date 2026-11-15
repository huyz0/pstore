# M236: one HEAD, one lane probe, for every leg of a request

## Serves
The C/F row of [DELIVERY.md](../../comparisons/DELIVERY.md): "lane tails are still probed per
leg, and legs at mixed levels or `as_of` run apart", the rest of [M131](../M131/SPEC.md). The
plan's F budget: "acquire a snapshot once, fan out legs within rounds". D-34 unchanged.

## Architecture proof
A snapshot taken at level X may serve a leg at level Y when it keeps Y's promise:
- `strong` (a fresh HEAD, the lane registry, and a probe that no lane holds an unfolded bundle)
  keeps every promise: `eventual` asks for a HEAD read now and this process's writes, which it
  is; `bounded(d)` asks for a HEAD no older than `d`, and it is 0 old.
- `eventual` (a fresh HEAD) keeps `bounded(d)`'s for every `d`.
- `bounded(a)` keeps `bounded(b)` when `a <= b`: the cached HEAD is at most `a` old.
These are a total order, so a request's strongest live level keeps every live leg's promise.
The unfolded rows are the same function of HEAD at every level (`fresh_view`), so nothing but
HEAD's age and the probe differs. The probe's verdict is a function of the HEAD and lanes the
snapshot holds and of this process's own lane, so one probe answers for every strong leg; a leg
probing later could only find a bundle acknowledged after the snapshot, which a read that
linearizes at the snapshot need not see. An `as_of` leg reads a past epoch from HEAD's history
(`Head::as_of`); any HEAD read now answers it the same, so the snapshot's own does, when the
snapshot read HEAD now (not a `bounded` cache hit, whose history may be older than the epoch).
The proof holds; M236 builds it. Depth is unchanged: the snapshot is the round each leg spent.

## Delta
1. **Mixed levels pin.** A multi-query, or a ranked query with `facets`, runs pinned at its
   legs' strongest live level (`strong` > `eventual` > `bounded` smallest tolerance first;
   `session` mapped first, as M131). A leg served from a stronger snapshot is served it whole,
   the lanes' verdict included. A leg finding an unfolded write refuses the request, as today.
2. **`as_of` legs pin.** A leg `as_of` an epoch no longer unpins the request. Under a pin whose
   HEAD was read for it, it reads that HEAD's history and no HEAD of its own; under a `bounded`
   cache hit it reads its own HEAD, as today. A request of only `as_of` legs pins at `eventual`.
3. **One probe.** Under a pin, the first strong leg to ask whether the lanes are settled
   probes them, one `head` request per lane; every other leg awaits that verdict.

Unchanged: single queries without facets, writes, the response (`epochs`, `facets_epoch`,
`staleness_ms` of a leg's own snapshot), what each level promises, peers' shares, depth, and
M131's one retry. No setting: every request reads no more than before (criterion 5).
Not done: a `bounded` leg served from a fresh snapshot does not refill the `bounded` cache.

## Acceptance criteria
1. A 3-leg `strong` multi-query over L registered lanes issues exactly L lane probes (parent:
   3L), one HEAD read and one registry read.
2. A multi-query of `eventual`, `bounded` and `strong` legs, whose rival folds after the
   snapshot, answers every leg from one epoch with one HEAD and one registry read; with the
   rival's write unfolded it is refused `not_folded`, as before.
3. A multi-query of a live leg and two legs `as_of` epoch 1, after the rival folded (epoch 2),
   reads HEAD once (parent: 3), and each leg answers what it answers alone.
4. A ranked query `as_of` 1 with `facets` reads HEAD once (parent: 2), the facets counting the
   hits' epoch.
5. Request counts, before and after, for the shapes of criteria 1 to 4: never more after.
6. Under a `bounded` cache hit, an `as_of` leg reads its own HEAD: the cached HEAD is not used
   for history.

## Test plan
In `crates/pstore-server/tests/it/snapshot.rs`, its `Interfering` store counting `head` probes.
1. `strong_legs_probe_each_lane_once`. Mutation: no memo (each strong leg probes).
2. `mixed_levels_share_one_snapshot`, and `the_tightest_tolerance_pins` (a cached HEAD 300 ms
   old, legs at 600 s and 100 ms). Mutations: `pinnable` refusing mixed levels (parent); a pin
   serving only its own level; the looser `bounded` covering the tighter; `strongest` keeping
   the first leg's level.
3. `as_of_legs_read_the_pinned_head`. Mutations: `as_of` unpinning (parent); the history
   read from the live HEAD's epoch rather than the asked one (answers differ from alone).
4. `a_past_faceted_query_reads_head_once`. Mutation: as 3's first.
5. The counts of tests 1 to 4 against the parent, recorded in VERIFIED.md.
6. `a_cached_pin_does_not_serve_history` (engine). Mutation: the pin's HEAD used on a hit.
Each new test observed red against the parent first; M131's and M164's tests unchanged.

## RA budget
Per multi-query of N legs and L lanes: HEAD reads N -> 1 when legs are mixed or `as_of`
(1 already when they shared a level); registry reads at most 1; lane probes N_strong x L -> L.
W, List: unchanged (0 List). Depth unchanged.

## Risks
- A pinned `bounded` retry now reruns `as_of` legs too: only after a cache hit failed.
- A leg spawning a task loses the pin and reads HEAD itself: test 1's and 3's counts show it.

## Tasks
- M236.1 spec. M236.2 tests, red. M236.3 engine and server. M236.4 VERIFIED, DELIVERY.
