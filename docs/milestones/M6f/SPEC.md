# M6f — The poisoned lock the appender drops on the floor

**Serves:** backlog item 16, which [M6e](../M6e/VERIFIED.md) opened when its coverage criterion
measured `pstore-catalog` at 94.90% and traced the gap to `append.rs` at 88.75%.

⚠️ **Coverage is how this was found, and it is not what the milestone is about.** The number
pointed at two branches; reading them turned up a behaviour the crate's own documentation says
it cannot afford.

## ⚠️ The failure: a lifecycle-rate append silently becomes a commit-rate one

`Appender`'s doc states the trade: *"C-12 buys a bounded write with a CAS on the append path,
and that trade is only affordable while an append is a **lifecycle** event. What holds it there
is `Appender::observe`"* — which writes only when a tenant's identity actually changed, and
remembers what it wrote in a `Mutex<HashMap<TenantId, u64>>`.

Both locks are handled by **dropping the result**:

```rust
if self.seen.lock().is_ok_and(|s| s.get(&rec.tenant) == Some(&identity)) { ... }
...
if let Ok(mut seen) = self.seen.lock() { seen.insert(rec.tenant, identity); }
```

On a poisoned lock the first reads as "not seen" — a wasted CAS, harmless — and the second
**skips the insert**. The appender then has no memory of that tenant, so every later `observe`
for it records unconditionally. ⚠️ The dedupe is off for that tenant for the life of the
appender, and what `observe` exists to hold at lifecycle rate arrives at commit rate, against
the same registers C-12's bounded write was measured on. Nothing reports it.

## ⚠️ It is also the only shipping module that does this

`unwrap_or_else(std::sync::PoisonError::into_inner)` — recover the value, the poisoning is
someone else's problem — is the idiom in **six** shipping modules: `pstore-blob`'s
`accounting.rs`, `memory.rs` and `faulty.rs`, `pstore-engine`, `pstore-meter` and
`pstore-testkit`'s `audit.rs`. `pstore-catalog`'s appender is the one that differs, and it
differs in the direction that loses state.

## Delta

**`pstore-catalog`**
- `observe` recovers both locks with `PoisonError::into_inner`, like every other module.

**`scripts/`**
- ⚠️ **AMENDED after implementation: `build-index.py` gains a third comparison.** It checked
  `ci.yml` against AGENTS.md's Gates table and **nothing against `gates.sh`** — so a gate added
  to `gates.sh` alone runs locally, never runs in CI, and both sets agree everything is fine.
  Found while adding this milestone's own gate, which is the same failure one level up. ⚠️ The
  comparison is **one-directional**: everything `gates.sh` runs must run in CI, and the
  converse is false on purpose — `coverage.sh`, `recall.sh`, `ndcg.sh`, `depth.sh` and
  `mutants.sh` run in CI and are deliberately absent from the fast local script. Checking both
  ways reported those five as drift on the first run.
- ⚠️ `scripts/check-poison.sh` — refuses `is_ok_and(…lock` and `if let Ok(…) = ….lock()` in
  `crates/*/src/`, and is added to `gates.sh`. **This is the milestone.** Fixing one instance
  leaves the next one to be found by a coverage number again; the rule is a predicate over
  files, and `gate-design`'s ladder says a rule that can be stated that way must not be left to
  someone remembering. Test code is out of scope — a decorator in a test that drops a poisoned
  lock loses a recording, not a bounded write.

**Does not add** — **a non-poisoning mutex.** `parking_lot` is a dependency and a different
failure model; the idiom this project already uses is the one to be consistent with.
**A test that poisons the lock.** See below — it cannot be written from outside the crate, and
the honest criterion is the predicate, not a contrived panic.

## Acceptance criteria

1. **`observe` recovers a poisoned lock rather than losing the entry**, and the dedupe still
   holds: recording the same identity twice writes once, and a changed identity writes again.
   ⚠️ **`OBSERVED-NOT` for the poisoned case itself** — the guard is held only inside `observe`
   and nothing inside it can panic, so from outside the crate the lock cannot be poisoned. The
   criterion is met by construction and by criterion 2, and this ledger says so rather than
   claiming a test that does not exist.
2. ⚠️ **The gate refuses the degrading form**, and is shown to: `scripts/check-poison.sh` fails
   on a file containing it and passes on the tree. A gate that cannot fail reports success
   while checking nothing — which is the pathology `gates.sh`'s own header records catching in
   itself on its first run.
3. **The gate is in `gates.sh` and in `ci.yml` and in AGENTS.md's table** — the three places a
   gate has to be to gate anything, and the third comparison above is what now enforces it.
4. **`pstore-catalog` coverage is re-measured and reported**, whatever it comes to. ⚠️ The
   remaining regions after this change are the `into_inner` closures themselves, which are
   uncovered in all six modules that use the idiom — so this is not expected to reach 95% on
   its own, and the number is reported rather than the criterion adjusted to it.

## Test plan

| # | Test | The mutation it kills |
|---|---|---|
| 1 | `an_unchanged_index_set_writes_nothing` and `an_observe_costs_one_read_and_one_write` (existing suite) | the memory dropped, which is what the poisoned path did silently |
| 2 | `scripts/check-poison.sh` against a fixture containing the form | a grep that matches nothing and exits 0 — a gate that cannot fail |
| 3 | the existing `append` suite | the recovery changing the uncontended behaviour |

## RA budget

Unchanged — no request shape changes. The point is that it **stays** unchanged: the failure
this removes turns one CAS per lifecycle event into one per commit.

## Risks

- **`into_inner` on a poisoned lock means using state a panicking thread left behind.** For a
  `HashMap<TenantId, u64>` of "what I last wrote", a torn view costs a redundant CAS — the same
  cost as the current behaviour on its *first* call, and unlike it, bounded.
- ⚠️ **The gate is a grep and greps go stale.** A future spelling — `lock().ok()`,
  `let Some(g) = …lock().ok()` — passes it. Criterion 2's fixture pins the forms it does catch;
  it does not pretend to catch every possible one.

## Tasks

| Id | Commit |
|---|---|
| **M6f.1** | The appender recovers its lock, and a gate refuses the form that does not |
