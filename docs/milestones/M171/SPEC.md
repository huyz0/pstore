# M171: UUID and bytes attributes

## Serves
Workstream D of [the delivery plan](../../comparisons/turbopuffer-next-plan.md): "Round-trip
... canonical UUIDs, bytes ... No lossy conversion". Closes `uuid` and `bytes` in the
"Scalar attributes" row of [the comparison](../../comparisons/turbopuffer-2026-10-25.md);
`uint64` is the next milestone's (it needs a zone of its own), and persistent declarations
after that. Extends [M9h](../M9h/SPEC.md)'s request-declared `datetime` the same way.

## Delta
Today a write's `schema` may declare an attribute `"datetime"` and nothing else, and a
string attribute is stored as a string.

After, `schema` may also declare an attribute `"uuid"` or `"bytes"`, with the datetime's
rules: per request, the value may be a scalar or an array of them, a declaration no
document carries is refused (so a write of patches alone cannot declare one, as today for
`datetime`), and the text field cannot be declared.
- `uuid`: a string of 32 hex digits in the 8-4-4-4-12 hyphenated form, either case. Stored
  as 16 bytes. Answered as the lowercase hyphenated form.
- `bytes`: a string of standard base64 with padding (RFC 4648 §4), decoding to at most
  4,096 bytes. Stored as those bytes. Answered as standard padded base64.
A value that is not one is refused, `bad_request`, naming the attribute; nothing is
buffered; the bound is the server's, as the engine API's other bounds are not
`check_storable`'s. A patch's attributes in a write that declares a name are read by the
declaration, as they are for datetimes.

The format gains two value tags, 6 (`uuid`, 16 bytes) and 7 (`bytes`, a length and the
bytes), in segment blocks and bundles; the deferred-condition codec (its own numbering) gains
6 and 7 too, which a `$ref_new` condition over such a value carries. A reader before M171
refuses these tags as unknown, never misreads them, and an older fold decodes such a
condition as unreadable: deploy readers first.

Compared, in a filter, against a string literal read as the row's type, as a datetime row
reads one (M9h.3): a literal that does not parse equals and orders against no such row, so
`Eq`, `In`, `Lt`/`Lte`/`Gt`/`Gte`, `AnyLt`..`AnyGte`, `Contains` and `ContainsAny` match none
of them, and the negated forms (`NotEq`, `NotIn`, `NotContains*`) match every one, as for a
datetime. UUIDs compare by their 16 bytes and bytes bytewise. A row's UUID never equals a
row's bytes or string. A boost's `Eq` term reads a string literal the same way. `rank_by`
ascending orders the groups bool, number, datetime, string, UUID, bytes, array, then
absent; descending reverses all but absent, which stays last. Facets and `limit_per` group
by the value. No zone prunes on these types: a block holding one marks
the name as holding strings, so a datetime zone over the same name cannot rule it out.

Unchanged: every other type, its encoding and its answers; a write with no declaration.
Patterns, fuzzy, token filters and highlights read strings only, so they match no UUID or
bytes value.

## Acceptance criteria
1. A write declaring `id_ref: uuid` and `blob: bytes`, scalars and arrays, durable and
   folded, answers the same values (UUID lowercased) in a projection, before and after a
   restart, and in the unfolded view before the fold.
2. Filters: `Eq`, `NotEq`, `In`, `Lt`, `Gte` and `ContainsAny` against UUID and bytes
   rows give the rows a reference computed in the test from the stored values; an
   unparseable literal matches no row under `Eq` and every typed row under `NotEq`; a UUID
   row and a string row with the same text are told apart; a boost `Eq` on a UUID fires.
3. `rank_by` asc and desc over a name holding ints, strings, UUIDs and bytes orders them in
   their groups, UUIDs by byte order.
4. Refused, 400, and nothing buffered: a malformed UUID (wrong length, non-hex, no
   hyphens), invalid base64, bytes over 4,096, a non-string value, a declaration no
   document carries, the text field declared.
5. Format: encode then decode of every tag round-trips; tag 6 with fewer than 16 bytes and
   tag 7 with a length past the buffer are refused as corrupt, in blocks and in the
   condition codec; a block holding a UUID, and one holding bytes, beside a datetime under
   one name is not pruned by a literal of that type. A `$ref_new` condition over a UUID
   applies.
6. Existing tests pass. The ones extended (the datetime pruning property, the two codecs'
   fixtures, the group key order) gain the new values and lose none of their cases.

## Test plan
Server tests in `crates/pstore-server/tests/it/uuid_bytes.rs`; format unit tests beside
`docs.rs`'s and `writer.rs`'s; filter unit tests beside `compare`. Red before: 1 to 4
are refused on the parent (`datetime is the one type a write declares`). The variants were
written before the spec passed review, so 5's red is its mutations. Mutations, each to be
killed:
- a UUID answered uppercase or unhyphenated (1); bytes answered unpadded (1);
- a UUID literal compared as a string (2); bytes compared as a UUID (2);
- the UUID and bytes groups swapped in `rank_by` (3);
- the 4,096 bound off by one (4); uppercase hex refused (1);
- the writer not marking the strings flag, for UUIDs or for bytes (5);
- the boost's `Eq` structural (2); the condition codec's tags dropped (5).

## RA budget
Unchanged: no request is added. A UUID is 17 bytes a value in a block, smaller than its
36-byte string.

## Risks
- A mixed-version cluster: an old reader refuses a segment holding a new tag. Readers
  first, as the plan's migration says; the refusal is loud, never a misread.
- External sources (Parquet, Lance) keep mapping their UUID and binary columns as today;
  mapping them is not in this milestone.

## Tasks
- M171.1: the format's variants and tags, the filter, order and grouping cases.
- M171.2: the server's declarations, parsing and answers, docs.

## Review
Spec review round 1 (block, two majors, both taken): boost's `Eq` named; the negated forms
match every typed row on an unparseable literal. Minors taken: the condition codec and
`$ref_new`; both types for the strings flag; real operator names; the full group order;
string-only predicates; the bound is the server's; patches-only writes; criterion 5's red.
