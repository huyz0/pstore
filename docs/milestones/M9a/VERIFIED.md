# M9a — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ **Where this ran:** a Linux x86-64 cloud container (4 cores, `cargo-mutants` 27.1.0), the
server driven in-process over `tower::ServiceExt::oneshot` against `MemoryStore`.

1. **Attributes round-trip** — `attributes_come_back_from_unfolded_rows`,
   `attributes_survive_a_fold_and_time_travel` and `text_written_as_an_attribute_is_the_text_field`
   (`cargo test -p pstore-server --test attributes`). The first two were **observed red** on the
   door as it was (it ignored `attributes`); the third was written with the revised spec and is
   held by the mutation sweep of criterion 7.
2. **Selection** — `a_named_list_returns_only_those_attributes`,
   `exclusions_and_an_empty_list`, and `attributes_are_absent_unless_asked_for`, which passed
   before and after by design: it pins that a query not asking is answered exactly as before.
3. **No extra request or byte** — `returning_attributes_costs_no_request_and_no_byte`:
   identical `meta.cost` with and without `include_attributes: true` on a folded index.
   **OBSERVED-NOT red**: it holds by construction (the id round already reads the blocks), and
   no mutation in the sweep adds a fetch.
4. **Refusals** — `a_value_the_format_cannot_store_is_refused_not_coerced`: float, bool, array,
   object, `null`, `u64::MAX`, an empty name, a non-string `text`, and `text` given twice — each
   `400 bad_request` naming document and attribute, and the index still absent after. Observed
   red on the door as it was.
5. **`rows_at`** — `rows_at_returns_each_rows_id_and_attributes_in_one_round`
   (`cargo test -p pstore-format --test ids_at`): one read for four rows across blocks, each row
   its own attributes, `None` past the end, `ids_at` agreeing.
6. **The writer fallback** — `distinct_integer_names_past_the_budget_seal_without_zone_maps`
   was **observed red** on the old writer (`Unsupported("too wide…")`), and
   `distinct_integer_names_do_not_brick_the_fold` red on it through the API (the fold answered
   `500`). `a_zone_free_index_that_still_does_not_fit_keeps_doubling` was seen red under the hand
   mutation "fallback without doubling"; `a_segment_that_fits_keeps_its_zone_maps` pins the
   unchanged case. ⚠️ `an_over_wide_segment_is_refused_by_the_fold` pinned the defect as
   intended behaviour — 400 integer attributes refused — so its **fixture** changed to a vector
   field name longer than the budget; its assertions did not. Code review confirmed the new
   fixture reaches the refusal (a `fits`-always-true mutation fails it).
   ⚠️ **Code review found the fallback incomplete**: the loop fitted the block index against the
   whole budget while the refusal measures the whole meta region, so ~175 integer names on one
   document still bricked an index (reproduced through the API). The loop now fits against
   `budget − overhead`, the overhead computed exactly before any block is sealed.
   `every_width_near_the_budget_seals` sweeps 150–200 names and 6,400–6,560 attribute-free rows,
   and was **observed red** on the previous writer at 175 names.
7. **Gates** — the mutation sweep over the diff:
   `./scripts/mutants.sh --check . --in-diff <the working-tree diff>`, whole-workspace tests per
   mutant: **108 tested in 33m, 49 caught, 58 unviable, 1 missed** — the function query_ids replaced by `Ok(vec![])`:
   it had no caller left, so it was deleted rather than tested. The writer, rewritten after
   code review, swept again over its own diff: **34 tested in 19m, 32 caught, 1 unviable,
   1 timeout** (`idx.len() <= target` → `>`: every segment rolls through doubling and the
   fallback, cut off at 300 s; timeouts count as caught). So 0 missed.
   `./scripts/gates.sh` on the final tree: all fifteen PASS. Review: spec two rounds (block on
   the zone-map brick, then on the fallback without doubling), code two rounds (block on the
   meta-region overhead, then pass — the reviewer's HTTP probe of 140–219 names all 200).

**Found, not fixed here:** a fold that fails permanently (as the over-wide segment does) answers
`500 internal` with `"retryable": true`; nothing a client retries will change it.
