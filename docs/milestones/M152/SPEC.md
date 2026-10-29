# M152: an index's further text fields, in the engine

## Serves
Workstream E of [the delivery plan](../../comparisons/turbopuffer-next-plan.md) ("multiple
string/string-array BM25 fields"). This is the second of three steps; [M151](../M151/SPEC.md)
made a segment able to carry several text fields, and a query read one of them. Here the
engine declares, records, seals, merges and queries them. M153 lets one query read several,
and M155 adds the HTTP declaration. No D- or OQ- id covers it.

## Delta
Today an index has one text field:
- `IndexSchema.text_field` names it and `IndexSchema.fts` analyzes and scores it;
- it is filled by the first fold with text from the writer's own field (M30, M36), and a row
  under another writer's field is refused (`SchemaConflict { what: "the text field" }`);
- a write may declare the analyzer (`Declared.fts`) and the trigram set (`Declared.trigram`),
  carried on each row (`$fts`, `$trgm`), checked at the door against this process's unfolded
  rows (`door_conflict`), recorded by the first fold (copied from the row that created the
  index, M14), and refused when they disagree with the record.

After, beside that field (**the primary**), whose fill, stamp and conflict are unchanged
except where named:

**Declared.** `Declared.texts: Option<BTreeMap<String, FullText>>`: an index's **further** text
fields, each with its own analyzer and BM25 parameters.
- It is carried on each row as `$texts`, an array of strings: each name, then its
  `FullText::encode()`, in name order. It is stripped before storage, as `$fts` and `$trgm`
  are.
- Refused at the door (`EngineError::Refused`, naming the reason):
  - more than 8 names;
  - an empty name, a name over 128 bytes, a reserved name (`$...`), or `id`;
  - a name equal to the writer's own text field.
- It is checked against this process's pending and unfolded rows by `door_conflict`, as
  `fts` and `trigram` are, and an index's record is checked at the door as theirs is.

**Recorded.** `IndexSchema.texts: BTreeMap<String, FullText>`, empty for none.
- It is filled by the first fold from the first row that declares it, and copied from the
  row that created the index, exactly as `trigram` is.
- It is immutable afterwards. A row declaring another set, including none when one is
  recorded, is refused (`SchemaConflict { what: "the further text fields" }`). A row
  declaring nothing has no opinion.
- **No name is both.**
  - `row_conflict` refuses a row whose text is under a further field of the schema
    (`text_of(row) ∈ texts`), with `SchemaConflict { what: "the text field" }`. So `filled`
    never fills the primary with a further field's name, and the fold that records both at
    once applies the same rule to its own rows.
  - A patch is rowless and passes `row_conflict`, so both fills skip a field in `texts`:
    `filled`, and the fill after `resolve` that reads merged rows. A patch setting text under
    a further field leaves the primary as it was.
  - The fresh view leaves out of `[primary] + texts` a primary that names a further field. Its
    primary may be this engine's own field while the schema has none.
- **In HEAD**, a new last optional section, written only when some schema (live or dropped)
  has further fields. A HEAD without any is byte for byte what the parent's encoder writes.
  - Per schema, addressed as the full-text section addresses one: the count, then each
    name and its `FullText::encode()`.
  - It forces every earlier optional section by the existing rule. `keys_since` has no
    presence marker. So when it is `None` and forced, it is written as `u64::MAX`, and the
    decoder reads `u64::MAX` as `None` wherever it appears. The encoder never writes
    `Some(u64::MAX)`: it is an epoch, and epochs advance by one a commit. A `debug_assert!`
    in `encode` states it.
  - Decoding refuses (`CorruptHead`):
    - a section that does not decode whole;
    - more than 8 names, an empty, reserved or repeated name;
    - a name equal to its schema's `text_field`.

**Sealed.** Every seal (fold, compaction, the fresh view) builds the fields
`[primary] + texts`, in name order.
- The primary is today's field, when it has text (M150's `has_text`), under `fts`. Each
  further field is built under its own `FullText`'s analyzer.
- `try_build_all_fields` (M151) carries only those with postings.
- With no further fields this is a one-element list, which M151 shows writes today's bytes.

**The fresh view.**
- It takes `texts` from the schema, and before the first fold from the first visible row's
  `$texts`, as it takes `fts` and `trigram`.
- Its cache key includes `texts`, so a view built before another process's fold recorded
  them is rebuilt.

**Merged.** Compaction's "inputs whose text fields disagree are refused" check ignores the
schema's further fields, then judges the primary as today. The merge rebuilds every field
from the rows.

**Queried.** A query's text legs name one field (M151 refuses two). The engine distinguishes
two `FullText`s:
- **the filter's**, which is always `fts`. It binds token predicates (`bound`), as today, and
  is `Answer.fts`, which the server uses for highlights of the primary;
- **the scoring one**, the queried field's: `texts[field]` for a further field, else `fts`.
  - It is passed to `pstore_query` on every query path: live, `as_of` and the fresh view
    (from the view's own `texts` before the first fold).
  - **A query whose scoring `FullText` is not `fts` is never split.** The engine builds no
    `Elsewhere` share for it and runs every segment itself, which is correct and costs only
    distribution. A share's `Part` has one `FullText`, used both to bind the filter and to
    score. Carrying a second one is a peer-protocol change, for phased and for summed parts
    alike, and is [M154](../M154/SPEC.md)'s.

**Unchanged:**
- the primary's fill, stamp (M36, M37) and conflict for rows under the primary;
- the segment format (M151), the query layer, requests and round trips;
- the server's requests. It declares no further field until M155, and its `Declared` literals
  gain `texts: None`;
- the peer wire and every query over the primary, split as today.

## Acceptance criteria
1. **Twins.**
   - An index `multi` declares further fields:
     - `title`: English stemming and stopwords, `k1 = 2.0`, `b = 0.3`;
     - `tags`: case-sensitive, `k1 = 0.8`, `b = 1.0`.

     Its primary is `text`, under the default `FullText`.
   - For each of `text`, `title` and `tags` there is a twin index whose primary is that field
     alone. A writer over that field writes it, under the same `FullText`.
   - The same rows go to all of them: strings, arrays, absent fields, and rows where a field
     holds only stopwords.
   - For each field and a fixed set of queries, the text query over `multi` returns the same
     ids, order and scores (to 1e-6) as over the field's twin. This is checked at each point:
     - before the first fold (the fresh view, with no schema yet);
     - after the fold;
     - after more writes (fresh rows beside a segment) and a second fold;
     - after a compaction merging the segments;
     - `as_of` the first fold's epoch, after the compaction. `multi` and its twins live in one
       tenant and are folded by the same folds, so one epoch names the same point for all.
2. **Recorded.**
   - After `multi`'s first fold, HEAD's schema for it has `texts` equal to what was declared,
     and it survives a HEAD encode and decode.
   - `multi` is dropped and re-created with other further fields. Its dropped schema keeps
     the first set, read back through a HEAD encode and decode, and `as_of` the dropped
     epoch scores `title` as before.
   - A HEAD with no further fields encodes to the bytes the parent's encoder writes. Fixed
     HEADs are compared with bytes captured from the parent's encoder: one with
     `keys_since: None` and one with `Some`. Each carries every earlier optional section: a
     non-default `fts`, a trigram set, named widths and a dropped schema. VERIFIED.md records
     the commit and command the bytes were captured with.
   - A HEAD with further fields and `keys_since: None` decodes with `keys_since: None`.
3. **Refused.**
   - A later write to `multi` declaring a different set is
     `SchemaConflict { what: "the further text fields" }`: a name added, a name removed, and
     an analyzer changed.
   - Before any fold, a second write in the same process declaring a different set is
     refused at the door.
   - Each door refusal in the Delta is refused.
   - A row declaring nothing is accepted.
   - **No name is both.** A writer over `title` writes text into `multi` without declaring
     anything:
     - after the first fold, it is refused with `SchemaConflict { what: "the text field" }`;
     - in the very first fold, beside the declaring writer's rows, with the primary still
       empty, it is quarantined and never fills the primary;
     - with the primary still empty after a fold of rows with no primary text, a patch from
       that writer sets `title` text on an existing row.

     In every case the fold succeeds, the primary is never `title`, and a query over `title`
     on that writer's own engine answers.
4. **Not split.** Through `split.rs`'s phased shares, a text query over `title`, with and
   without a token filter over the primary, makes no peer call and answers exactly as the
   same query on an engine with no peers. A query over the primary is still split, as today.
5. **HEAD decode.** A HEAD carrying the new section, truncated at every byte inside it,
   decodes as `CorruptHead`. So does one carrying each impossible record the Delta lists.

## Test plan
Red first against the parent:
- 1, 3, 4 and 5, and 2's further-field half, do not compile: `Declared.texts` and
  `IndexSchema.texts` are absent.
- 2's no-further-fields half is green on the parent, and is a guard.

A compile failure shows only that the API is new, so VERIFIED.md rests each test's power on
the mutations below, each observed red.
- 1: `crates/pstore-engine/tests/it/multi_text.rs`. Mutations:
  - "the seal builds the primary only";
  - "the fresh view builds the primary only";
  - "the fresh view's cache key ignores `texts`";
  - "before the first fold the view takes no `texts`";
  - "compaction refuses further fields as disagreeing";
  - "a further field is built under `fts`";
  - "the live query scores under `fts`";
  - "`as_of` scores under `fts`";
  - "the fold does not copy `texts` from the creating row".
- 2, 5: `crates/pstore-engine/src/head.rs` unit tests. Mutations:
  - "the section is always written";
  - "a forced `None` is written as 0";
  - "a truncated section decodes as fewer fields";
  - "the dropped schemas' further fields are not written".
- 3: `multi_text.rs`. Mutations:
  - "a different set is accepted";
  - "`door_conflict` ignores `texts`";
  - "a row under a further field is accepted";
  - "the fill after `resolve` takes a further field";
  - "the fresh view keeps a primary naming a further field";
  - "a name equal to the writer's field is accepted".
- 4: `split.rs`. Mutations: "a further-field query is split", "a primary query is not
  split".

## RA budget
Unchanged in requests and rounds:
- a seal writes one segment and one sidecar, whatever the field count (M151);
- a query reads one sidecar per segment.

The new costs are bytes:
- `$texts`, about 35 bytes plus the name per field, rides on each row of a declaring write,
  in the WAL and the memtable;
- HEAD grows by about the same per field of each index declaring some, and every query reads
  HEAD;
- the sidecar carries each further field's dictionary.

The cap of 8 fields bounds all three.

## Risks
- **Readers first.** The parent's decoder refuses any byte after `keys_since`, so a node of an
  older build cannot read a tenant whose HEAD carries the new section. Nothing declares
  further fields before M155's server does, and M155 must not until every node runs M152.
  The delivery plan requires readers-first deployment.
- **Fixed at creation.** Further fields are recorded once, as the analyzer and the trigram
  set are. An index created before M152 has none and can never gain one. Adding a field
  means copying the rows into a new index, the migration path the refusals of `fts` and
  `trigram` already name. This is deliberate: a field added later would leave every earlier
  segment without its postings until a rewrite. M155 inherits it, and refuses a vendor schema
  update that adds `full_text_search` to an existing index, with that path named.
- **The filter analyzer is the primary's.** A token filter over a further field analyzes with
  `fts`, so M138's token filters may tokenize a further field differently from its BM25.
  M155 owns per-attribute declarations and resolves it.

## Tasks
- M152.1: `Declared.texts`, `IndexSchema.texts`, the HEAD section, the door, `door_conflict`
  and the conflicts (2, 3, 5).
- M152.2: seal, fresh view, compaction, the query's two `FullText`s, and not splitting a
  further-field query (1, 4).

Split out after the second spec review: M154, phased and summed shares scoring a further
field, under new peer protocol numbers.
