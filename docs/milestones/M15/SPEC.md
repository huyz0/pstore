# M15 — `Glob`, `IGlob`, `Regex` and `Fuzzy`, pruned by a trigram sketch

## Serves

- The parity row for `Glob`, `IGlob`, `Regex` and `Fuzzy`
  ([`turbopuffer-api-parity.md`](../../research/11-design/turbopuffer-api-parity.md)).
- "glob and regex (trigram-accelerated)" in
  [`filtering.md`](../../research/06-indexing/filtering.md).

## The decision

**A per-block trigram sketch in the segment's meta region, read by the open round, and not an
inverted index.** An inverted index answers with rows, which costs a postings fetch between the
open and the block reads. That is a round M9b's rule forbids a filter.
[`full-text-search.md`](../../research/06-indexing/full-text-search.md) carries the correction
banner.

## M15.1 — the filters, evaluated per row

### Delta

All filters here apply to a **scalar string**, including `id`. An array, a non-string or an
absent attribute is false.

- **Glob.** `[attr, "Glob", p]` and `IGlob` compile once, at the door, to the regex
  `(?s)\A…\z`, with `(?i)` added for `IGlob`.
  - `*` is `.*` and crosses `/`; `?` is `.`.
  - `[abc]`, `[a-z]` and `[!a]` are classes, and `\` escapes.
  - `NotGlob` and `NotIGlob` are `Not` of these.
- **Regex.** `[attr, "Regex", r]` uses the `regex` crate's syntax and is unanchored. The
  compile limit is set explicitly: `RegexBuilder::size_limit(1 << 20)`, with a pattern of at
  most 4,096 bytes.
- **Fuzzy.** `[attr, "Fuzzy", {"value": v, "max_edits": k}]` holds when the Levenshtein
  distance in characters is at most `k`, for `k` in `0..=2` and `v` of at most 256 characters.
  It computes a band of width `2k+1` and exits early when the lengths differ by more than `k`,
  so a row costs `O(|value|·k)`.
- **Predicates.** `Predicate` gains `Pattern(attr, kind, source, compiled)` and
  `Fuzzy(attr, v, k)`.
  - Equality is by `(kind, source)`.
  - `condition.rs` carries the kind and source, and `decode` recompiles.
  - A pattern that no longer compiles at the fold skips its operation, as an unreadable
    condition does. That is stated, not new.
- **Where they run:** everywhere a filter runs. A filter holds at most 16 patterns, and the
  fold compiles each distinct source once. The retriever-shaped `Prefetch::Trigram` stays
  refused, and its comment drops the inverted index.
- **New dependency:** `regex`, with default features off and `std` and `unicode` on, pulling in
  `regex-automata` and `regex-syntax` (MIT or Apache-2.0).

### Acceptance criteria

1. Over 2,000 generated strings, each filter equals brute force: the test's own glob matcher,
   the `regex` crate, and the test's own Levenshtein. The strings include `\n`, `/`, and every
   character listed under M15.2's fold.
   - The patterns cover classes, negation, escapes, `IGlob` against mixed case, anchored and
     unanchored regex, `Fuzzy` at k = 0, 1 and 2, `Not` of each, `id`, arrays and numbers.
2. Refusals, each `400` naming the operator:
   - a regex that does not compile, and one past the size limit;
   - an unterminated `[` in a glob;
   - `max_edits` 3, and `v` of 257 characters;
   - a non-string pattern.
3. Each filter survives `condition::encode`/`decode`, and decides a `delete_by_filter` at the
   fold as the query does.

## M15.2 — the trigram sketch

### Delta

- **The fold.** Every string is folded per character by **simple case folding**: `N(c)` is
  the smallest code point in `c`'s simple-case-folding class, from `regex-syntax`'s tables.
  That is exactly the relation `(?i)` uses.
  - It is one character to one character, so a trigram is three folded Unicode scalar values.
  - An edit touches at most three trigrams.
  - One fold serves `Glob`, `IGlob`, `Regex` and `Fuzzy`. For a case-sensitive pattern, `N`
    of a literal is still `N` of every value that contains it.
- **The sketch.** Each block has a Bloom filter per declared attribute over its values'
  trigrams:
  - hashed by FNV-1a-64 over the trigram's UTF-8, seeded with `0` and with `0x9e37_79b9`;
  - `bits` a power of two from 64 to 2,048, with 2 hashes.

  A block where the attribute is absent, or holds no string, gets an all-zero filter: a false
  filter prunes it soundly.
- **Where it lives:** a new section id, `TrigramSketch`, in the meta region beside the
  `Fields` and text-field tables.
  - Old readers record unknown ids and ignore them.
  - The layout is the attribute names, `bits`, then the block filters.
  - It gets **only what is left** of the index budget after the block index fits, **less its
    own directory entry**. `bits` halves until it fits, and below 64 the section is omitted.
    So a declaration never coarsens blocks, and never costs another filter its zone maps.
  - It is built at every seal: fold, compaction, and the fresh segment.
- **Pruning** is keyed on **this segment having a sketch for this attribute**, never on HEAD's
  declaration. A block is skipped only when its filter lacks a required trigram. What is
  required:
  - **Glob:** the trigrams of each literal run. A wildcard, `?` or class ends a run.
  - **Regex:** from the HIR, a literal run is a maximal concatenation of literals. A
    repetition, class, alternation, look-around or empty node **ends the run** and never
    joins its neighbours: `abc(d)+ef` requires `abc` and nothing from `ef` or across the join.
    Under `(?i)` literals become classes, so nothing is required.
  - **Fuzzy:** at least `|distinct trigrams(v)| − 3k` of `v`'s trigrams must be present; a
    bound of 0 or less prunes nothing.
- **Declared as M14's analyzer is.** A write's `schema` takes `"regex": true` on a string
  attribute, the text field included, beside `full_text_search`.
  - `$trgm` carries the write's **whole declared set**, sorted, and HEAD's schema holds it as
    `IndexSchema.trigram`.
  - A declaring write must equal the recorded set, or it is `schema_conflict`, naming reindex
    and copy. A write declaring nothing has no opinion. `"regex": false` is not a
    declaration, and `id` cannot be declared.
  - In HEAD, a trailing trigram section follows M14's, and **M14's count is always written**,
    0 if empty, whenever a trigram section follows.

**Does not change:** requests, depth, block sizes, or any other filter's pruning.

### Acceptance criteria

1. **Sound.** Over 2,000 rows, 300 generated patterns of every kind answer as brute force, on
   a declared attribute and on an undeclared one holding the same values. The values include
   `ſ`, `ς`/`Σ`, `µ`/`μ`, `K` (U+212A) and `İ`.
2. **Prunes.** A glob, a regex and a fuzzy filter each matching one row read fewer block bytes
   on the declared attribute than on the undeclared one, with equal answers and equal depth.
3. **Budget.** A declared segment has the same block count as the same rows undeclared.
4. **Fixed at creation.** A differing set is `400 schema_conflict`, and an undeclared write is
   accepted. The set survives compaction, a new process, and HEAD round-trips with and
   without M14's section.
5. **Format.** An undeclared segment is byte-for-byte M14's, and a truncated sketch is refused.
6. `./scripts/gates.sh` passes, and `./scripts/mutants.sh` misses 0.

## Test plan

| Criterion | Fails first | Mutation it catches |
|---|---|---|
| 15.1.1 | the operator is refused | glob anchor or `(?s)` dropped; `IGlob` case-sensitive; the fuzzy band off by one |
| 15.1.2 | as 15.1.1 | a limit not enforced |
| 15.1.3 | as 15.1.1 | a tag missing; the kind dropped from equality |
| 15.2.1 | `regex: true` is refused | joining a literal run across `(d)+`; lowercasing in place of `N`; `−3k` as `−2k`, found by a lone row at distance k with its edits spread out |
| 15.2.2 | as 15.2.1 | the sketch not consulted, or not written |
| 15.2.3 | as 15.2.1 | the sketch counted before the index fits |
| 15.2.4 | as 15.2.1 | `$trgm` ignored; M14's count omitted before the trigram section |
| 15.2.5 | a sketch section does not exist | a section written with nothing declared |

## RA budget

Unchanged in requests and depth: the sketch rides the suffix fetch the open round already makes.

## Risks

- **At scale the sketch is mostly absent.** Near 20,000 rows the block index leaves 0–4 KiB,
  so a compacted segment gets 64–128 bits per block, which saturates, or no sketch at all.
  The acceleration serves small and freshly folded segments. That is sound, only slower, and
  nothing reports it.
- **Mixed versions.** A pre-M15 process that rewrites HEAD drops the trigram section, as M14's
  risk says of its own.
- **Regex extraction is conservative**: `(?i)` and alternations prune nothing.

## Tasks

- **M15.1** — the filters, evaluated per row.
- **M15.2** — the trigram sketch.
