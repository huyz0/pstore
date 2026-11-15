# M260: BM25 `k3`, query-term frequency saturation

## Serves
The [turbopuffer gap list](../../comparisons/turbopuffer-gaps-2026-11-15.md), "Older gaps
still open": BM25 `k3`. Workstream E of [the delivery plan](../../comparisons/turbopuffer-next-plan.md).
No D- or OQ- id covers it.

Today a BM25 leg scores each distinct query term once: `pstore-index`'s `row_score` and
`TextIndex::search_prefixed` both deduplicate the analyzed query terms, so `"puffer puffer
fish"` scores as `"puffer fish"`. That is the limit `k3 → 0` of BM25's query-term weight.

## Delta
1. **`full_text_search.k3`**, a number in `(0, 1000]`, beside `k1` and `b`, on any text
   field (primary or further, M151). Unset by default.
2. **When set**, each distinct query term's BM25 contribution is multiplied by
   `w(qtf) = ((k3 + 1) · qtf) / (k3 + qtf)`, where `qtf` is how many times the analyzed query
   holds the term, computed in `f32` in that order, and applied as `term_score(..) * w`. A
   `last_as_prefix` token is a prefix, not a term, so it does not count toward any `qtf`. A term held once has `w = 1` exactly, so **a query with no repeated term
   scores bit for bit as without `k3`**. The weight applies wherever a BM25 leg is scored:
   postings (`search_prefixed`), row rescoring (`row_score`, used by `rank_by` expressions,
   M182), every text field, split or not (M154, M157). `last_as_prefix`'s 1.0 is unchanged.
3. **When unset**, scoring is exactly today's: each distinct term once.
4. **Fixed per index, like `k1` and `b`**: part of the field's `FullText`, recorded in its
   schema, compared by bits, a differing declaration refused `schema_conflict`, and read
   back by `GET /v1/indexes/{index}` as `k3` only when set. A conflict's message names
   `k3` when either side has one.
5. **A new schema line, so an older build refuses it.** A field with `k3` is encoded as
   `k3:<bits as 8 hex>:` followed by the line it would otherwise have (which may itself be a
   `v4:` line). No earlier grammar reads that prefix, so a build before M260 refuses a
   **HEAD** holding it (`CorruptHead`). A field without `k3` keeps its line byte for byte.
   The same line stamps a write's rows (`$fts`, `$texts`); see Risks for what an older
   build does with such a stamp.
   `decode` reads the prefix only with bits of a value in range; `k3:` with any other value
   is refused, so each schema has one spelling.

**No setting.** `k3` is a per-index declaration whose absence is today's behaviour, and
which changes nothing for queries without a repeated term, so the cluster-default rule has
nothing to switch (the gap list's settings note). The default stays unset; the vendor's 8.0
is the facade's to declare, not the native default.

**Not changed:** `k1`, `b`, idf, the analyzer, prefixes, any other ranking, the request
count of any query.

## Acceptance criteria
1. **Oracle.** `k3_weights_repeated_query_terms`, in `crates/pstore-index/tests/it/`: over a
   fixed corpus, for `k3 ∈ {0.5, 8, 1000}`, `search_prefixed`'s and `row_score`'s scores for
   `"a a b"` equal a hand-written `term_score(..) * w` with Delta 2's `w`, to the bit, and the
   two paths agree; and `"a a"` with `last_as_prefix` scores `a` with `qtf = 1`.
2. **Unset is today.** `k3_unset_scores_each_term_once`: with `k3` unset, `"a a b"` scores
   exactly as `"a b"` on both paths.
3. **No repeat, no change.** `k3_without_repeats_is_bit_identical`: for each `k3` above, a
   query with no repeated term scores bit for bit as with `k3` unset.
4. **Encoding.** `k3_lines_are_refused_by_older_grammars`, in
   `crates/pstore-format/tests/it/`: a field with `k3` round-trips over the v1, v2, v3 and
   `v4:` lines, starts `k3:`, and its inner line alone decodes to the field without `k3`;
   a line with `k3:` and a value out of range, or `k3:` around a line that does not
   decode, is refused; a field without `k3` encodes to its pre-M260 line.
5. **HTTP.** `k3_over_http`, in `crates/pstore-server/tests/it/analyzer.rs`: an index
   declared `{"k3": 8}` answers `"puffer puffer fish"` with scores equal to the oracle and
   ranks a row with `puffer` above one with only `fish` where unset `k3` ties them as
   `"puffer fish"` would; the schema reads back `k3: 8`; a re-declaration with another
   `k3` is refused `schema_conflict`; `k3` of `0`, `-1`, `1001` and `"8"` are refused 400.
6. **Expressions.** `k3_in_rank_by_expressions`: a `rank_by` `Sum` of the BM25 leg equals
   the plain BM25 leg's score under `k3` (M182's exactness held).
7. **Gates:** fmt, clippy, the workspace tests (the two IPv6 tests NOT-RUN in this
   container), and the named mutations below observed red.

## Test plan
Each test is seen red first against the parent, where `k3` is an unknown option.

| AC | Mutation |
|---|---|
| 1 | weight `(k3+1)/(k3+qtf)` (qtf dropped); weight applied on one path only |
| 2 | `qtf` counted when `k3` is unset |
| 3 | weight `(k3+1)/k3` for every term; `k3` replacing `k1` in `term_score` |
| 4 | prefix not written; out-of-range bits accepted |
| 5 | `k3` not compared in the schema; range check `>= 0`; `describe` omitting `k3` |
| 6 | `row_score` passed no `k3` |

## RA budget
Unchanged: a schema option. W, R and List are what a write and a query already cost; the
schema line grows by 12 bytes where `k3` is declared.

## Risks
- **Float equality with the vendor.** The vendor's exact arithmetic is not public; the
  facade's fixtures compare rankings, not bits.
- **Rollback and mixed clusters.** A pre-M260 binary refuses a HEAD whose schema holds
  `k3`. But a write's rows carry the analyzer as a `$fts`/`$texts` stamp, and a pre-M260
  binary that fails to decode a stamp reads it as no opinion: if such a binary runs the
  **first fold** of an index whose first write declared `k3`, it records the default
  analyzer, dropping `k3` and every other declared option. M181's and M204's lines have
  the same hole today. So during a rolling upgrade, declare `k3` only once every server
  runs M260 or later; `docs/deploy.md` says so. Stated, not repaired: repairing it needs a
  change to the old binaries.

## Tasks
- **M260.1** The spec.
- **M260.2** `FullText.k3`, the line, the scorer, the server option, tests, docs.
