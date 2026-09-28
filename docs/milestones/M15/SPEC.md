# M15 — `Glob`, `IGlob`, `Regex` and `Fuzzy`, pruned by a trigram sketch

## Serves

- The parity row for `Glob`, `IGlob`, `Regex` and `Fuzzy`
  ([`turbopuffer-api-parity.md`](../../research/11-design/turbopuffer-api-parity.md)), and
  "glob and regex (trigram-accelerated)" in
  [`filtering.md`](../../research/06-indexing/filtering.md).

## The decision this milestone makes

**The acceleration is a per-block trigram sketch, read with the zone maps. It is not an
inverted trigram index.** The corpus sketches "a second inverted index over character
trigrams" ([`full-text-search.md`](../../research/06-indexing/full-text-search.md)). But M9b's
rule is that a filter adds no round trip:
- An inverted index answers with rows, which costs a postings fetch between the open and the
  block reads. That is one more round than every other filter.
- A sketch per block lives in the index section, which the open round already fetches. It
  prunes blocks the way a zone map does, at no extra depth.

The corpus's sentence gains a correction banner that says this.

## M15.1 — the four filters, evaluated per row

### Delta

- **Filters**, all over a **scalar string** attribute. An array, a non-string value or a
  missing attribute is false.
  - `[attr, "Glob", p]` matches Unix glob: `*`, `?`, `[abc]`, `[a-z]`, `[!a]`, and `\` to
    escape. The whole value must match. `IGlob` is the same, case-insensitive.
  - `NotGlob` and `NotIGlob` are `Not` of the above.
  - `[attr, "Regex", r]` is the `regex` crate's syntax. It matches anywhere in the value, so a
    caller anchors with `^…$`.
  - `[attr, "Fuzzy", {"value": v, "max_edits": k}]` holds when the Levenshtein distance
    between the value and `v`, in characters, is at most `k`, for `k` in `0..=2`.
- **Compiled once, at the door.** A glob is translated to a regex. A pattern that does not
  compile, or whose compiled size exceeds the crate's 1 MiB limit, is refused with `400`.
  - `Predicate` gains `Pattern(attr, Matcher)` and `Fuzzy(attr, value, k)`. Their equality is
    by source text.
  - `condition.rs` gains tags for both. The encoding carries the source, and `decode`
    recompiles it.
- **Everywhere a filter runs**: queries, orders, aggregations, write conditions and
  by-filter operations. With no sketch, every block is read.

**Does not change:** any request count, or any other filter.

### Acceptance criteria

1. Over 2,000 rows of generated strings, each filter equals brute force. That brute force is
   the test's own glob matcher, the `regex` crate called directly, and the test's own
   Levenshtein. The patterns include:
   - glob wildcards, classes, negated classes and escapes;
   - `IGlob` against mixed case;
   - an unanchored and an anchored regex;
   - `Fuzzy` at k = 0, 1 and 2;
   - `Not` of each, and arrays and numbers, which admit nothing.
2. Refusals, each `400` naming the operator:
   - a regex that does not compile;
   - an unterminated `[` in a glob;
   - `max_edits` 3;
   - a non-string pattern.
3. Each filter survives `condition::encode`/`decode`, and decides a `delete_by_filter` at the
   fold as the query does.

## M15.2 — the trigram sketch

### Delta

- **Declared as the analyzer is (M14).** A write's `schema` accepts `{"<attr>": {"type":
  "string", "regex": true}}`.
  - It is carried to the fold as `$trgm`, the sorted names, and fixed in HEAD's schema at
    creation: `IndexSchema.trigram`, a trailing HEAD section like M14's.
  - A different declaration is `schema_conflict`, naming reindex and copy. An absent one has
    no opinion.
- **The sketch.** For each declared attribute in a block, a 2,048-bit Bloom filter over the
  character trigrams of the block's scalar string values, lowercased. There are two hashes
  per trigram (FNV-1a with two seeds).
  - It is written in the index section after the datetime tables, under **index flag 3**. A
    segment with no declared attribute keeps flag 1 or 2 and its exact bytes.
  - It is built at every seal the analyzer reaches: fold, compaction, fresh segment.
- **Pruning.** `could_admit` asks each pattern for its **required trigrams**, lowercased. A
  block whose sketch lacks any of them is skipped. The required trigrams are:
  - for a glob, those of each literal run between wildcards;
  - for a regex, those of each literal that every match must contain. They are found by a
    conservative walk of the parsed HIR:
    - concatenations of literals;
    - captures;
    - repetitions with a minimum of at least 1;
    - anything else requires nothing.
  - for `Fuzzy`, the q-gram lemma: a value within k edits shares at least
    `|trigrams(v)| − 3k` of `v`'s trigrams, so a block holding fewer is skipped.

  A pattern with no required trigram, or an undeclared attribute, prunes nothing.
- **Soundness is the criterion.** A sketch may say "maybe" wrongly (false positives), and must
  never say "no" wrongly.

### Acceptance criteria

1. **Sound.** Over 2,000 rows on a declared attribute, 300 generated patterns of every kind
   answer exactly as brute force. So do the same patterns on an undeclared attribute holding
   the same values.
2. **Prunes.** A selective glob, regex and fuzzy filter (≤ 1% of rows) on the declared
   attribute reads **fewer block bytes** than on the undeclared one, with equal answers and
   equal depth.
3. **Fixed at creation.** A differing declaration is `400 schema_conflict`, an undeclared
   write is accepted, and the declaration survives compaction and a new process.
4. **Format.** A segment without a declared attribute is byte-for-byte what M14 wrote. A
   flag-3 index section round-trips, and a truncated one is refused.
5. `./scripts/gates.sh` passes, and `./scripts/mutants.sh` over the diff misses 0.

## Test plan

| Criterion | Fails first | Mutation it catches |
|---|---|---|
| 15.1.1 | the operator is refused | glob anchoring dropped; `IGlob` case-sensitive; the fuzzy bound off by one |
| 15.1.2 | as 15.1.1 | a refusal accepted |
| 15.1.3 | as 15.1.1 | a tag missing; the pattern re-parsed differently |
| 15.2.1 | `regex: true` is refused | a trigram required that is optional (an alternation, `*`), which prunes a matching block |
| 15.2.2 | as 15.2.1 | the sketch not consulted; not written |
| 15.2.3 | as 15.2.1 | `$trgm` ignored by `implied` or the door |
| 15.2.4 | a flag-3 segment does not exist | flag 3 written for no declaration |

## RA budget

Unchanged in requests and depth. The index section grows by 256 bytes per block per declared
attribute, and it is read in the open round a query already pays for.

## Risks

- **A sketch saturates** on long values: a block of 64 rows of 1 KB strings sets most of its
  2,048 bits and prunes nothing. That is sound, and slower, and criterion 15.2.2 uses short
  values. Nothing reports a saturated sketch.
- **Regex literal extraction is conservative**, so a regex whose literals sit inside an
  alternation prunes nothing.
- **Unicode case.** `IGlob` uses Unicode simple case folding, and the sketch lowercases.
  A character whose uppercase and lowercase forms differ in length is matched correctly per
  row and may be pruned wrongly. Criterion 15.2.1 includes `ß` and `İ` to find out.

## Tasks

- **M15.1** — the four filters, evaluated per row.
- **M15.2** — the trigram sketch.
