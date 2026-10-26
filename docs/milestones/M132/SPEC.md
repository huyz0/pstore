# M132: idempotency keys for applied writes

## Serves
Workstream C of [the delivery plan](../../comparisons/turbopuffer-next-plan.md) ("retrying
after an ambiguous network failure needs a bounded, durable idempotency/result policy; do
not claim exactly-once execution without proving its scope and retention"), and the
"Exactly-once" limit of [M129](../M129/SPEC.md)'s proof. The design and its argument are
[mutation-completion.md §9](../../research/11-design/mutation-completion.md).

## Delta
Today a client that loses an applied write's response can only retry, and the retry may
apply again: every attempt draws a fresh operation id.

After: an applied write may carry `"idempotency_key"`. Of all the attempts carrying one key,
**at most one applies**. A later attempt is answered with the winner's outcome and
`replayed: true`. An attempt the fold cannot prove is not a repeat is refused
`409 idempotency_expired`, and applies nothing.

- **Key.** The format is `<e0>-<32 lowercase hex>`, with `e0` a decimal epoch.
  - `POST /v1/indexes/{index}/idempotency-keys` (write scope) answers
    `{"idempotency_key": ...}`, with `e0` set to the engine's M129 mark.
  - The door refuses a key whose `e0` is above HEAD. It reads HEAD only when `e0` is above
    this engine's mark.
- **Engine.**
  - `applied::keyed(op, key, ids, f)` stamps as `applying` does, adding the key:
    `$req` becomes `<op hex>[:ids];<key>`.
  - Each fold attempt decides every key once, across indexes, in bundle order, before the
    reject pass, by §9.3's three rules.
  - A discarded operation counts `rejected` under its own attempt.
- **HEAD.** A trailing `keys_since` (§9.2).
  - Every commit carries it forward.
  - Every fold raises it to each entry it drops from the list, and decides keys, whatever
    the engine's level.
  - An engine set for keys (`Engine::set_idempotency(true)`, which the server calls at
    level 3) also initialises it. Any commit it makes on a HEAD without the field sets it
    to that HEAD's epoch.
- **Outcome object.** A trailing keys section lists, per attempt, its key and its verdict:
  won, duplicate of W, or expired. When it is present, the ids section is written before
  it, empty if need be. A commit that decided no key writes exactly M130's bytes.
- **Waiter.** `await_applied` takes the attempt's key. It answers:
  - `Known`: its own outcome;
  - `Replayed(Option<W's outcome>)`: `None` when HEAD no longer covers the key, or an
    object was reaped;
  - `Expired`;
  - `Partial`: an expired record beside applied operations, or beside a window that fell
    off the list. A keyed waiter reads its own records in the listed commits after its
    mark before it answers `Unavailable`.
- **Server.**
  - The field needs `durability: "applied"` and `PSTORE_APPLIED_WRITES=3`.
  - A malformed key, or one beside another durability, is `bad_request`. A key at a lower
    level, and the issue route at a lower level, are `unsupported_durability`.
  - A key whose `e0` is above HEAD is `bad_request`.
  - A replay is `200` with `"replayed": true`, the winner's `applied` (or `null` with
    `outcome_unavailable`), and this attempt's own `operation`.
  - An expired or partial attempt is `409`, with code `idempotency_expired` or
    `idempotency_partial`.

Unchanged: writes without a key, `batched` and `durable`, and the bytes of HEAD and of
outcome objects in a deployment below level 3. A level-3 HEAD gains `keys_since`, and the
empty optional sections before it if it had none. An M131 decoder ignores it, and an M131
commit drops it, which criterion 9 covers. An M131 fold would apply every keyed attempt,
so the documented rollback order is: stop sending keys, let keyed bundles fold, then
downgrade.

Not done:
- comparing a retry's body with the winner's (the docs say a retry must resend the same
  write);
- keys on the compatibility facade;
- retention longer than K = 32 tallying commits.

## Acceptance criteria
1. **A retry is answered, not applied.** A keyed write upserts `x = 1` and its response is
   lost. A plain write then sets `x = 2`. The retry with the same key answers
   `replayed: true` with the first attempt's counts (`upserted: 1`), and `x` is still 2.
2. **One winner in one fold.** Two attempts of one key upsert `a` and `b` respectively, and
   one fold folds both. Exactly one of `a` and `b` exists, and both attempts answer with
   the same counts, one of them `replayed`.
3. **At most once under contention.** 100 attempts of one key, over 4 engines on one store,
   each upsert a distinct id, concurrently. Exactly one id exists, and exactly one answer
   is not `replayed`. Every replayed answer is either the winner's counts or
   `outcome_unavailable`, never different counts. The test reports the number of tallying
   commits, and when there were fewer than 32 it asserts that every replay has counts.
4. **Survives restart.** The retry of criterion 1, served by a new engine on the same store
   with empty caches, answers the same.
5. **Expiry is a refusal.** A key is issued and its first attempt is committed at `e0 + 1`.
   After 30 more tallying commits, a retry is replayed with the winner's counts; its own
   commit is the 32nd listed. A second retry is replayed with `outcome_unavailable`: it is
   decided while the winner is listed, but its own commit drops the winner's. Once 32
   further tallying commits leave no listed record naming the winner, a retry is refused
   `idempotency_expired`, and the row it would write does not exist. *(Amended during
   implementation: the first draft said "after 31 replayed, after 32 refused". That draft
   missed that a retry's own commit is tallying, and that a duplicate's record keeps naming
   the winner.)*
6. **A stale key never applies.** A key issued and then left unused through 33 tallying
   commits is refused `idempotency_expired` on first use, and its row does not exist. A key
   left unused through 32 is still accepted. Its `e0 + 1` is then the oldest listed epoch.
7. **Partial is reported.** One attempt, run in one `keyed` scope, writes and flushes `p`,
   which is folded. Then 32 tallying commits follow. Then it writes and flushes `q`. It
   answers `Partial` (engine) although its window fell off the list, `p` exists, and `q`
   does not.
8. **Refusals and levels.**
   - A key beside `batched` or `durable`, or a malformed key, is `bad_request`.
   - A key whose `e0` is above HEAD is `bad_request`. That includes `u64::MAX`, which
     never reaches a fold.
   - A key, or the issue route, at `PSTORE_APPLIED_WRITES` below 3 is
     `unsupported_durability`.
   - At level 3 the issue route answers a key whose `e0` is at most HEAD's epoch, and
     makes no blob request while the mark is fresh.
9. **A HEAD re-commit expires, never re-admits.** A key wins, and then HEAD is re-committed
   without `keys_since` and with no outcome list, as a pre-M132 binary does. A retry is then
   refused `idempotency_expired`, not applied.
10. **Format compatibility.**
    - An M130 outcome object decodes. A commit that decided no key encodes byte-for-byte as
      M130 does, and a HEAD without `keys_since` encodes as M131's. M131's decoder stops
      after the outcome list without checking for the end, so it reads a HEAD with it. The
      test decodes one with a copy of M131's decode, made from this release's decode less
      the new field.
    - An object with keys, and a HEAD with `keys_since`, round-trip.
    - Every truncation of either is refused, except a cut at a section boundary, which
      decodes as the older format.
11. **Raising is not the flag's.** An engine without `set_idempotency` folds the commit
    that drops a winner's entry. It raises `keys_since`, and a later retry is refused
    `idempotency_expired`.

## Test plan
- Engine, `crates/pstore-engine/tests/idempotency.rs`: criteria 1 to 7, 9 and 11, through
  `keyed` and `await_applied`.
  - "Lost response" means the test drops the answer and retries.
  - Tallying commits are unkeyed applied writes of other ids, with no gc or compact
    between, so epochs are pinned.
  - Criterion 9 writes the old HEAD through the existing encode with the field cleared.
  - Mutations, each named with the criterion it turns red:
    - "rule 1 ignores listed records": criterion 1 (`x` becomes 1);
    - "rule 1 ignores this fold's earlier attempt": criterion 2;
    - "rule 2 off": criteria 5 and 6;
    - "rule 2 compares `>` instead of `>=`": criterion 6's 32-commit case;
    - "a dropped entry does not raise `keys_since`": criterion 5;
    - "a missing `keys_since` reads as 0": criterion 9;
    - "only a flagged engine raises `keys_since`": criterion 11;
    - "a keyed waiter answers `Unavailable` before reading its own records": criterion 7;
    - "the first keyed fold, not every level-3 fold, sets `keys_since`": criterion 6 (the
      key is refused on first use after 32 commits);
    - "the waiter sums its own attempt on a duplicate": criterion 1 answers `upserted: 0`,
      `rejected: 1`.
- Unit (`applied.rs`, `head.rs`): criterion 10.
- Server, `crates/pstore-server/tests/idempotency.rs`: criteria 1, 5 and 8 over HTTP,
  including the issue route's HEAD count.
- Red first: each test runs against the parent, and against a stub fold that stamps keys
  without deciding them.

## Budget
- No new object, LIST, lock or lease. HEAD gains one `u64`, only at level 3.
- A fold attempt with keyed operations reads at most 32 outcome objects, in one round.
- The door reads HEAD only for a key ahead of the engine's mark. The issue route reads it
  at most once a second per engine.

## Review
Three rounds. Round 1 found one blocking issue: the waiter answered `Unavailable` before it
could report `Expired` or `Partial`. It also found five majors:
- a forged `e0` could stop a fold or defeat the guarantee;
- a list a rollback had shortened;
- criterion 6 was off by one;
- criterion 3 depended on timing;
- criterion 9's truncation claim could not hold.

Round 2 found one blocking issue: criterion 7 still ended at `outcome_unavailable`. It also
found two majors: raising `keys_since` must not depend on the flag, and a pre-M132 fold
applies every keyed attempt. Round 3 passed, and its one minor is folded into §9.4.
