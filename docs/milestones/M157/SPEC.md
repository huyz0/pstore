# M157: splitting a query over several text fields

## Serves
Workstream E of [the delivery plan](../../comparisons/turbopuffer-next-plan.md) ("multiple
string/string-array BM25 fields"). [M153](../M153/SPEC.md) answers a query whose text legs
read two or more fields correctly by never splitting it: a phased share carries one `Stats`,
for one field. [M154](../M154/SPEC.md) split a query over any one field. This milestone splits
a query over several, so the vendor's `Sum` and `Max` of BM25 clauses over different
attributes cost what one field's query costs in distribution. No D- or OQ- id covers it. D-30
(corpus statistics ride in the dictionary sidecar) and D-34 (the round-trip budget) constrain
it.

## Delta
Today a phased share's phase 1 answers one `Stats`, for the one field its text legs read.
Phase 2 takes the global sum of that one field. `run` refuses a phased share beside several
text fields (`Unimplemented("a phased share of several text fields")`), and the engine never
gives such a query shares (`one_text_field`).

After:

**The query layer.**
- **Statistics are per field everywhere a share carries them.** `FieldStats`, today private,
  becomes `pub type FieldStats = BTreeMap<String, Stats>`, keyed by the field a text leg
  names. These change to it:
  - `Share::Phased`'s `stats` future yields a `FieldStats`, and its `scan` takes one;
  - `open_part` takes `terms: &BTreeMap<String, Vec<String>>`, each field's terms, and returns
    a `FieldStats` with one entry for each field its legs read, summed over the share's
    segments that carry that field;
  - `scan_part` and `part` take `&FieldStats`.
- **`open_part` and `part` accept legs over several fields.** M151's refusal (`text legs over
  different fields`) is lifted for them. `open_part` keeps M151's "a segment without the
  field adds nothing" for each field. The terms `open_part` counts for a field are that
  field's entry alone (M119's `only`, per field).
- **`run` takes phased shares beside several fields.** The refusal is removed. Each field's
  global `Stats` is the sum of the coordinator's own segments' and every phased share's
  entry for that field. Every phase 2 is sent the whole `FieldStats`. A share whose phase 1
  failed is opened and summed here, as today, for every field.
- `query_with` and the other single-`FullText` entry points keep M151's refusal of legs over
  different fields: one `FullText` describes one field.

**The engine.**
- **`Part.terms` becomes `BTreeMap<String, Vec<String>>`**: each field's text legs' terms,
  analysed by that field's analyzer (`analyze_query`, M158). It is empty for a share in one
  exchange, as today.
- **`one_text_field` is replaced by `shareable_fields`.** A query with peers is offered shares
  when its text legs read no field, one field, or several fields **each of which is the
  primary or a declared further field**. A query naming a field the index never declared
  beside another stays unsplit, so its `UnknownTextField` costs no failed share, as today.
  `needs_texts` is true when any field the legs read is a further field, so a several-field
  query that is offered shares always carries `texts`.
- **The engine's half of the type change.** `Phased`'s `stats` future yields a `FieldStats`
  and its `scan` takes one (and `Peers::phased`'s doc says so); `Engine::open_part` returns a
  `FieldStats` and `Engine::scan_part` takes one. In `shares`, phase 2 is sent the global sum
  trimmed **per field**: each field's entry by `only` to that field's own terms.

**The wire** (`crates/pstore-server/src/peers.rs`).
- **Two new protocol numbers**, for a phased part whose text legs read two or more fields:
  - `PHASED_FIELDS = 7`, beside `PHASED_TEXTS = 5`;
  - `SUMMED_FIELDS = 8`, beside `SUMMED_TEXTS = 6`.

  Such a part always carries `texts`, since at most one of its fields is the primary.
- **At 7 and 8:**
  - `WirePart` carries `field_terms`, an object from field name to that field's terms, with
    two or more entries, and no `terms`;
  - phase 1 (`WireOpened`) answers `field_stats`, an object from field name to `WireStats`,
    with one entry for each of the part's `field_terms`;
  - phase 2 (`WireScan`) carries `field_stats` likewise;
  - the `stats` key of both is absent.
- **At every other number, the wire is today's, byte for byte.** A part over one field is
  sent at M154's number, with `terms` as today, and its `FieldStats` of one entry travels as
  today's one `stats`. The `field_terms` and `field_stats` keys are absent.
- **A one-field part's `terms`** are keyed, by `decode`, under the one field its text legs
  read. A peer's one `stats` reply is keyed the same way by the coordinator.
- **The coordinator checks every phase-1 reply.** A reply at 7 or 8 whose `field_stats` keys
  are not exactly its part's `field_terms` keys, or that carries `stats`, and a reply at 2, 3,
  5 or 6 without `stats` or with `field_stats`, is a failed share: counted in
  `pstore_peer_parts_failed`, and run by the coordinator. A share's statistics short of a
  field would leave its segments out of that field's corpus, a sum one share short.
- **The door.** A peer refuses `400 bad_part`:
  - a part at 7 or 8 with `terms`, without `field_terms`, with fewer than two entries in it, or
    with more than 9 (the primary and 8 further fields, M152);
  - a part at 7 or 8 without `texts`;
  - a `field_terms` entry naming a field no text leg of the part reads, or a text leg reading
    a field `field_terms` does not name;
  - a part at 1 to 6 with `field_terms`, or whose text legs read two or more fields;
  - a part at 7 with a `sum`, or at 8 without one;
  - a scan whose protocol is not its part's (7 for a part opened at 8 or 5, 8 for one opened
    at 7 or 6, 5 or 6 for one opened at 7 or 8);
  - a scan at 7 or 8 whose `field_stats` names a field other than its part's `field_terms`,
    or lacks one, or that carries `stats`; a scan at 2, 3, 5 or 6 that carries
    `field_stats`, or carries no `stats`.

  A server of an earlier build answers 7 and 8 with `409 protocol_mismatch`, and the
  coordinator runs that share itself, as for any other failed share.

**Unchanged:** every request, response and stored format; every query over one field or none,
whose parts and their bytes are M154's; the one-exchange share, which carries vector legs only.

## Acceptance criteria
1. **Phased shares over several fields.** Checked in the engine, over `split.rs`'s `Three`
   peers. The index declares `title`, whose analyzer (English, stemming) and BM25 `k1` and `b`
   differ from the primary's, and `tags`, with the default `FullText`. It holds enough folded
   segments to be split. Each of these queries makes at least one `open` and one `scan` call,
   and its answer equals the unsplit answer (`exactly`: every hit's segment, row and score
   bits):
   - RRF over a leg on the primary and a leg on `title`;
   - `Sum` over legs on all three fields, which splits through summed shares;
   - `Max` over legs on `title` and `tags`, beside a dense leg;
   - `Sum` over the primary and `title`, filtered by a token predicate over `title`.

   The rows hold the queried words in every field, at `df`s that differ by field by at least
   a factor of two, and the queries name the same word in two fields. So a sum that mixed
   two fields' statistics would give a different `df`, and a different answer. Each query's
   words are also inflected forms that only `title`'s analyzer stems to a term the rows hold,
   so terms analysed under another field's analyzer would find no `df` in `title`.
2. **One field is M154's.** `split.rs`'s M154 tests pass unchanged, and a query over one
   field, `title` alone, has a `Part.terms` of one entry, keyed `title`.
3. **Over HTTP, through real peers.** Over `peers.rs`'s three-server cluster, on an index
   created with `title` declared `{"language": "english", "stemming": true}` and `tags`
   declared `{"type": "[]string", "full_text_search": true}`, each of these answers exactly
   as the server alone, and is split (`pstore_peer_parts_sent` grows):
   - `{"text": [{"field": "text", ...}, {"field": "title", ...}]}` under RRF;
   - a `sum` over three legs, one on each field;
   - the first query with a filter `["title", "ContainsAnyToken", ...]`.

   No part fails (`pstore_peer_parts_failed` stays 0).
4. **The protocol numbers, and an old peer.** Against a fake peer that records each part's
   protocol and body and answers `409`, as a server of an earlier build does:
   - criterion 3's three queries are sent at 7, 8 and 7, each with `field_terms` of two or
     three entries and no `terms`;
   - a query over `title` alone is sent at 5, and over the primary alone at 2, each with
     `terms` and no `field_terms`;
   - each answers exactly as the server alone, and `pstore_peer_parts_failed` grows by one a
     query.

   Against a fake peer that answers phase 1 with `200`, its own build's well-formed reply
   **but for one field left out of `field_stats`**: criterion 3's first query answers exactly
   as the server alone, and `pstore_peer_parts_failed` grows by one. The same with a reply at
   5 that carries no `stats`, to a query over `title` alone.
5. **The door.** A real server answers `400` to each refusal the Delta lists, one case each,
   and `200` to a well-formed part at 7 (open, then scan) and at 8.

## Reversed criteria
Three earlier tests pin the refusal this milestone lifts:
- M153's `a_query_over_several_text_fields_is_not_split` (`split.rs`) becomes criterion 1's
  test: its several-field half asserts the query is split and answers as unsplit, and its
  primary half is unchanged.
- M153's `what_several_fields_cannot_mean_is_refused` (`several_fields.rs`) loses its third
  point, "a phased share of several fields is refused". Its other two points are unchanged.
- M151's `what_one_field_per_query_cannot_answer_is_refused` (`multi_text.rs`) loses its
  `open_part` refusal. Its `query_with` refusal and the rest are unchanged.

## Test plan
Red first against the parent, with the tests alone:
- 1, 3 and 4's several-field cases fail: the parent never splits them. 4's queries are
  answered by the server, so its fake peer records no part at 7 or 8.
- 5 fails: the parent answers 7 and 8 with `409`.
- 2 and 4's one-field cases are guards.

VERIFIED.md rests each test's power on these mutations, each observed red:
- 1: "`run` sums every share's statistics into the first field"; "`open_part` summarises
  every field from the first field's index"; "a field's terms are analysed under the
  primary's analyzer"; "`one_text_field` is kept".
- 2: "a one-field query's `Part.terms` holds an entry for the primary too".
- 3: "`WirePart` drops `field_terms`"; "phase 1 answers only the first field's statistics";
  "phase 2 is sent only the first field's statistics".
- 4: "a several-field part is sent at M154's number"; "a one-field part is sent with
  `field_terms`"; "the coordinator does not check a reply's `field_stats` keys"; "the
  coordinator does not check a one-field reply for `stats`".
- 1, 3 and 4 together: "`shareable_fields` admits an undeclared field". It is checked by a
  query over the primary and an undeclared field through the fake peer of 4: no part is sent,
  and the answer is `unknown_field`.
- 5: each refusal removed, one at a time.

The tests live in `crates/pstore-engine/tests/it/split.rs` (1, 2) and
`crates/pstore-server/tests/it/peers.rs` (3 to 5).

## RA budget
Unchanged in rounds: a share is two exchanges, as M154's. A part grows by its fields' terms,
and each phase by one `Stats` a field, at most 9, each about 20 bytes plus its terms' `df`s.
The requests and bytes a several-field query costs fall: its segments are scanned by the
servers that hold them rather than all by one.

## Risks
- **A rolling deploy.** Until every server speaks 7 and 8, a several-field query's shares
  sent to an older server run on the coordinator, which is today's cost. Nothing is answered
  wrongly.
- **Mixing two fields' statistics** gives a plausible answer and no error. Criterion 1's
  data makes every field's `df` for a shared word differ, so a mix is a different answer.

## Tasks
- M157.1: per-field statistics in the query layer, and the engine's `Part.terms` (1, 2).
- M157.2: the wire, its protocol numbers, and the door (3 to 5).
