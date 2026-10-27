# M154: splitting a query that reads a further text field

## Serves
Workstream E of [the delivery plan](../../comparisons/turbopuffer-next-plan.md) ("multiple
string/string-array BM25 fields"). [M152](../M152/SPEC.md) answers a query that scores a
further text field correctly by never splitting it. [M156](../M156/SPEC.md) does the same
for a query whose filter holds a token predicate over a further field. This milestone splits
both, so a further field costs what the primary costs in distribution. No D- or OQ- id
covers it.

**Out of scope, and unchanged:** a query whose text legs read two or more fields. A share's
phase 1 and phase 2 carry one `Stats`, for one field, so such a query stays unsplit
([M153](../M153/SPEC.md)). That is [M157](../M157/SPEC.md).

## Delta
Today a share's `Part` carries one `FullText`, the index's primary. It analyses the query's
terms, scores the text legs, and binds the filter's token predicates. `one_part` refuses
shares for any query that would need another.

After:
- **`pstore_engine::Part` gains `texts: BTreeMap<String, FullText>`.** It holds the index's
  further fields when the query needs them, and is empty otherwise. A query needs them when
  its text legs read a further field, or its filter holds a token predicate over one
  (`Predicate::token_fields`, M156).
- **A peer reads `texts` as the coordinator does.** `part` and `scan_part` bind the filter with
  `bound_texts(&part.fts.analyzer, &part.texts)`. Both score the text legs under the
  `FullText` of the field those legs read: `part.texts`'s entry, else `part.fts`. To do so,
  `pstore_query::part` and `scan_part` take a `&Texts` (M152's, `{default: part.fts, fields:
  part.texts}`) where they take a `&FullText` today, and score through `Texts::of`, as the
  unsplit query does.
- **The coordinator analyses `Part.terms` under that same field's analyzer.** Those are the
  terms phase 1 counts and phase 2's sum carries.
- **`one_part` refuses shares only for a query whose text legs read two or more fields.** Its
  other two refusals, a field scored under another `FullText` (M152) and a token predicate over
  a further field (M156), are removed.
- **The wire.** `WirePart` gains `texts`, an object from field name to `FullText::encode()`,
  omitted when empty. A part that carries `texts` is sent at a new protocol number, one for
  each of today's three exchanges, in `crates/pstore-server/src/peers.rs`:
  - `WHOLE_TEXTS = 4`, beside `PROTOCOL = 1`;
  - `PHASED_TEXTS = 5`, beside `PHASED = 2`;
  - `SUMMED_TEXTS = 6`, beside `SUMMED = 3`.

  A scan speaks its part's protocol. A part without `texts` is sent at today's number and
  serializes byte for byte as today.
- **The peer's door.** A part at 4, 5 or 6 with no `texts` is refused `400 bad_part`, as is a
  part at 1, 2 or 3 with `texts`, a `texts` entry that does not decode, and more than 8
  entries. M58's coupling of a cut by sum to protocol 3 extends to 6, and a scan's protocol
  must be its part's. A server of an
  earlier build answers 4, 5 and 6 with `409 protocol_mismatch`, and the coordinator runs that
  share itself, as for any other failed share.

**Unchanged:** every request, response and stored format; a query over the primary with no
token predicate over a further field, whose parts and their bytes are today's; a query over
several text fields, which is not split.

## Acceptance criteria
1. **Phased shares score a further field.** This is checked in the engine, over
   `split.rs`'s `Three` peers. The index declares `title` with a `FullText` whose analyzer
   (English, stemming) and BM25 parameters (`k1`, `b`) both differ from the primary's. Its rows
   carry `title` text, and it holds enough folded segments to be split. Each of these queries
   makes at least one `open` and one `scan` call, and its answer equals the unsplit answer
   (`exactly`: every hit's segment, row and score bits):
   - a text leg over `title` under RRF;
   - two text legs over `title` under `Sum`, which splits through summed shares;
   - a text leg over `title` and a dense leg;
   - a text leg over `title` filtered by a token predicate over the primary.

   Each query's words are inflected forms ("running") that only `title`'s analyzer stems to a
   term the rows hold ("run"). So terms analysed under the primary's analyzer would find no
   `df`, and the answer would differ. The fourth query's filter over the primary also uses
   words the two analyzers read differently: a stopword or an inflected form the primary keeps
   and `title`'s analyzer drops or stems, chosen so that binding it under `title`'s analyzer
   admits a different set. It admits some rows and refuses others.
2. **Token filters over a further field split.** On the same index:
   - a vector-only query filtered by a token predicate over `title`, at the top level and under
     `Not`, makes at least one `part` call;
   - a text leg over the primary with the same filters makes at least one `open` call.

   Each answers exactly as unsplit. The filter's words are inflected forms that only `title`'s
   analyzer matches, and each filter admits some rows and refuses others.
3. **Several fields stay unsplit.** M153's `a_query_over_several_text_fields_is_not_split`
   passes unchanged.
4. **Over HTTP, through real peers.** Over `peers.rs`'s three-server cluster, on an index
   created with `title` declared as `{"language": "english", "stemming": true}`, each of these
   queries answers exactly as the server alone, and is split (`pstore_peer_parts_sent` grows):
   - `{"text": [{"field": "title", "query": ...}]}`;
   - a `sum` over two `title` legs;
   - a vector query filtered by `["title", "ContainsAnyToken", ...]`;
   - a text query over `text` with that filter.

   No part fails (`pstore_peer_parts_failed` stays 0).
5. **The protocol numbers, and an old peer.** Against a fake peer that records each part's
   protocol and answers `409`, as a server of an earlier build does:
   - the four queries of criterion 4 are sent at 5, 6, 4 and 5;
   - the same index's queries over the primary with no token filter over `title` (a text query,
     a `sum`, a vector query) are sent at 2, 3 and 1, with no `texts` key;
   - each answers exactly as the server alone, and `pstore_peer_parts_failed` grows by one a
     query.
6. **The door.** A real server answers `400` to:
   - a phase-1 part at 5 or 6, or a whole part at 4, with no `texts`;
   - a part at 1, 2 or 3 with `texts`;
   - a part at 5 whose `texts` entry does not decode, or with more than 8 entries (the most an
     index declares, M152);
   - a part at 5 with a `sum`, at 6 without one, and at 4 with one;
   - a scan whose protocol is not its part's: 2, 3 or 6 for a part opened at 5, 5 for a part
     opened at 2, and 3 or 5 for a part opened at 6.

   It answers `200` to a well-formed part at 4, 5 (open, then scan) and 6.

## Reversed criteria
Two earlier tests pinned "not split", a limit their specs named as this milestone's to lift:
- M152's `a_query_over_a_further_text_field_is_not_split` becomes criterion 1's test;
- M156's `a_token_filter_over_a_further_field_is_not_split` keeps its primary half and now
  asserts the further-field half is split.

## Test plan
Red first against the parent, with the tests alone:
- 1, 2, 4 and 5's further-field cases fail: the parent never splits them.
- 6 fails: the parent answers 4, 5 and 6 with `409`.
- 3, and 5's primary cases, are guards.

VERIFIED.md rests each test's power on these mutations, each observed red:
- 1: "the peer scores under `part.fts`"; "the coordinator analyses terms under the
  primary's analyzer"; "`texts` is never filled"; "a token predicate over the primary is
  bound under the queried field's analyzer".
- 2: "`Engine::part` binds the filter with `bound`"; "`Engine::scan_part` binds the filter
  with `bound`"; "`texts` is filled only for a text leg over a further field".
- 4: "`WirePart` drops `texts`"; "a scan of a part with `texts` is sent at today's number".
- 5: "a part with `texts` is sent at today's number"; "`texts` is always filled".
- 6: each refusal removed, one at a time.

The tests live in `crates/pstore-engine/tests/it/split.rs` (1 to 3) and
`crates/pstore-server/tests/it/peers.rs` (4 to 6).

## RA budget
Unchanged in rounds: a share is one exchange or two, as today. A part over a further field
grows by its `texts`, at most 8 entries of about 30 bytes. The requests and bytes a further
field's query costs fall: they are spread over the servers that hold its segments, rather
than run by one.

## Risks
- **A rolling deploy.** Until every server speaks 4 to 6, a further field's shares sent to an
  older server run on the coordinator, which is today's cost. Nothing is answered wrongly.
- **Two analyzers per part.** The filter binds under each field's own analyzer, and the
  score under the queried field's. A peer that mixed them would answer wrongly without
  failing. Criteria 1 and 2 choose words on which the two analyzers disagree, so a mix is a
  different answer.

## Tasks
- M154.1: `Part.texts`, the peer's bind and score, the coordinator's terms, and `one_part`
  (1 to 3).
- M154.2: the wire, its protocol numbers, and the door (4 to 6).
