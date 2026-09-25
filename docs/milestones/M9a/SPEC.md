# M9a — Attributes: written with a document, returned with its result

**Serves:** **D34** ([`api-design.md`](../../research/11-design/api-design.md): a document
carries `attributes`; the query chooses what comes back). First of M9's nine, per
[`turbopuffer-api-parity.md`](../../research/11-design/turbopuffer-api-parity.md).

## Delta

**Write.** `PUT /v1/indexes/{index}/documents` accepts `attributes: {name: value}` per
document. A value is a JSON **integer** that fits `i64` → `Value::Int`, or a JSON **string** →
`Value::Str` — the two types the format stores. Anything else is **refused, never coerced**,
`400 bad_request` naming the document and the attribute, and the whole batch is refused before
anything is buffered:
- a float, bool, array, object or `null` (M9h adds types; turbopuffer reads `null` as "unset",
  which is M9c's to decide once a write can overwrite);
- an integer outside `i64` (e.g. `u64::MAX`);
- an empty name.

`attributes.text` **is** the text field, as `api-design.md` writes it: a string, BM25-indexed
exactly as the top-level `text` is. It must be a string, and a document giving **both** is
refused — two spellings of one field would need a rule for which wins. `attributes: null` is a
malformed body. A key repeated in one JSON object keeps its last value (serde_json's rule, as
for every body this server parses).

**Query.** `POST /v1/indexes/{index}/query` accepts turbopuffer's spelling:
`include_attributes` — `true` (every attribute), `false`, or `[names]` — and
`exclude_attributes: [names]`. Omitting both returns rows with **no** `attributes` key,
byte-identical to today. `exclude_attributes` removes names from what `include_attributes`
selects, and alone means "all except these"; with an explicit `false` it selects nothing. A row that was
asked for attributes always carries the key, `{}` if it has none of them (`[]` included). Names
a row lacks are omitted. Works on the unfolded rows, on folded segments, and with `as_of`.
⚠️ `api-design.md` proposed `include: [...]`; M9 speaks turbopuffer's names, recorded there.

**Zero extra requests.** Resolving a hit's id already fetches and decodes its whole data
block (`Segment::ids_at`, one fan-out round). The block carries the attributes, so
`Segment::rows_at` keeps them, `ids_at` becomes a map over it, `pstore_query::query_rows`
replaces `query_ids` (⚠️ amended: the mutation sweep found `query_ids` had no caller left), and
`Answer` carries attributes in hit order.

**⚠️ Found at spec review: integer attribute names can make a segment unwritable.** Every
integer attribute's name becomes a zone-map key in the block index, which must fit
`INDEX_BUDGET`. Doubling the block size cannot help — at one block the zone map is the union
of every name — so today `try_finish` refuses, and since the fold seals with it, **one batch of
`{"score_<uuid>": 1}` documents fails every fold and query of the tenant, forever.** Unreachable
until this milestone lets a client write integer attributes. **Fix, in the writer:** when the
doubling ends without fitting, seal again **without zone maps**, running the same doubling
from the requested block size — a zone-free entry is 20 bytes, so it always fits (⚠️ amended at
spec review: without that doubling a 30,000-row fold still failed). A block with no zone map is always read (`blocks_to_read`), so this costs pruning, never rows.
Segments that fit today are byte-identical.

**Does not change:** the bundle or segment encoding (attributes were always stored), the
schema (types are per value until M9h), ranking, or the request **count** of any query.
⚠️ **Bytes do grow with attributes**, whether or not a query asks for them: the id round
reads whole blocks, so every query pays for the attribute bytes of the blocks its hits land
in. No size limit is added here beyond the body limit.

## Acceptance criteria

1. Attributes written in a batch — `attributes.text` included — come back exactly from
   `include_attributes: true`: from unfolded rows, after a fold, and with `as_of`; and
   `attributes.text` is found by a `text` query.
2. `[names]` returns only those; `exclude_attributes` removes them; omitted means no key;
   `[]` means `{}`.
3. Against a folded index, the same query with and without `include_attributes: true`
   reports the **same** `meta.cost`.
4. Each refusal returns `400 bad_request` naming the document and attribute, and the index is
   still absent afterwards.
5. `Segment::rows_at` returns each row's id and attributes (`None` past the end) in one round;
   `ids_at` agrees.
6. A fold of 300 documents with 300 distinct integer names succeeds and answers with their
   attributes; at the format, such a segment seals with **more than one** block and no zone
   maps — doubling further when even zone-free it does not fit — and a segment that fits
   keeps them.
7. `./scripts/gates.sh` passes; `./scripts/mutants.sh` over the changed lines misses 0.

## Test plan

| # | Fails first | Mutation it catches |
|---|---|---|
| 1 | today's door, which ignores `attributes` | not written; lost at fold or on `as_of`; `attributes.text` not indexed |
| 2 | `ResultRow` has no `attributes` | names or exclusions ignored; `{}` emitted when not asked |
| 3 | by hand: a second `get_ranges` in `rows_at` | a second fetch for attributes |
| 4 | today's door accepts the batch | coercion; a partial batch buffered |
| 5 | no `rows_at` | attributes dropped in the decode; rows misattributed |
| 6 | today's writer refuses the segment | the fallback not taken, taken at one block, or without doubling |

## RA budget

Request counts unchanged: W 1 per durable batch; a query is HEAD, open, legs, and the id round
(BACKLOG row 26). ⚠️ Row 26's plan to drop the id round would keep it for queries that ask for
attributes — the block is where they live.

## Risks

- A client already sending `attributes` (ignored until now) starts storing them. Undocumented
  before; the feature now.
- Mixed types under one name are stored as written; M9b's filters must say what `Gt` means for
  a string.
- A zone-map-free segment is unprunable. Only a segment that could not be written at all
  loses them.

## Tasks

- **M9a.1** — `rows_at`, `query_rows`, `Answer`'s attributes, the writer's fallback, the door
  and the query fields, the tests, this ledger.
