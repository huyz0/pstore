# M195: f16 and i8 vector types

## Serves
Workstream D of [the plan](../../comparisons/turbopuffer-next-plan.md): "Treat f16/i8
ingestion/output ... as separate increments, not as aliases for internal quantization", and
"Validate f16/i8 precision and score direction with exact oracles". Closes the "Dense
vectors" row's public-type half in [the comparison](../../comparisons/turbopuffer-2026-10-25.md)
and the "f16/i8 ... precision contracts" row of [DELIVERY](../../comparisons/DELIVERY.md).
Builds on the recorded attribute types of [M173](../M173/SPEC.md), whose rules it reuses for a
vector field's element type, and on M9d's base64 vectors. No `D-` or `OQ-` decision is
reopened; D-34's budget is unchanged.

## Why a contract, not storage
Internal quantization (SQ8, RaBitQ) is an index's private approximation of f32 vectors; it
says nothing about what a client wrote. A declared f16 or i8 field is a promise about the
values: what is stored is exactly the declared type's value (before the metric's own
transform), and a score is the metric over those values. This milestone makes that promise.
Segments keep storing f32, which holds every f16 and every i8 exactly, so no codec changes;
storing the narrower bytes is a later increment that changes no answer (asked of the owner,
recorded under Review).

## Delta
- **Request.** A write's `schema` may give `vector`, or a named dense field, a vector type:
  `"[N]f32"`, `"[N]f16"` or `"[N]i8"`. `N` is the width as a client writes it (`client_dims`,
  never the euclidean stored width), and must equal the width of every vector of the field
  in the request; otherwise `400`. `f32` is the default and records nothing. Which a name is:
  `vector`, or a key of some document's `vectors` in the request, or a field the schema this
  process reads records (`IndexSchema.fields`), is a vector field; any other name is an
  attribute. A vector type on an attribute, an attribute type on a vector field, a vector type
  on `sparse`, and a vector type on a field no document of the request carries (including
  `vector` in a write with no vectors, M170) are each `400`, as M173 refuses a declaration
  that would do nothing.
- **Every vector of the field.** The type applies to each vector of a field, single or
  multi-vector alike (whether a field holds one vector a row is not recorded and may change
  between rows); M67's encoding is derived from the rounded values.
- **The door.** A vector's components are read from JSON as f32 (as today), then, under a
  known f16 type, rounded to the nearest f16 (ties to even) and refused unless the result is
  finite: 65,519 is admitted as 65,504 and 65,520 is refused; under i8, each must be an
  integer from -128 to 127 as that f32 (`3.0` is one, `3.5` and `128` are not; `127.00001`
  reads as `127` in f32 and is one). A refusal is `400 bad_request` naming the field, the
  component's index and the rule, nothing buffered. A base64 vector (M9d) is decoded after the
  known types are read, as the known type's little-endian bytes: `2N` bytes for f16, `N`
  for i8, `4N` for f32; another length is `400`. Then cosine normalizes and euclidean extends,
  as today.
- **Known types.** As M173's: recorded, this process's unfolded declarations, the request's.
  M173's rule that a write holding no attribute value reads none is amended: a write holding
  a vector reads them too, in the engine after its once-a-process HEAD read
  (`refuse_replica`), so a fresh process's write costs no extra request and M141's
  zero-request refusal holds (it refuses before the engine).
- **Stamping and recording.** A row's `$types` names each vector field it holds that has a
  known non-f32 type, under the reserved name `$vector.<field>` (`$vector.vector` for
  `vector`), beside its attributes'. The fold records it as M173 records an attribute's
  type, in M173's recorded-types section, which is keyed by any string (M176's `id` is the
  precedent), with two new kind codes. Attribute names cannot begin with `$` and no named
  field is `vector` or `sparse`, so none collides. One field holds one type
  (`schema_conflict` naming both). No new HEAD section.
- **The fold's judgement is by stamp, not value:** a stored vector under cosine is
  normalized and under euclidean carries its norm component, so its values cannot be judged.
  An upsert whose `$types` names another type for a recorded field, or names none for a field
  recorded f16 or i8 (a writer that had not yet read it), is quarantined, counted in
  `quarantined_rows`. The quarantine export's reason names the field and both types.
- **Readers of the recorded types** skip `$vector.` names wherever they skip `id`'s:
  `schema.attributes` never answers one, and the per-request copy of known attribute types
  does not carry them.
- **Patches** carry no vector and are unchanged. `upsert_columns`' `vector` and `vectors`
  columns pass the same door.
- **Answer.** `GET /v1/indexes/{id}` answers `schema.vector_types`: each field recorded f16
  or i8 to `"[N]f16"` or `"[N]i8"`, absent when none. `include_vectors` is unchanged: under
  dot product and euclidean it answers the declared type's values exactly; under cosine the
  normalized vector, as M179 has it. `vector_encoding: "base64"` answers f32 bytes for every
  type; a narrower output encoding is a later increment.
- **Queries** are unchanged: a query vector is f32, and a score is the metric over the
  stored values, as for an f32 field.
- **Deploy readers first.** A reader before M195 refuses a HEAD holding a new kind code
  (`CorruptHead`), as for M173. A fold before M195 drops an unknown `$types` name silently,
  records nothing and seals the rows: during a rolling deploy a declared write folded by an
  older build leaves its field unrecorded, and its rows hold the rounded values.
- **Unchanged:** f32 fields, sparse, internal quantization, and every index that declares no
  vector type (its HEAD keeps its bytes).

## Acceptance criteria
1. Precision, both types and every metric: 400 rows of width 16 with components drawn to
   exercise rounding (halfway cases, subnormals, 65,504, 65,519 and -65,519 for f16; -128,
   127 and integral floats for i8), written declared, folded in two segments, 50 unfolded,
   and restarted. Under dot product and euclidean `include_vectors` answers each row's
   components bit-equal to an oracle's rounding of the written f32 (f16: an independent
   round-to-nearest-even over the f32 bits; i8: the integer). Each of 20 exact queries a
   metric answers the same ids, in order, and bit-equal scores as the same rows written
   rounded by the oracle into an undeclared f32 index. A named dense field and a
   multi-vector field declared f16 answer the same way (the multi field by `multi` MaxSim).
2. Refusals, each `400` naming the field and the rule, nothing stored: an f16 component of
   65,520 or -65,520 and a non-finite one; an i8 component of 3.5, 128 or -129; a declared
   `N` other than the width; a vector type on an attribute, on `sparse`, on a field no
   document carries and on `vector` in a write with none; an attribute type on a vector
   field; a base64 vector of the wrong length for its known type. A base64 f16 and i8 vector
   of the right length is admitted and answers the decoded values; the same through
   `upsert_columns`.
3. Recording: after a declared write folds, a later vector-only undeclared write is read by
   the recorded type (rounded, or refused for i8), from this process and from a second process
   over the same store; declaring another type is `400 schema_conflict` naming both;
   `schema.vector_types` answers each recorded field and `schema.attributes` shows no
   `$vector.` name. A second process that wrote undeclared rows before the type was recorded
   folds after it: those rows are quarantined, and the export's reason names the field and
   both types. Two processes declaring `vector` f16 and i8 before one fold keep the first in
   lane order and quarantine the other's rows.
4. Compatibility: an index that declares no vector type keeps its HEAD bytes (a pinned
   byte count) and every existing vector test; the depth and request counts of a write, a
   fold and a query of a declared index equal those of an undeclared one, and a fresh
   process's first vector-only write costs the requests it costs today (pinned).

## Test plan
Server tests in `crates/pstore-server/tests/it/vector_types.rs`, and format tests of the two
kind codes in `crates/pstore-format/tests/it/`. Red first: on the parent a vector type in
`schema` is `400`, so every criterion fails.

Mutations, each to be killed:
- rounding toward zero, or ties away from even (1);
- the f16 bound checked before rounding, or the bound off by one f16 step (1, 2);
- an i8 integrality or range check removed (2);
- cosine normalizing before rounding (1);
- the base64 element width taken as f32's, or from the declaration only (2, 3);
- the known types not read for a vector-only write (3);
- a missing stamp under a recorded field not quarantined (3);
- `$vector.` names shown in `schema.attributes` (3);
- a HEAD written with a types section for an index that declares none (4).

## RA budget
No request is added: the type travels in M173's section and `$types`, which the door and
the fold already read and write, and the engine's once-a-process HEAD read is already made
for every write. A declared index's HEAD grows by one entry a declared field (about 20
bytes). No segment byte changes.

## Risks
- A reader before M195 refuses a HEAD holding a vector type, and a fold before it records
  none: deploy readers first, and no rollback after.
- Storage is not smaller: a declared f16 field costs what an f32 one does until the later
  increment. The docs say so.
- Under cosine the answered vector is normalized, so an i8 field does not answer integers
  there; documented, as M179's cosine answer is.

## Tasks
- M195.1: the declarations, the door's rounding and refusals, base64 widths, stamping,
  recording and quarantine, `schema.vector_types`, the tests, docs and VERIFIED.md.

## Review
Asked of the owner on a decision card (2026-11-06): the precision contract first with
storage unchanged (recommended), both at once, or neither. Written to the recommendation.

Spec review round 1 (approve with changes, two majors, both taken): the fold judges by the
`$types` stamp, not by value, since a stored cosine or euclidean vector is transformed; the
multi-vector refusal dropped, the type applying to every vector of a field. Minors taken:
base64 is M9d's and decoded after the known types; known types read for vector-only writes
in the engine after `refuse_replica`, keeping M141's zero-request refusal; how a vector field
is stamped; readers skipping `$vector.`; an old fold's silent drop; the quarantine reason;
M170's no-vector write; a field no document carries; attribute or vector field; columns;
f32 double rounding; the f16 bound's values; scores compared with an undeclared index
written rounded, not an independent summation; `N` as `client_dims`.

Amended while the tests were written:
- **Base64 stays f32 bytes for every type**, in and out (M9d's transport, decoded before the
  types are known, as today): a base64 vector under f16 or i8 is read as f32s and then
  rounded or refused as an array is. A narrower transport, with the narrower output, is the
  later increment's. So the "decoded after the known types" rule and the "base64 of the
  wrong length for its type" refusal are withdrawn; criterion 2 checks a base64 f32 vector
  rounded under f16 and refused under i8 instead.
- **A vector field is `vector` or a key of some document's `vectors` in the request.** A
  declaration on any other name is an attribute's; a vector type there is refused (with no
  document carrying it as a vector field, the same message as a field no document
  carries), so the recorded fields are not consulted.
- **Criterion 4's HEAD bytes** are checked as an f32 declaration and no declaration costing
  the same requests (pinned) and recording nothing (`vector_types` absent); no HEAD byte
  count is pinned.
- **`[N]f32` on a field typed f16 or i8** (recorded, or declared by this process's unfolded
  writes) is `400 schema_conflict` naming both, after one HEAD re-read as M173's refusals
  make; the server reads the known types for a write declaring f32 only. Before, it would
  have been rounded silently (code review).
- A recorded type whose field's width the schema does not hold is answered without a
  width (`"f16"`), never dropped (code review).
- Tests: `crates/pstore-server/tests/it/vector_types.rs` and
  `crates/pstore-format/tests/it/vector_types.rs`.
