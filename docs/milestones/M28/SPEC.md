# M28 — The read cache's three loose ends

**Serves:** [BACKLOG](../BACKLOG.md) rows 45, 47 and 48, all opened by [M20](../M20/SPEC.md)'s
reviews and all inside `pstore-cache`. Each one is small. They share one crate, one test
binary and one sweep, so they are one milestone.

## What is true today

- **Row 47, the scan's claim.**
  - `CacheCore::fetch_once` claims a range's singleflight gate on a miss, whatever the class.
  - A `Class::Scan` read is never admitted (D-50). So bulk readers waiting on a scan's claim
    wake to a miss, and each fetches again: N waiting readers cost 1 + N requests.
  - A reader that fetches without a claim also removes whichever gate is in the map when it
    finishes. That is the claim-free path after a failed fetch, and a scan under this
    milestone. The gate it removes may be a later claimant's, so the next arrival claims
    again and fetches a range already in flight.
  - Rare today: a query and a scan share no range (M20 criterion 9's amendment).
  - ⚠️ **A cancelled claimant leaves its gate forever** (spec review M1). A server drops a
    handler's future when its client disconnects. A claimant dropped mid-fetch never removes
    or closes its gate, so its waiters, and every later read of that range on that node, hang.
    Today a claim-free fetcher can remove the gate by accident. Once only a claimant removes
    its gate, nothing could.
- **Row 45, the LRU.**
  - Each class's memory arena keeps recency as a `Vec<Id>`.
  - A hit's `touch` searches it, and each eviction removes its head. Both are O(entries) under
    the arena's lock.
  - Since M20 one core serves every tenant on a node, so this grows with the node's whole
    working set.
- **Row 48, the disk hash.**
  - The disk tier (`foyer`) finds an entry by `XxHash64` over `Id`'s `Hash`. `Id` derives
    `Hash`.
  - A derived hash feeds the enum discriminant as an `isize`, and integers in native byte
    order. Rust promises neither is stable across releases.
  - A toolchain upgrade that changes it makes every recovered entry miss. The bytes are never
    wrong, since `foyer` checks the key on load, but the tier is silently flushed and its
    directory is filled with entries that can never be found.

## Delta

**Row 47 — only a claimant removes a gate, and a scan never claims.**
- A `Class::Scan` miss still waits on a gate it finds, since a bulk claimant admits what a scan
  can then read. It never inserts one.
- A fetch removes the gate only if it inserted it. Claim-free fetches leave the map alone,
  whether after a failed claimant or for a scan.
- **The claim is a guard.** Dropping it, on success, failure or cancellation, removes its gate
  and closes it. So a cancelled claimant's waiters wake, miss, and fetch claim-free.
  - ⚠️ `Drop` cannot await (spec review round 2, M1). So the gates move out of `State`'s
    `tokio` mutex into a `std::sync::Mutex` per arena, which is never held across an await,
    and which the guard's `Drop` locks.

**Row 45 — recency in O(log entries).**
- Each arena keeps a monotonically increasing tick per entry, and a `BTreeMap<tick, Id>`
  ordered oldest first.
- A touch moves an entry's tick, and an eviction pops the first. No new dependency.
- What is evicted, and when, is unchanged: least recently used first, within a class's quota.

**Row 48 — a hash defined by the entry format.**
- `Id`'s `Hash` is written by hand. It writes `Id`'s structural encoding, the bytes
  `foyer::Code::encode` already writes, in order and with no allocation (spec review m6:
  streaming `XxHash64` gives the same value however the bytes are split).
- So a key's hash is `XxHash64` of bytes this crate defines, which no toolchain can change.
- The entry format string becomes `pstore-cache/2`. A directory filled under the old hash is
  emptied on its first open, by the identity check that exists, rather than left as
  unfindable entries.

**Not changed:**
- the memory and disk quotas;
- the classes and the admission rules (a scan is never admitted);
- the disk tier's identity check;
- every other test's counts, and every engine and server cost.

## Acceptance criteria

1. **A scan's miss costs no bulk reader a request.** A scan's fetch is held, then 8 bulk reads
   of the range arrive, then it is released: 2 requests, not the parent's 9.
2. **A claim-free fetcher leaves another's gate.** A scan's fetch and then a claimant's are held.
   The scan's finishes, then a second bulk read arrives and waits: 2 requests, not 3.
3. **A failed or cancelled claim releases its waiters and its gate.** A failed fetch's waiters
   fetch claim-free and get the bytes. A claimant aborted mid-fetch: its waiters get the bytes.
   After either, two later reads of the range cost 1 request. On the parent, cancellation hangs.
4. **Recency is unchanged.** `eviction_is_least_recently_used` passes, and so does a test of 8
   entries checked step by step against a model LRU.
5. **Recency is O(log entries).** 100 000 entries in one arena, each touched in reverse order,
   then evicted by as many new ones, within 20 s in a debug build. A bound, not a measurement.
6. **The disk hash is pinned.** `foyer`'s hash of one `Id` per shape equals a literal: `XxHash64`
   with seed 0 over its encoding, computed outside this code (below).
7. **The format moved.** A directory recorded as `pstore-cache/1` is emptied on open, and the
   existing restart tests still serve from disk.
8. **Gates.** `./scripts/gates.sh` is green, and the mutation sweep over M28's source diff misses
   0. Every miss is closed by a test or named as equivalent.

## Test plan

Criteria 1–4 in `crates/pstore-cache/tests/cache.rs`, over a store whose fetches are released
one at a time and can be made to fail (`Barriered` releases all at once and cannot fail). 5 and
6 are unit tests in `src/cache.rs` and `src/disk.rs`, on crate-private `State` and `Id`, with no
async, so a mutant run pays milliseconds. 7 is in `tests/disk.rs`.

⚠️ **Tests 3 (failure) and 4 guard behaviour the parent already has** (spec review M2). They are
seen red by applying the mutation named, and `VERIFIED.md` records which.

| # | Test | Red on the parent because / mutation it catches |
|---|---|---|
| 1 | `a_scan_miss_costs_no_bulk_reader_a_request` | 9 requests; a scan that claims |
| 2 | `a_claim_free_fetch_leaves_the_claimants_gate` | 3 requests; removal by anyone |
| 3 | `a_failed_claim_releases_its_waiters_and_its_gate`, `a_cancelled_claim_releases_its_waiters` | cancellation hangs; the guard's removal or close dropped |
| 4 | `eviction_follows_a_model_lru` | a touch that does not move the tick; eviction from the newest end |
| 5 | `recency_is_logarithmic` | the `Vec`'s scan (bounded by the test's own clock, so the parent fails, not hangs) |
| 6 | `the_disk_hash_is_the_encodings` | the derived `Hash`; a field left out |
| 7 | `a_tier_of_the_previous_format_is_emptied` | the format string unchanged |

**Test 6's literals** come from a reference `XxHash64` written in Python, from the published
algorithm, over the encoding written out by hand. They are checked first against the
algorithm's published test vector, so they depend on neither this code nor `foyer`.

## RA budget

| Op | W | Rseq | Rpar | List | Depth |
|---|---|---|---|---|---|
| A bulk read waiting on a scan's miss | 0 | −1 each waiter but one | 0 | 0 | unchanged |
| A read arriving after a claim-free fetch finished | 0 | −1 | 0 | 0 | unchanged |
| Any other read | 0 | unchanged | unchanged | 0 | unchanged |
| A node's first open after upgrading | 0 | the disk tier's entries, once | 0 | 0 | unchanged |

## Risks

- **The format bump flushes every disk tier once**, on the upgrade that ships it. It is a cache:
  the cost is a cold start, the same as an AZ loss, and it is what a hash change would have
  cost silently anyway.
- **A claimant cancelled between its fetch and its admission** admits nothing. Its waiters then
  fetch claim-free, one request each. That is a cost under cancellation, not a hang.
- **A tick counter that wraps.** It is a `u64` incremented once per admission or hit: at 10⁹ a
  second it wraps after 584 years.

## Tasks

- **M28.1** — The gate's ownership, its guard and the scan's claim (row 47), with tests 1–3.
- **M28.2** — The tick-ordered recency (row 45), with tests 4–5.
- **M28.3** — The hand-written hash and the format bump (row 48), with tests 6–7.
- **M28.4** — The ledger, `BACKLOG.md` rows 45, 47 and 48 closed, and the roadmap row.
