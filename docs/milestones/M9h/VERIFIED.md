# M9h — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ **Where this ran:** a Linux x86-64 cloud container (4 cores, `cargo-mutants` 27.1.0).

## M9h.1 — `float` and `bool`

1. **Round trip** — `floats_and_bools_come_back_as_written`
   (`cargo test -p pstore-server --test typed_values`): `1.5`, `2.0`, `-0.25`, `1e3`, `true`
   and `false` come back as the same JSON numbers and types, both unfolded and folded.
   `floats_and_bools_round_trip_in_a_version_2_segment`
   (`cargo test -p pstore-format --test typed_values`). **Observed red** on the M9g.2 server,
   which refused the float at the door.
2. **Comparison** — `numbers_compare_as_numbers_and_exactly`, unfolded and folded:
   - `Gt 1.5`, `Eq 2`, `Eq 2.0`, `In [1, 2.5]`, `In [1, 2]`, `In [0]`, `Eq true`, `Lt true`;
   - `2^53 + 1` `Gt` the float `2^53`, and `Eq` only its own;
   - `-0.0` `Eq` both `0` and `0.0`;
   - `NotEq 2`, which keeps the absent row.
   `numbers_compare_exactly_across_int_and_float` (`cargo test -p pstore-format --test
   typed_values`) covers the edges at `±2^63` and NaN. **Observed red**: the float was refused
   at the door.
3. **Pruning pays** — `a_float_column_prunes_its_blocks`: `Lt 64.0` and `Lt 64` each read at
   most a quarter of the unprunable `Not(Gte 64.0)`. **Observed red**: refused at the door.
   `an_int_column_still_prunes_for_either_literal` is the same over an int column, whose
   segment stays version 1. It is a regression guard, passing on the base for `Lt 64`.
4. **Pruning is sound** — `cargo test -p pstore-query --test typed_pruning`:
   - `mixed_ints_and_floats_prune_soundly_with_zone_maps` (flag `1`);
   - `a_zone_free_typed_segment_prunes_soundly` (flag `0`, found by lowering the budget
     until the zones go);
   - `an_int_only_segment_prunes_soundly` (version 1).
   Each checks every operator, its negation, and `In` over the values and their neighbours,
   `-0.0`, `2^53 + 1`, a string and a bool included, against brute-force `admits`.
5. **Bytes kept** — `a_segment_without_floats_keeps_its_bytes`: FNV-1a of a 10-row int,
   string and vector segment, **recorded on f17fdd7**, the commit before M9h.1.
   `an_untyped_segment_stays_version_1`.
6. **rank_by order** — `rank_by_orders_bools_then_numbers_then_strings_then_absent`, unfolded
   and folded. **Observed red**: refused at the door. The existing order tests in
   `crates/pstore-server/tests/order.rs` and `crates/pstore-query/src/order.rs` pass
   untouched.
7. **Refusals** — `what_still_has_no_type_is_refused`: `9223372036854775808`, `u64::MAX`, an
   object, an array and `null`, written or filtered. It is a regression guard: all were
   refused before too.
   `an_integer_serde_json_parsed_as_a_float_is_stored_as_that_float` stores 2^64 as the f64
   serde_json parsed.
   `a_float_that_is_not_finite_is_refused_where_documents_enter` covers NaN and ±∞.
   `a_value_the_format_cannot_store_is_refused_not_coerced` and `a_malformed_filter_is_refused`
   drop their `1.5`, `true` and `[1, 2.5]` members, and keep every other member.
8. **Gates** (M9h.1):
   - `./scripts/mutants.sh --check . --in-diff <the M9h.1 diff>`: **101 tested in 44m, 85
     caught, 15 unviable, 1 missed**. The miss was `Dec::at_end -> true`, the refusal of bytes
     after the float table, which code review had predicted untested.
   - `a_typed_index_is_framed_exactly` (`cargo test -p pstore-format --lib reader`) was added
     for it, and `--check 'at_end|decode_index'` then gave **31 tested, 30 caught, 1
     unviable, 0 missed**.
   - Code review: one round, pass with two minors (that test, and this line).
   - `./scripts/gates.sh` on the committed tree: all fifteen PASS.
