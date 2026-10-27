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

§9.4 carries a correction banner for the row it changes.

Unchanged: every answer when the attempt's own records are listed; unkeyed waiters; the
fold; HEAD and outcome object formats; the server's mapping of each answer.

## Acceptance criteria
1. A loser whose own record fell off, with a later duplicate's record listed naming the
   winner, answers `Replayed(None)`. Today it answers `Unavailable`.
2. A loser whose own record fell off, with no listed record naming the key, answers
   `Partial`. Today it answers `Unavailable`.
3. The winner whose own record fell off, with a later duplicate's record listed naming it,
   answers `Unavailable`.
4. Every M132 test passes unchanged.

## Test plan
`crates/pstore-engine/tests/it/idempotency.rs`. Each attempt is folded explicitly, then 31
or 32 tallying commits land on another engine, and only then does the attempt's waiter
read. 1 and 2 are red against the parent; 3 is a guard, green on the parent.
- 1: `a_fallen_loser_is_replayed_not_applied`. Mutation: "the unknown case answers
  `Unavailable`" (today's code).
- 2: `a_fallen_loser_with_no_record_is_partial`. Same mutation; and "the unknown case
  answers `Replayed(None)` when no winner is named".
- 3: `a_fallen_winner_is_unavailable`. Mutation: "a named winner is always replayed",
  which answers the winner `Replayed`.

## RA budget
Unchanged on every path but this one. That path adds one parallel round of at most 32
GETs, most of them cached, on the applied path, which is outside D-34. No LIST, nothing
scaling with records.

## Risks
- `Partial` for a loser that applied nothing is imprecise, but it does not say "applied".
  Revealed by criterion 2, which pins it.

## Tasks
- M134.1: the unknown case in `own_answer` and `await_keyed`, the §9.4 banner, and the
  tests.
