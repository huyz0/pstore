# M289: f16 and i8 vector storage, opt-in per index

## Serves
Workstream D of [the plan](../../comparisons/turbopuffer-next-plan.md): the narrower-storage
increment [M195](../M195/SPEC.md) left for later ("storing the narrower bytes is a later
increment"), recorded "Not now" in [remaining-2026-11-17.md](../../comparisons/remaining-2026-11-17.md)
and since asked for by the owner (2026-11-24), opt-in per index. Planned under the label
M195's follow-up; M289 was the next free number. The transport half stays as decided there
(the pinned vendor SDK reads and writes f32 base64 only) and is not built. D-34's budget is
unchanged; no `D-` or `OQ-` decision is reopened.

## Delta
- **The setting.** `vector_storage`: `f32` (today), `f16` or `i8`, what a segment's full
  precision dense vectors are stored as. A **cluster default**, `PSTORE_VECTOR_STORAGE`
  (`f32` unset, `f16`, `i8`, any other value refused by name at start), and an **index
  override** set by `PATCH /v1/indexes/{index}` `{"storage": {"vector_storage": ...}}`
  (absent leaves, `null` clears), kept in HEAD and reported by `GET /v1/indexes/{index}`'s
  `storage`, exactly as M231's settings are. Every fold and compaction seals by the override
  over the sealing server's default; a segment already written keeps what it holds.
- **HEAD.** A new trailing section after M231's overrides: per index that exists and sets
  one, its name and one code (1 f32, 2 f16, 3 i8). Written only when some index sets one,
  so a HEAD without one keeps its bytes. M231's byte has no free bits, so this is a section.
- **What is stored.** Every dense field of the segment (`vector`, named, multi-vector and
  derived), and only the full precision section; centroids, RaBitQ and int8 codes are
  unchanged and still built from the f32 vectors.
  - `f16`: each component rounded to the nearest f16, ties to even (M178's conversion). A
    field with a component that would be infinite is stored f32 in that segment, whole.
  - `i8`: per vector one signed exponent byte `e`, then each component `q = round(x / 2^e)`
    (ties to even) as an i8, read back as `q * 2^e`. `e` is the smallest in -127..=127 with
    every `q` in -128..=127. A power of two, so decoding is exact and re-encoding a decoded
    vector gives the same bytes; integers from -128 to 127 have `e = 0` and are exact.
  - The metric's own extra component (euclidean's `-|x|^2 / 2`, M9d) stays f32, after the
    narrow components: the engine tells the writer how many there are (`tail`).
- **Segment format.** A segment with at least one narrow field is **version 4**
  (`VERSION_NARROW`): version 3's columnar layout, and its Fields table may hold kinds 2
  (dense f16) and 3 (dense i8), whose `metric` byte is the f32 `tail` count. A segment with no
  narrow field is written as today. A reader before M289 refuses version 4 at `open`
  (`UnsupportedVersion`), never misreads it. With row blocks (`PSTORE_BLOCK_LAYOUT=rows`,
  the rollback layout) vectors stay f32 whatever the setting, so a rollback setting stays
  readable by an older binary.
- **Readers.** Every reader of a dense section decodes by the field's layout: whole fields,
  named rows, offsets of a varying field (byte offsets of the narrow body), the legacy
  `vector_rows`, `vectors` and `scan`'s inline slices. What they return is f32, so the
  index, the query and the server above them are unchanged.
- **What a client sees.** Under `f16` or `i8`, a folded row's stored vector is the rounded
  one: `include_vectors`, exact scores and the rerank are over it; an unfolded row is answered
  from the log, unrounded, as today. Exactly lossless where M195's declared type matches and
  the metric is dot product or euclidean (`[N]f16` under `f16`, `[N]i8` under `i8` or `f16`
  storage of values f16 holds); under cosine the stored vector is normalized and rounding it
  loses precision whatever was declared.

## Acceptance criteria
1. Codec, both types: f16-exact components are stored bit-equal; a halfway component rounds
   to even; a component past 65,504's rounding bound makes the field f32 in that segment;
   i8 integers from -128 to 127 are exact (`e = 0`); for seeded random vectors at every
   scale from 1e-30 to 1e30 each decoded component is within `2^(e-1)` of the written one,
   and encoding a decoded vector gives the same bytes; a zero vector decodes to zeros; the
   `tail` components are bit-equal f32. A truncated narrow section or an exponent outside
   -127..=127 is refused, never a panic.
2. Readers: a segment of fixed, named, varying (multi-vector) and sparse fields written in
   each mode returns from `field_vectors`, `read_fields`, `field_rows`, `field_spans`'
   ranges, `vector_rows`, `vectors` and `scan` exactly the codec's decoded values for every
   row; under `f32` the segment is byte-equal to one written before M289 (pinned by the
   existing fixtures) and its version is 3; with any narrow field its version is 4; with row
   blocks no field is narrow.
3. Lossless where declared: a `[16]f16` index under dot product and under euclidean with
   `vector_storage: f16`, and a `[16]i8` one with `i8`, folded and restarted, answer
   `include_vectors` and 20 exact queries' ids and scores bit-equal to the same index under
   `f32`. Under cosine, and for an undeclared field, the answer is the decoded vector's.
4. The setting: the environment's three values and a refused fourth; an override wins over
   the default for every later fold and compaction, by any server, survives a restart,
   keeps M231's settings, and cleared returns the default with HEAD forgetting it; a HEAD
   without one keeps its bytes; a malformed code is `CorruptHead`; a patch value other than
   the three or `null` is `400` naming the setting; `GET` reports the effective value and
   the override.
5. Both modes: every existing test runs under the default (`f32`) with its assertions
   unchanged; new server tests fold under `f16` and `i8` and check criterion 3 and that a
   folded row's answer is the codec's; `scripts/depth.sh` green (no request or round added).
6. The bench, `provisional` (container): `cargo run --release -p pstore-index --example
   vector_storage` over the recall gate's two corpora (20,000 x 384d clustered, 10,000 x
   384d uniform, both normalized) and the same corpora unnormalized under euclidean, each
   mode: the dense section's bytes, recall@10 of the default ANN search against exact f32
   truth, the exact top-10's overlap with f32's, the largest score error, one segment's
   exact scan time and its write time. Recorded in `VERIFIED.md` with the downsides. The
   default changes from `f32` only if, on every corpus, the narrow mode loses no more than
   0.002 of recall@10 and 0.002 of exact overlap and its exact scan is not slower; otherwise
   it stays `f32`, and the numbers say when to turn it on.

## Test plan
- 1: unit tests in `crates/pstore-format/src/narrow.rs` and
  `crates/pstore-format/tests/it/narrow.rs`. Kills: ties away from even; the overflow
  fallback dropped; `e` one too large or too small; the tail narrowed; the exponent's range
  check removed; a decoded component's scale `2^(e+1)`.
- 2: `crates/pstore-format/tests/it/narrow.rs`, every reader per mode. Kills: a reader
  slicing by `dims * 4` under a narrow kind; a varying field's offsets in f32 bytes; version
  4 written for an all-f32 segment; narrow written with row blocks.
- 3, 5: `crates/pstore-server/tests/it/vector_storage.rs`. Kills: the override ignored at
  seal; the tail not passed for euclidean.
- 4: `head.rs` unit tests beside M231's; engine and server tests beside
  `tests/it/storage.rs`. Kills: the codes swapped; an unset override written; a patch
  replacing M231's settings.
- 6: the example.

## RA budget
No request and no round is added: every section is where it was, only shorter. A rerank or
projection reads `2N` (f16) or `N + 1` (i8) bytes a vector instead of `4N`, plus 4 a tail
component. A patch: one HEAD read and at most one CAS PUT, as M231's. HEAD grows by a name
and a byte for each index that sets an override.

## Risks
- Answers change under the narrow modes: scores, the rerank order at ties, and the vectors
  answered. Opt-in, and the bench measures how much.
- Every exact score and rerank decodes the narrow bytes: CPU the bench measures.
- Rollback: version 4 segments are unreadable before M289. `PSTORE_VECTOR_STORAGE=f32` and
  overrides cleared stop new ones; existing ones need a rewrite (compaction under `f32`), as
  M231's `docs/deploy.md` rollback says for version 3. An override is a trailing HEAD section
  that a binary before M289 refuses (`CorruptHead`): no override until the rollout is done.
- Unfolded and folded rows of one id differ in precision: a row's answer can change at its
  fold. Documented.

## Tasks
- M289.1: `narrow.rs`, the writer's kinds and version 4, every reader, format tests.
- M289.2: the setting (BlockOptions, HEAD section, engine, server env, PATCH, GET,
  OpenAPI and clients, deploy docs), server tests in both modes.
- M289.3: the bench example, `VERIFIED.md`, DELIVERY and remaining rows.

## Review
