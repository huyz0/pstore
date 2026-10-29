# M145 verification

The spec was reviewed in one round before any code. The review found one blocking issue:
criterion 6 could not go red as first written. It found two majors: `$ref_new`'s `costly()`
was not covered, and the oracle had no fixed size. All three were fixed in the spec.

How to run the tests: `cargo test -p pstore-query --lib` and
`cargo test -p pstore-server --all-features --test it patterns:: ref_new::malformed`.

Red runs:
- Criteria 1, 5 and 6 did not compile against the parent: `Predicate::FuzzySubstring` did
  not exist.
- Criteria 2 and 3: the server tests were written after the parser arm. Their red run is
  the mutation "vendor arm removed" below. It gives the parent's answer, a `400` for every
  four-element `Fuzzy`, and it failed both tests.
- Criterion 4's refusals are guards, green on the parent, which refused every four-element
  `Fuzzy`. So is criterion 5's tag 14. The `$ref_new` case is the exception: it is red
  under "`costly()` omits the variant" below.

The mutations were applied one at a time by a script and then reverted. No `cargo mutants`
run is claimed for them.

1. `fuzzy_substrings_match_the_reference`: green after. Killed: "the start is not free",
   "the end is not free", "`k` off by one", "case folding ignored", "only the first array
   element is read", "an empty array holds".
2. `the_vendor_fuzzy_picks_its_distance_by_query_length`: green after, before and after the
   fold. Killed: "the smallest entry not above `n` is chosen", "a query below every
   threshold matches everything", "`case_sensitive` defaults to `false`", "an array is read
   as no strings", and "vendor arm removed".
3. `the_vendor_fuzzy_reads_arrays_and_decides_a_delete_by_filter`: green after. Killed: "a
   query below every threshold matches everything", "an array is read as no strings",
   "vendor arm removed".
4. `what_the_vendor_fuzzy_cannot_mean_is_refused` and `malformed_templates_are_refused`:
   green after. The latter killed "`costly()` omits the variant".
5. `every_predicate_round_trips` and `every_truncation_and_every_unknown_tag_is_none`: green
   after. Killed: "the case flag is not encoded".
6. `a_fuzzy_substring_never_prunes_on_the_sketch`: green after. Killed: "it prunes on the
   sketch" (M15's arm, copied).
