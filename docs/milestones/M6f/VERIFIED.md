# M6f — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

1. **`observe` recovers a poisoned lock**, and the dedupe still holds —
   `an_unchanged_index_set_writes_nothing` (one write, then 100 observes that write nothing,
   then a real change that does) and `an_observe_costs_one_read_and_one_write`, both unchanged
   and green through the rewrite.
   ⚠️ **`OBSERVED-NOT` for the poisoned case itself, and this is the honest part.** The guard is
   held only inside `observe` and nothing inside it can panic, so from outside the crate the
   lock cannot be poisoned — no test can reach the branch, which is *why* it was wrong for two
   milestones and why a coverage number is what found it. The criterion is met by construction
   and by criterion 2's predicate, not by a contrived panic.
   ⚠️ What was wrong: `if let Ok(mut seen)` **skipped the insert** on a poisoned lock, leaving
   the appender with no memory of that tenant — so every later `observe` for it recorded
   unconditionally. `Appender`'s own doc says the trade C-12 bought *"is only affordable while
   an append is a **lifecycle** event"*; the dropped `Result` turned it into a commit-rate one,
   silently, against the registers that trade was measured on.
2. **The gate refuses the degrading form, and is shown to** —
   `./scripts/check-poison.sh --selftest` builds a tree containing `if let Ok(..) = ..lock()`,
   asserts the scan **fails** it, then builds one using the recovery idiom and asserts it passes.
   ⚠️ Verified against the **real tree** as well, not just the fixture: reintroducing each form
   into `append.rs` in turn was observed to fail the gate, and removing it to pass. A gate that
   cannot fail reports success while checking nothing, which is the pathology `gates.sh`'s own
   header records catching in itself on its first run.
3. **In all three places a gate has to be** — `gates.sh`, `.github/workflows/ci.yml`, and
   AGENTS.md's Gates table.
   ⚠️ **And the checker that was supposed to enforce that had a hole**, found by this
   milestone: `build-index.py` compared `ci.yml` against the table and **nothing against
   `gates.sh`**, so a gate added locally and forgotten in CI passes it. It now compares those
   too, **one-directionally** — everything `gates.sh` runs must run in CI; the converse is false
   on purpose, because `coverage.sh`, `recall.sh`, `ndcg.sh`, `depth.sh` and `mutants.sh` are
   CI-only by design. Checking both ways reported those five as drift on the first run.
   Observed red: adding a script to `gates.sh` that CI does not run fails
   `./scripts/build-index.py --check` by name.
4. **Coverage re-measured and reported** via
   `cargo llvm-cov -p pstore-catalog --all-features --lib --tests` — `pstore-catalog`
   **94.94%** regions, up from 94.90%;
   `append.rs` **89.33%**, up from 88.75%.
   ⚠️ **Still below the 95% [M6e](../M6e/VERIFIED.md)'s criterion 7 asked for, and this closes
   the question rather than the gap.** There are no uncovered *lines* in `append.rs` at all —
   the eight uncovered regions are sub-line branches: the `?` error arms, and the poison-recovery
   closures themselves, which are uncovered in **all six** modules that use the idiom. That
   makes it a property of the idiom rather than a hole in this crate, and chasing it would be
   hunting a number. The workspace gate — `scripts/coverage.sh --fail-under-regions 95`, the
   one that actually runs — is green.

## What is not built, and named rather than omitted

- ⚠️ **The gate is a grep and greps go stale.** It catches `is_ok_and` over a `.lock()`,
  `if let Ok(..)`/`while let Ok(..)` over one, `.lock().ok()` and `.lock().unwrap_or_default()`.
  A future spelling passes it. The selftest pins what it does catch and claims nothing more.
- **Shipping code only.** `crates/*/src/`; `tests/` is full of decorators that drop a poisoned
  lock deliberately, and one of those loses a recording, not a bounded write.
- **A non-poisoning mutex.** `parking_lot` is a dependency and a different failure model; being
  consistent with the idiom the tree already uses in six modules is the cheaper correctness.
- ⚠️ **Nothing measures whether a lock is ever actually poisoned.** `into_inner` makes the
  consequence bounded; it does not make the panic that caused it visible.
