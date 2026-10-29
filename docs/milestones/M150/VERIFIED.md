# M150 verification

The spec was reviewed in two rounds before any code. Round 1 found three blocking issues.
The door refused arrays. Three of the four "has text" sites were missing. The M36 stamp had
no criterion. All three were fixed, and round 2 passed.

How to run the tests:
- `cargo test -p pstore-format --test it text::`
- `cargo test -p pstore-engine --test it schema::`
- `cargo test -p pstore-server --all-features --test it text_arrays:: arrays::`

Red runs against the parent:
- Criterion 1 failed on the fieldnorms: an array row had no tokens.
- Criterion 5 failed with "the fold of array rows filled nothing".
- Criteria 2 to 4 failed at the door. The array write got a `400` "must be a string", and
  `"[]string"` got "the text field's type is string".
- Criterion 6 could not be set up on the parent, which stores no array text. It is a guard
  after.
- Criterion 5's `[]`/`[""]` case was added after a mutation survived (below). Its red run is
  that mutation.

The mutations were applied one at a time by a script and then reverted. No `cargo mutants`
run is claimed for them.

1. `a_string_array_indexes_as_its_strings_joined_by_a_space`: green after. Killed: "only the
   first element is read", "elements are joined without a separator before analysis", "a
   non-string element ends the array".
2. `an_array_text_field_scores_as_its_strings_joined`: green after, before and after the
   fold. Killed: "`wants_text` reads strings only", "`seal` reads strings only", "the field
   length counts elements, not tokens".
3. `an_array_with_no_text_is_no_text`: green after. Killed: "`wants_text` reads strings
   only", "`seal` reads strings only". "`[""]` has text" survived it, because a `[""]` row
   has no tokens either way. The spec now names criterion 5 for that mutation.
4. `the_door_takes_an_array_of_strings_and_nothing_else_as_text`, and `arrays.rs`'s
   `what_an_array_cannot_hold_is_refused` with the reversed `["a", 5]` case: green after.
5. `an_array_of_strings_is_text_to_the_fold_and_to_the_door`: green after. Killed:
   "`has_text` reads strings only", and "`[""]` has text" once the `[]`/`[""]` writes by a
   second field were added.
6. `an_array_text_field_is_not_highlighted`: green after. This is a guard.
