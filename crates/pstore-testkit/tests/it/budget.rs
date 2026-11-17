//! M278: a request budget fails above, and an `exact` one below as well.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

use pstore_testkit::budget::{Bound, Budget, budget, misfit, parse, spend, spend_per};

const TABLE: &str = "\
# Budgets

Prose with a `name` in it is not a row.

| Budget | Bound | Requests | What |
|---|---|---|---|
| `one_put` | exact | 1 | a write |
| `few_rounds` | max | 4 | a depth |
";

fn exact() -> Budget {
    Budget {
        name: "one_put".to_owned(),
        bound: Bound::Exact,
        count: 1,
    }
}

#[test]
fn the_table_parses_to_its_rows_and_nothing_else() {
    assert_eq!(
        parse(TABLE),
        [
            exact(),
            Budget {
                name: "few_rounds".to_owned(),
                bound: Bound::Max,
                count: 4,
            }
        ]
    );
}

#[test]
fn an_exact_budget_fails_above_and_below() {
    assert_eq!(misfit(&exact(), 1, 1), None);
    let over = misfit(&exact(), 2, 1).unwrap();
    assert!(over.contains("over its budget of 1"), "{over}");
    let under = misfit(&exact(), 0, 1).unwrap();
    assert!(under.contains("under its exact budget of 1"), "{under}");
}

#[test]
fn a_max_budget_fails_only_above() {
    let max = parse(TABLE).pop().unwrap();
    assert_eq!(misfit(&max, 0, 1), None);
    assert_eq!(misfit(&max, 4, 1), None);
    assert!(misfit(&max, 5, 1).is_some());
}

#[test]
fn a_per_unit_budget_scales_with_its_units() {
    assert_eq!(misfit(&exact(), 100, 100), None);
    assert!(misfit(&exact(), 99, 100).is_some());
    assert!(misfit(&exact(), 101, 100).is_some());
}

#[test]
#[should_panic(expected = "is neither exact nor max")]
fn a_misspelt_bound_fails_rather_than_vanishing() {
    let _ = parse("| `x` | exactly | 1 | |");
}

#[test]
#[should_panic(expected = "is not a number")]
fn a_missing_count_fails_rather_than_vanishing() {
    let _ = parse("| `x` | exact | one | |");
}

#[test]
#[should_panic(expected = "no request budget named")]
fn an_unknown_name_fails() {
    let _ = budget("no_such_budget");
}

#[test]
fn the_checked_in_table_is_read() {
    // A row every server test suite spends: if the table stops parsing, this says so here.
    let b = budget("durable_write_puts");
    assert_eq!((b.bound, b.count), (Bound::Exact, 1));
    spend("durable_write_puts", 1);
    spend_per("durable_write_puts", 3, 3);
}

#[test]
#[should_panic(expected = "under its exact budget")]
fn spending_less_than_an_exact_budget_fails() {
    spend("durable_write_puts", 0);
}
