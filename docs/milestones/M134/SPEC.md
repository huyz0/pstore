# M134: a keyed waiter whose own records fell off is never told "applied"

## Serves
Workstream C of [the delivery plan](../../comparisons/turbopuffer-next-plan.md) ("do not
claim exactly-once execution without proving its scope"), and a defect in
[M132](../M132/SPEC.md) found in review: [mutation-completion.md
§9.4](../../research/11-design/mutation-completion.md) answers `outcome_unavailable` when a
keyed waiter finds none of its own records.

## Delta
Today a keyed waiter whose window fell off and whose own records are no longer listed
answers `Unavailable`. For a keyed write, that is a `200` with `outcome_unavailable`,
which says the write applied. A losing attempt (a duplicate) or an expired one gets it
too, and applied nothing. It takes 32 tallying commits between the attempt's fold and its
waiter reading HEAD.

After: when none of the attempt's own records are listed and its window fell off or was
reaped, the waiter reads every listed outcome object (one parallel round, through the
engine's cache) for a record naming its key's winner:
- the winner is this attempt: `Unavailable`, as today;
- the winner is another attempt W: `Replayed`, through M132's `replay` with W. That gives
  `Replayed(None)` unless HEAD covers the key and W's commits are listed;
- no listed record names a winner: `Partial`, that is `409 idempotency_partial`, "was, or
  may have been, applied". It is the only honest answer when nothing listed says.
  - ⚠️ **This includes the common case of a sole winner never retried**, whose own record
    fell off before its waiter read: today a correct `200` with `outcome_unavailable`,
    after M134 a `409`. The waiter cannot tell that winner from an expired attempt, which
    applied nothing, so "applied" would be a guess.
  - The `idempotency_partial` message and `docs/api-migrations.md` stop saying "part was
    refused as expired", which is false for that winner. They say the write was, or may
    have been, applied, in whole or in part, and to read the rows.

§9.4 carries a correction banner for the row it changes.

Unchanged: every answer when the attempt's own records are listed; unkeyed waiters; the
fold; HEAD and outcome object formats; which status and code each answer maps to.

## Acceptance criteria
1. A loser whose own record fell off, with a later duplicate's record listed naming the
   winner, answers `Replayed(None)`. Today it answers `Unavailable`.
2. A loser whose own record fell off, with no listed record naming the key, answers
   `Partial`. Today it answers `Unavailable`.
3. The winner whose own record fell off, with a later duplicate's record listed naming it,
   answers `Unavailable`.
4. A sole winner whose own record fell off answers `Partial` (the case criterion 2 cannot
   tell it from), and its row exists.
5. Every M132 test passes unchanged.

## Test plan
`crates/pstore-engine/tests/it/idempotency.rs`. Each attempt is folded explicitly by its
own engine, then 31 or 32 tallying commits land **on the waiter's engine**, so it observes
the newer HEAD, and only then does the waiter read. (Fillers elsewhere would leave the
waiter on its stale HEAD, where its own record is listed.) 1, 2 and 4 are red against the
parent; 3 is a guard, green on the parent.
- 1: `a_fallen_loser_is_replayed_not_applied`. Mutation: "the unknown case answers
  `Unavailable`" (today's code).
- 2: `a_fallen_loser_with_no_record_is_partial`. Same mutation; and "the unknown case
  answers `Replayed(None)` when no winner is named".
- 3: `a_fallen_winner_is_unavailable`. Mutation: "a named winner is always replayed",
  which answers the winner `Replayed`; and "the unknown case is always `Partial`".
- 4: `a_fallen_sole_winner_is_partial`. Same mutation as 2's first.

## RA budget
Unchanged on every path but this one, which adds at most one parallel round of at most 32
GETs, through the cache, on the applied path (outside D-34). When the window fell it adds
none: the window already read every listed object. No LIST, nothing scaling with records.

## Risks
- `Partial` is imprecise both ways: a loser that applied nothing, and a sole winner that
  applied everything, both get it (criteria 2 and 4). Neither is told something false.
  A client that sees it reads the rows, as the message says.

## Tasks
- M134.1: the unknown case in `own_answer` and `await_keyed`, the §9.4 banner, and the
  tests.

## Review
Spec review round 1 (blocking on two majors, amended): the sole-winner case is now stated,
pinned by criterion 4, and the message and docs reworded; the test construction puts the
fillers on the waiter's engine. Minor taken: the RA budget's zero-cost case.
