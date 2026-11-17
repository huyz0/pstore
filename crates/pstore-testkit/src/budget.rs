//! Request budgets that ratchet both ways (M278).
//!
//! The numbers live in one table, `docs/research/11-design/request-budgets.md`, and a test
//! reads its number from there rather than repeating it. A row is `exact` or `max`:
//!
//! - **`exact`** fails when the measurement comes in **below** the budget as well as above
//!   it. A saving must be written into the table, where it then holds, and cannot be lost
//!   again silently by a later change that spends it back.
//! - **`max`** fails only above. For a figure that legitimately varies (a depth the counter
//!   merges, a share that depends on timing).
//!
//! `scripts/check-budgets.py` holds the other half: every row is spent by some test, and every
//! name a test spends has a row.
#![allow(
    clippy::panic,
    reason = "an assertion helper: a panic in a test is the reporting mechanism"
)]

/// The table, compiled in so a test needs no working directory.
const TABLE: &str = include_str!("../../../docs/research/11-design/request-budgets.md");

/// How a row bounds its measurement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bound {
    /// Equal, or the test fails: above is a regression, below is an unrecorded saving.
    Exact,
    /// At most.
    Max,
}

/// One row of the table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Budget {
    /// The row's name, as a test spends it.
    pub name: String,
    /// How it bounds.
    pub bound: Bound,
    /// The number.
    pub count: u64,
}

/// Every row of `table`: a line `| \`name\` | exact or max | count | ... |`. Header, rule
/// and prose lines are skipped.
///
/// # Panics
/// A row whose bound or count does not parse: a typo in the table must fail, not vanish.
#[must_use]
pub fn parse(table: &str) -> Vec<Budget> {
    table
        .lines()
        .filter_map(|line| {
            let cells: Vec<&str> = line
                .trim()
                .strip_prefix('|')?
                .split('|')
                .map(str::trim)
                .collect();
            let name = cells.first()?.strip_prefix('`')?.strip_suffix('`')?;
            let bound = match cells.get(1).copied() {
                Some("exact") => Bound::Exact,
                Some("max") => Bound::Max,
                other => panic!("budget {name}: bound {other:?} is neither exact nor max"),
            };
            let count = cells
                .get(2)
                .and_then(|c| c.parse().ok())
                .unwrap_or_else(|| {
                    panic!("budget {name}: count {:?} is not a number", cells.get(2))
                });
            Some(Budget {
                name: name.to_owned(),
                bound,
                count,
            })
        })
        .collect()
}

/// The row `name` of the checked-in table.
///
/// # Panics
/// When the table has no such row.
#[must_use]
pub fn budget(name: &str) -> Budget {
    find(&parse(TABLE), name)
}

fn find(rows: &[Budget], name: &str) -> Budget {
    rows.iter()
        .find(|b| b.name == name)
        .cloned()
        .unwrap_or_else(|| panic!("no request budget named {name} in request-budgets.md"))
}

/// Why a measurement does not fit its budget, or `None` when it does.
#[must_use]
pub fn misfit(b: &Budget, measured: u64, units: u64) -> Option<String> {
    let allowed = b.count.saturating_mul(units);
    if measured > allowed {
        return Some(format!(
            "{} spent {measured} requests, over its budget of {allowed} ({} x {units})",
            b.name, b.count
        ));
    }
    (b.bound == Bound::Exact && measured < allowed).then(|| {
        format!(
            "{} spent {measured} requests, under its exact budget of {allowed} ({} x {units}): \
             a saving -- lower the row in docs/research/11-design/request-budgets.md so it holds",
            b.name, b.count
        )
    })
}

/// Asserts `measured` fits the row `name`.
///
/// # Panics
/// When it does not, or there is no such row.
pub fn spend(name: &str, measured: impl TryInto<u64>) {
    spend_per(name, measured, 1);
}

/// Asserts `measured` fits the row `name` taken `units` times: a per-tenant row measured over
/// many tenants.
///
/// # Panics
/// When it does not, or there is no such row.
pub fn spend_per(name: &str, measured: impl TryInto<u64>, units: u64) {
    let measured = measured
        .try_into()
        .unwrap_or_else(|_| panic!("{name}: a count past u64"));
    if let Some(why) = misfit(&budget(name), measured, units) {
        panic!("{why}");
    }
}
