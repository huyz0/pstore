# M8g — `pstore-engine`: the fresh cache, the split store, and the fold's two guards

**Serves:** **D-111**, on the crate that holds the commit protocol and Invariant I1. A clean
sweep (`./scripts/mutants.sh --check "^crates/pstore-engine/src/"`, 320 mutants, 45 min, after
the dev container's zombie fix) missed **17** in `pstore-engine/src/lib.rs`. This milestone
takes **12**; the other **5** are deferred with a reason (below).

## Delta

- **`Split`'s routing** (4: `get`, `get_suffix`, `delete_batch`, `list_unrestricted` replaced
  by a constant). M7b pinned `head` the same way; the others were never exercised. (`get_tag`
  was caught in the clean sweep only indirectly — by `pstore-testkit`'s
  `sweep_reports_a_curve_not_a_point`, not by any test of the routing — so it is asserted
  directly beside them.) Extend
  `the_split_store_routes_head_by_key_prefix`'s pattern: a `mem/` key reads from the fresh
  store, any other from the durable one, and `delete_batch`/`list_unrestricted` go to the
  durable store.
- **`backoff` emptied.** A retry that does not wait is a livelock the jitter exists to prevent.
  Pinned under `tokio::time::pause`: `backoff(lane, n)` advances the clock by at least
  `backoff_delay(lane, n)` and by less than one more millisecond — a paused clock advances in
  whole milliseconds, so "exactly" fails on correct code. Needs tokio's `test-util` in
  `pstore-engine`'s dev-dependencies.
- **The fresh-segment cache**, two different mistakes:
  - `generation += 1` → `*= 1` in `write`: the generation never moves, so a query after a
    second write is served the segment cached before it. Pinned: write A, query, write B,
    query — B is returned.
  - `f.index == index` → `!=` in `fresh_target` (line 1598, column 24 — **not** the generation
    comparison, which is already caught): a query on index `b` is served index `a`'s cached
    segment. Pinned: write to `a` and `b`, query `a`, then query `b` with no write between —
    `b`'s answer names only `b`'s rows.
- **The test-only write helper's copy of that bump** (`write_without_schema_check_for_test`).
  Rung 1: `write` and the helper share one private `buffer` step, so the bump exists once.
- **A sparse query on unfolded rows** (`!d.is_empty()` → `d.is_empty()` on the fresh segment's
  dictionary). The fresh segment would be stored without its dictionary, and a sparse leg on
  unfolded rows fails. Pinned: write hybrid rows, do not fold, a `Prefetch::Sparse` leg returns
  them.
- **`fold`'s reject count** (`dropped > 0` → `>=`). A fold over an index with a recorded schema
  and conforming rows would record `schema_rejects[idx] = 0`. Pinned: such a fold leaves
  `schema_rejects` empty.
- **`fold`'s watermark guard** (`tail > 0` → `>=`). ⚠️ **This reverses a recorded
  decision**: the comment at the guard calls the mutant provably equivalent and argues against
  a test. It is equivalent for *reads*; its purpose is HEAD's *size*, and a zero entry is encoded
  — observable. Pinned: a lane registered with nothing to fold (`lanes::register`, public)
  gets no `lane → 0` entry, while a second lane with a bundle makes the fold commit. The
  comment is rewritten to say the guard is now pinned.
- **`commit_stale_for_test`'s `epoch: Epoch(1)`** is equivalent: `commit` returns the epoch
  only on success and every caller asserts refusal. Rung 1: the stale head is
  `Head::default()`, with a comment saying why the epoch is irrelevant.

**Deferred, with a reason: the three flush-path generation bumps** (`flush_inner` ×2 → 3
mutants, `restore` → 2). They matter only when a query races a flush — and investigating them
found a **visibility gap**: `flush_inner` takes rows out of `pending` before the bundle PUT and
puts them in `durable` only after it, so a query in that window sees **neither**, and
acknowledged rows vanish from results for one blob write. A test that kills those mutants today
would get its power **from the bug** — the window's cached content differs from the post-flush
content only because the rows are missing — and would silently stop killing once the gap is
fixed, at which point the flush bumps may be removable. So they wait for the gap's fix. Spec review found a **worse sibling**: `fold` does not
take the flush lock and clears `durable` after committing, so rows a concurrent flush moved in
after the fold probed the lanes stay invisible to this process **until the next fold** — any
length of time — and a query between the commit and the clear sees folded rows twice. Both,
and `query`'s re-lock of the fresh segment without re-checking its index, are BACKLOG rows.

**Does not change:** any engine behaviour, request count, or threshold.

## Acceptance criteria

1. A `Split` with both stores routes `get`, `get_suffix` and `get_tag` by prefix, and sends
   `delete_batch` and `list_unrestricted` to the durable store.
2. Under paused time, `backoff(lane, 3)` advances the clock by `d` with
   `backoff_delay(lane, 3) <= d < backoff_delay(lane, 3) + 1ms`.
3. Write A, query, write B, query: the second answer contains B. And with rows in indexes `a`
   and `b`, querying `a` then `b` gives `b` only `b`'s rows.
4. `write` and `write_without_schema_check_for_test` share one buffering step:
   `grep -c 'generation += 1' crates/pstore-engine/src/lib.rs` goes from **6 to 5**.
5. A sparse leg over unfolded hybrid rows returns them.
6. A second, conforming fold on an index with a schema leaves `schema_rejects` empty.
7. A registered lane with nothing to fold has no watermark entry after a fold.
8. `commit_stale_for_test` builds `Head::default()`; its three callers still see refusal.
9. `./scripts/mutants.sh --check "^crates/pstore-engine/src/"`: exactly the **5** deferred
   flush-path mutants missed, and no others in `pstore-engine`.
10. The flush-window gap, the fold's `durable.clear()` race, and `query`'s fresh-segment re-lock
    are BACKLOG rows; the watermark guard's comment no longer claims equivalence.
11. `./scripts/gates.sh` passes in the dev container.

## Test plan

| # | Fails first | Mutation it catches |
|---|---|---|
| 1 | each constant-return mutant, by hand | the four routing replacements |
| 2 | `backoff` emptied, by hand | the missing wait |
| 3 | `+=` → `*=` in `write`; `f.index ==` → `!=` in `fresh_target`, by hand | stale cache; another index's cache |
| 5 | `delete !` on the dictionary filter, by hand | fresh segment without its dictionary |
| 6 | `>` → `>=`, by hand | a zero reject count recorded |
| 7 | `>` → `>=`, by hand | a zero watermark per idle lane |

## RA budget

Unchanged: tests, one shared private step, one simplified test helper.

## Risks

- Criterion 7 needs a lane registered with no bundle; `lanes::register` is public, so the test
  lives in `tests/`, beside the other lane tests.

## Tasks

- **M8g.1** — the tests, `buffer`, the helper, the BACKLOG row, this ledger.
