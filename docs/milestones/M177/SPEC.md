# M177: string sparse dimensions

## Serves
D-72 (sparse impacts) and workstream D of [the plan](../../comparisons/turbopuffer-next-plan.md):
"sparse string dimensions map deterministically and losslessly across restart/writers; do
not hash them into u32 without collision handling". The engine half; M178 is the HTTP API.

## Delta
Today a sparse field is `(u32, Impact)` pairs (M5a), so a string vocabulary needs a mapping
no writer can agree on without a lock or a shared table. After, a dimension **is** its
string, `VectorField::Sparse(Vec<(String, Impact)>)`: nothing to map, nothing to agree on.
Keys compare as raw bytes, no Unicode normalization.
- **Dictionary v2** (`.sdict`): fixed-width entries sorted by key bytes, each naming its key's
  span (`u64` offset, `u16` length) in a key area after the table, as M32's term dictionary
  does; lookup stays a binary search. `Entry` loses `dim`. The shared decoded cache charges
  the key bytes. Postings (`SparsePostings`) are unchanged.
- **WAL bundles**: a sparse field is kind 2, keys then impacts, and an index's run holding
  one is `DOCS_VERSION` 3, so a pre-M177 binary refuses that run loudly (`UnsupportedVersion`)
  rather than reading key bytes as pairs; a run with no sparse field stays version 2, so a
  rollback replays it. A version-2 run takes kinds 0 and 1, a version-3 run 0 and 2.
- **Old encodings read**: a v1 dictionary decodes to a byte-sorted key set of `d.to_string()`,
  a kind-1 field likewise; only that canonical spelling matches (`"07"`, `"+7"` do not). A
  compaction over v1 and v2 segments writes v2. Nothing writes v1 or kind 1 again.
- **A query** (`Prefetch::Sparse`, and M54's peer `WireLeg::Sparse`) carries `(String, f32)`;
  a repeated query key sums its weights; a key a segment lacks costs nothing, as today.
- **The door**: `check_storable` refuses a repeated key in one field, an empty key, and a key
  over 65,535 bytes. A scan answers a row's pairs in key byte order.
- **Footprint** (M141) counts a pair as its key's bytes plus 4.
- Unchanged: one sparse field a document, impact encodings, a sparse leg's depth, BM25, the
  memtable sealed as a segment. Not here: the HTTP field, limits, the public encoding.

## Acceptance criteria
1. A field over keys `"1"`, `"01"`, `"1.0"`, `"é"` precomposed and decomposed, a 200-byte key
   and one differing only in its last byte, two sharing their first 8 bytes, a 65,535-byte
   key and 70,000 random ones round-trips, as a sorted set, through a bundle, a segment scan
   and a compaction; each key keeps its own postings.
2. Top-k of a sparse search, F32 encoding, over 2,000 rows and those keys equals an
   exhaustive dot-product oracle's, with ties (by row), zero and negative weights, keys a
   segment lacks, and a repeated query key (summed).
3. Two engines on one store write rows naming the same keys in different orders and
   batches; after a fold and a restart a query by key finds both's rows as one dimension.
   A sparse leg crosses M54's peer wire with its keys and weights bit for bit, and a
   split query answers as the unsplit one.
4. A v1 dictionary and a version-2 bundle with a kind-1 field, written by copies of
   today's encoders, decode: `7` answers `"7"`, `9` and `10` both found, `"07"` nothing; a
   compaction of v1 with v2 writes v2. A v3 dictionary, a kind-3 field, kind 2 in a version-2
   run, and (by a copy of today's decoder) a version-3 run are refused, that decoder still
   replaying a version-2 run of the same bundle; a run with no sparse field is version 2.
5. A repeated, empty or 65,536-byte key is refused at the door; the existing sparse depth
   tests' round trips and bytes are unchanged; `Buffered` counts a pair as key + 4.

## Test plan
Format tests in `crates/pstore-format/tests/it/sparse.rs`, index and engine tests beside the
existing sparse ones, the peer wire in `peers.rs`'s round trip, the split in the engine's `split` tests. Red first: 1 to 3 do not
compile against u32 dimensions, so their red is the mutations, as M176's unit tests' was;
4's fixtures come from copies of today's encoders kept in the test. Mutations, each to be
killed: a key truncated to 4 or 8 bytes, or to a hash (1); the keys sorted other than by
bytes, or searched in a stale order (1, 2, 4); the first of a repeated query key kept (2);
the peer leg's keys dropped (3); v1 refused, matched by `parse`, or left numerically sorted
(4); version 3 written for a run with no sparse field, or kind 2 taken in a version-2 run (4);
kind 1 read as kind 2 (4); a
repeated or over-long key admitted (5); the footprint counting 8 (5).

## RA budget
Unchanged: no request is added; a sparse leg is still the dictionary beside the footer and
one batch of ranges. The dictionary grows by its key bytes, inside its `Pinned` class.

## Risks
- Strings cost memory a pair; the footprint bound (M141) sees it.
- A big vocabulary makes a big pinned sidecar (70,000 keys of 200 bytes is 14 MB) until M178
  bounds keys; the existing depth tests read its bytes.
- A rollback with an unfolded version-3 run refuses that index's replay, loudly, until it
  rolls forward; nothing public writes one until M178.
- An old peer cannot decode a new sparse leg, so a rolling upgrade fails that peer's legs;
  nothing public sends one until M178.

## Tasks
- M177.1: string keys in the model, bundle, dictionary, index, query and peers; old read.

## Review
Spec review round 1 (approve with changes, five majors, all taken): a bundle holding a
sparse field is `DOCS_VERSION` 3 so an old binary refuses it, others stay 2 for rollback;
v1 decodes to a byte-sorted set of canonical spellings; keys bounded at 65,535 bytes; the
peer leg has a criterion; adversarial keys rather than a seed-dependent hash collision.
Minors taken: the cache charges key bytes, `Entry` loses `dim`, a repeated query key sums,
no normalization, sorted-set round trips, D-72 in Serves, the pinned sidecar's size.
Round 2 (approve, three minors, all taken): the version is an index's run's, not a bundle's;
which kinds each version takes; the rollback risk.
Amended in implementation: criterion 3's peer clause is the wire round trip plus M54's split
equality, as no HTTP request sends a sparse leg before M178; fixtures that named one
dimension twice, now refused, name another. From M178's spec review: the key offset is a `u64`, as M178 bounds no
segment's vocabulary (a refusal at fold would block every later fold).
