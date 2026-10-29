# M152 verification

The spec was reviewed in three rounds before any code.
- **Round 1 was blocking:** a name could be both the primary and a further field, `keys_since`
  had no presence marker to force, and a share's `Part` has one `FullText`.
- **Round 2 was blocking:** a patch could fill the primary with a further field's name, and
  the summed peer protocol could not carry a second `FullText`. The first was fixed by "No
  name is both". The second was split out: a further-field query is never split here, and
  [M154](../M154/SPEC.md) owns the peer protocol.
- **Round 3 passed.** Its two minors were applied: a `debug_assert!` that the encoder never
  writes `Some(u64::MAX)`, and a line saying `multi` and its twins share one tenant and one
  fold.

How to run the tests:
- `cargo test -p pstore-engine --lib head::`
- `cargo test -p pstore-engine --test it multi_text:: split::a_query_over_a_further`

Red runs: against the parent, every new test fails to compile, because `IndexSchema.texts`
and `Declared.texts` were absent. Criterion 2's no-further-fields half was green on the parent
by construction, since its bytes were captured from the parent. A compile failure shows only
that the API is new, so each test's power to fail rests on the named mutations below. They
were applied one at a time by a script, each test run, and then reverted. All 21 were
observed red. No `cargo mutants` run is claimed for them.

The parent's bytes for criterion 2 were captured at commit 4af59f2, the spec commit on top of
M151, with `cargo test -p pstore-engine --lib print_rich_for_capture -- --nocapture`. That was
a test printing `hex(&rich(None).encode())` and `hex(&rich(Some(17)).encode())`. It was then
replaced by the test that compares against those strings. `rich` carries a non-default
analyzer, a trigram set, named widths, a dropped schema, dictionaries, lengths, an external
and outcomes.

1. `each_further_field_scores_as_its_own_index_would`: green after. It checks before the
   first fold, after it, beside a segment, after a second fold, after a compaction, and
   `as_of` the first fold. Killed:
   - "the seal builds the primary only";
   - "the fresh view builds the primary only";
   - "before the first fold the view takes no `texts`";
   - "compaction refuses further fields as disagreeing";
   - "a further field is built under `fts`";
   - "the live query scores under `fts`";
   - "`as_of` scores under `fts`".

   "The fresh view's cache key ignores `texts`" is killed by criterion 3's
   `a_further_field_is_never_the_primary`. A view built before the fold, with no further
   fields known, must be rebuilt to stem `title`. "The fold does not copy `texts` from the
   creating row" is killed by `what_further_fields_cannot_mean_is_refused`. There, a fold
   creates an index whose declaring rows an undeclared write of the same ids supersedes.
2. `a_head_without_further_fields_writes_the_parents_bytes` and
   `further_fields_round_trip_live_and_dropped` (head.rs), and the drop half of
   `each_further_field_scores_as_its_own_index_would`: green after. Killed:
   - "the section is always written";
   - "a forced `None` is written as 0";
   - "the dropped schemas' further fields are not written".
3. `what_further_fields_cannot_mean_is_refused` and `a_further_field_is_never_the_primary`:
   green after. Killed:
   - "a different set is accepted";
   - "`door_conflict` ignores `texts`";
   - "a row under a further field is accepted";
   - "the fill after `resolve` takes a further field";
   - "the fresh view keeps a primary naming a further field";
   - "a name equal to the writer's field is accepted";
   - "the fresh view's cache key ignores `texts`";
   - "the fold does not copy `texts` from the creating row".

   One finding while writing the test: the title writer's flush refuses its stale row once,
   then waives it to the fold's quarantine. That is M35's rule, unchanged, and the test
   asserts it.
4. a_query_over_a_further_text_field_is_not_split (split.rs): green after. Killed: "a
   further-field query is split", "a primary query is not split". ⚠️ **Reversed by
   [M154](../M154/SPEC.md)**, which lifts the limit this criterion pinned: the test is now
   `a_query_over_a_further_text_field_is_split_as_unsplit`.
5. `a_damaged_further_fields_section_is_a_corrupt_head` (head.rs): green after. Killed: "a
   truncated section decodes as fewer fields".

The whole workspace's tests pass, except the two IPv6 `peer_http::response_tests`, which fail
in this container on the parent too.

Code review round 1 passed with no findings. It noted two things below the bar, recorded here:
- the decoder would merge two records addressing one schema;
- the replica fingerprint does not hash `texts`, which is fixed at creation.
