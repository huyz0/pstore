# M11 — `session` and `bounded` consistency

**Serves:** [BACKLOG](../BACKLOG.md) row 41. D-69's session token
([`session-and-affinity-protocol.md`](../../research/11-design/session-and-affinity-protocol.md))
and `bounded(max_staleness_ms)` from
[`consistency-model.md`](../../research/03-metadata-consistency/consistency-model.md) §2. Both
are refused today (M9i.2).

⚠️ **Split before starting**, as M9i was:
- **M11.1** — `session`: a token minted by writes and echoed by the client. A read with it
  reflects every durable write the session made, and is never served from an older HEAD than
  one the session was served or committed. Specified below.
- **M11.2** — `bounded`: a read served from a HEAD this process read within the client's
  tolerance, skipping the HEAD request. Specified when M11.1 lands, because both change what
  a query does with the HEAD it reads.

## M11.1 — `session`

### What is true today

- A durable write through process A is invisible to a read through B until a fold commits
  it. `eventual` serves B's read stale. `strong` refuses it, and to decide that it probes
  every registered lane.
- Nothing lets a client ask for exactly "my own writes, and never older than what I saw".

### Delta

**The token.** It is opaque to clients, carried in the `x-pstore-session` header both ways,
and echoed in the body. It is base64url, unpadded, of:
- `version` (1);
- `tenant` (16 bytes);
- `epoch` (8), the newest epoch this session has been served or committed;
- `flags` (1), with bit 0 = **overflow**;
- `n` (1), at most **16**;
- `n` entries of `(lane, next)` (16 bytes each).

An entry means "this session made a durable write to `lane` below sequence `next`".
**At most 283 bytes.**

**Writes mint it.** Every `PUT …/documents` response carries a token, returned in the
`x-pstore-session` header and as the body's `session`. It is the request's token (or an
empty one) merged with:
- this engine's last committed epoch;
- after a durable write, `(this lane, the engine's next sequence)`.
  - Spec review, B1: this holds **whatever this request's own flush returned**. Two concurrent
    durable writes on one process serialise their flushes, so the second can find its rows
    already written by the first and flush nothing. Its token still needs the entry.

A batched (not durable) write adds no entry. It is visible to this process's reads, as today,
and to no one else's until something flushes it. The token covers **durable** writes only.

⚠️ **What `session` does not promise** (spec review, M1). An answer on A fuses A's
memtable:
- other clients' unfolded durable rows;
- batched rows, from this session or another.

A session that saw those rows on A may not see them on B. The token orders HEADs and names
the session's own durable writes; it does not name what A's memory happened to hold.

**Merging.** Epochs take the max, entries take the max per lane, and overflow takes the OR.
A merge that would exceed 16 entries sets overflow and drops every entry.

**`consistency: "session"`** on a query. `eventual` stays the default (see "D-69's default"
below). The read is served only if the HEAD the query already read covers the token:
- every entry `(lane, next)` is **covered**: HEAD's watermark for `lane` is at least `next`,
  **or** `lane` is this engine's own and its memtable holds every sequence below `next` that
  HEAD has not folded. The own-lane rule is the one `settled` uses (M9j): the lane is resumed,
  the watermark is at least the resume point, and `next` is at most the engine's `next`;
- the token's `epoch` is at most the served epoch. A token ahead of the store cannot come from
  it, so a `session` read of it is `400 bad_session`, and not retryable. Other reads and writes
  echo it without checking (code review), because only `session` promises anything from it;
- with **overflow**, the read runs as `strong` instead, which covers every write the session
  acknowledged. On success the returned token clears overflow, holding one own-lane entry if
  this engine has unfolded bundles.

An uncovered entry is `503 not_folded`, with `retry_after`. It records the tenant as
requested, exactly as a refused `strong` does (M9i.2), so this process's next fold tick folds
it. The check reads nothing: the HEAD is the query's own, and the rest is memory. **0
requests beyond `eventual`**, unless the token overflowed.

**Every query returns a token.** The request's token (or an empty one) is merged with:
- the served epoch;
- the entries left after dropping those the served HEAD covers by watermark.

So tokens shrink as folds land. An `as_of` query returns the request's token unchanged, and
`session` with `as_of` is `400`, as `strong` is. A multi-query checks every sub-query that
asks for `session` against the same token, and returns the merge of all of them.

**Refusals, each `400 bad_session`:** a token that is not base64url, has the wrong version or
length, belongs to another tenant, holds more than 16 entries, or is ahead of the store.
`consistency` of `"Session"` stays `400 bad_request`, as today.

**D-69's default is not adopted.** `eventual` stays the default:
- `session` with no token is exactly `eventual`;
- but a default of `session` would make every `as_of` query a refusal, or a silent special
  case;
- and OQ-125 asks whether the default surprises anyone, which nothing here has measured.

The row stays open in `open-questions.md`, with a note there.

**The MAC is deferred, with a banner** on the protocol document §3 and §6.
- Its stated purpose is to stop a forged far-future token from making a node **wait**. This
  server never waits on a token: it refuses.
- So a forged token can only refuse its own reads, or request a fold of its own tenant, which
  a `strong` query can already do.
- The MAC lands with the token's routing `hint` (D-70), where a forged hint could steer load.
  That half is also deferred, because this server does not route.

**Does not change:** `eventual` and `strong` answers, any request count of theirs, the format.

### Acceptance criteria

1. **Read-your-writes across processes.**
   - Through A: a durable write returns a token.
   - Through B: a `session` query with that token is `503 not_folded`, with `retry_after`.
   - After B's `fold_due`, the same query answers the write.
   - An `eventual` query through B before the fold does not see it.
1b. (Spec review, B1) **Concurrent durable writes.** Two durable writes through A run at once.
   Each token, read through B, is refused until a fold, then answered with that write.
2. **Own process, no fold.** Through A, a durable write, then a `session` query with its token
   answers the write at once.
2b. (Spec review, minor) **A restart on the lane.** A token from a process on lane L is read
   through a new process on L that holds none of the token's writes in memory. It is refused
   until a fold, both before and after the new process writes.
3. **Monotonic.**
   - The token a query returns has an epoch no lower than the one sent.
   - A token whose entries are folded comes back with no entries: its decoded length is 27
     bytes.
   - A token forged with an epoch past HEAD is `400 bad_session`.
4. **Cost.** With a token that is covered, a `session` query issues exactly the requests of the
   same `eventual` query, compared by `meta.cost`.
5. **Overflow.**
   - A token merged across 17 lanes sets overflow.
   - A `session` read with it is refused while any write is unfolded, and served after a fold.
   - The token it returns has overflow cleared.
6. **Batched writes.** A batched write's token has no entry. The write is visible to a
   `session` read on its own process, and not required of B's.
7. **Refusals.** Each is `400`: every `bad_session` case above, `session` with `as_of`, and a
   token from another tenant.
8. **Multi-query.**
   - A multi-query with one `session` sub-query refuses as criterion 1 does.
   - Once covered, it returns one merged token.
9. `./scripts/gates.sh` passes, and `./scripts/mutants.sh` over the diff misses 0.

### Test plan

| # | Fails first | Mutation it catches |
|---|---|---|
| 1 | `session` refused as unknown | a covered check that ignores other lanes; the refusal not requesting a fold |
| 1b | as 1 | the entry minted from the flush's own return |
| 2 | as 1 | the own-lane rule dropped (refuses its own write) |
| 2b | as 1 | the own-lane rule without its resume-point guard |
| 3 | no token returned | entries not pruned; epoch merged with min |
| 4 | as 1 | a probe added on the session path |
| 5 | as 1 | overflow not set at 17; overflow not run as strong |
| 6 | as 1 | a batched write minting an entry |
| 7 | accepted or ignored | a check unchecked |

### RA budget

- `session` with a covered or refused token: **0 requests beyond `eventual`**.
- With overflow: `strong`'s (M9i.2).
- Writes: unchanged. The token is computed from memory.

### Risks

- A write-only session through many processes overflows at 17 lanes, and then pays `strong`
  until a read clears it.
- The token is unsigned. That is safe only while nothing waits or routes on it, so this
  document and the banner must say so.

## M11.2 — `bounded`

### What is true today

- Every query reads HEAD, including `eventual` (M9i.2: "never cached"). That is 1 GET and one
  sequential round on every query, even when the client would accept an answer seconds old.
- `consistency-model.md` §2 promises `bounded(max_staleness_ms)`: serve from the cached
  epoch if it was validated within the client's tolerance. Nothing caches one.

### Delta

**The request.** `"consistency": "bounded"` with `"max_staleness_ms"`, an integer from 0 to
3,600,000 (one hour, D-39's fold age). A larger value would let a process serve one HEAD
indefinitely (spec review, B2).

**The engine.** It keeps the last HEAD a `bounded` read fetched, with the instant it was read.
It is one per engine, so one per tenant a process serves, and it is filled only by `bounded`
reads: no other path pays to clone HEAD.

A `bounded` read uses that HEAD (a **hit**) only when both of these hold:
- it is no older than `max_staleness_ms`, measured with `tokio::time::Instant` from the moment
  **before** its GET was issued, so that `staleness_ms` never understates the age;
- **this engine has committed nothing since** (spec review, B1). The engine's last committed
  epoch must be at most the cached epoch.
  - A fold, drop, compaction or `gc` by this engine changes what it would answer, and the
    prune fallback below does not see every one of them.
  - A drop commits without pruning, so without this rule a hit would keep answering a
    dropped index.

Otherwise the read is a **miss**: it reads HEAD, as today, and caches it. A miss replaces the
cached HEAD only if its epoch is at least the cached one, so concurrent misses cannot move
the cache backwards.

**A hit that fails falls back** (spec review, B2): any error from a hit drops the cache and
re-runs the read once as a miss.
- The case this exists for is `gc` reaping a segment the cached HEAD still names. That fails
  in `Segment::open`, and without the fallback it is a `500` on every hit until the entry
  expires.
- A genuine error fails the miss too, and is returned from it. That includes a client's own
  error, such as a schema conflict, which costs one extra HEAD read. That is intended: the
  fallback does not classify errors.

**The schema cache is monotonic** (spec review, M1). `answer` records the schemas of the HEAD
it served. From a hit, that could roll the door's check back to an older HEAD, so the engine
records schemas only from a HEAD whose epoch is at least that of the one it last recorded.

⚠️ **A cached HEAD older than a prune this engine made is never paired with the memtable**
(M9c.1). The fold that pruned would have folded rows the memtable no longer holds into a HEAD
the cache does not have. `fresh_view` already refuses that pairing, and the read then fetches
HEAD fresh, as a miss that refills the cache. So this process's own writes stay visible under `bounded`: batched, durable and
folded alike.

**The response.** `meta.staleness_ms` is the age of the HEAD served: 0 for a HEAD this query
read, on every level. That is design rule 10's "the epoch and staleness it was served at".

**Refusals, each `400 bad_request`:**
- `bounded` without `max_staleness_ms`;
- `max_staleness_ms` without `bounded`;
- a value that is not an integer from 0 to 3,600,000;
- `bounded` with `as_of`.

`"bounded"` leaves M9i.2's refusal list, and `"Bounded"` takes its place.

**Multi-query.** Each sub-query is its own read: each may hit or miss.

**Session.** A `session` read always reads HEAD, so M11.1's `400` for a token ahead of the
store stays sound: `session` and `bounded` are separate levels, and no read is both.

**Does not change:** `eventual`, `strong` and `session`, their request counts, and the format.

### Acceptance criteria

1. **A hit skips HEAD.** Within the bound, a second `bounded` query costs exactly 1 read fewer
   than the same `eventual` query, for a relevance query and for a `rank_by` order, and its
   depth is lower. `meta.staleness_ms` equals the time elapsed since the first query, under
   paused time.
2. **A miss reads it.** Past the bound, a `bounded` query costs what `eventual` does, and
   reports `staleness_ms` 0.
3. **Stale, and says so.** Another process folds a write. A `bounded` hit does not answer it
   and reports the cached epoch. Past the bound, it answers it.
4. **Own writes stay visible.** Within the bound, this process's batched write, then its
   durable write and fold, are each answered by the next `bounded` read. The second case is
   the prune fallback.
4b. (Spec review, B1) **Its own commits end a hit.** Within the bound, a drop of the index
   through the same process makes the next `bounded` read `404`.
4c. (Spec review, B2) **A failed hit falls back.** Within the bound, another process drops the
   index and runs `gc` with retention 0, reaping its segments. The next `bounded` read through
   the caching process answers `404`, not `500`.
5. **Refusals.** Each case above is `400`, including 3,600,001.
6. `./scripts/gates.sh` passes, and `./scripts/mutants.sh` over the diff misses 0.

### Test plan

| # | Fails first | Mutation it catches |
|---|---|---|
| 1 | `bounded` refused | the cache ignored; the age compared the wrong way |
| 2 | as 1 | a HEAD served past its bound |
| 3 | as 1 | the cache refreshed on every read |
| 4 | as 1 | the prune fallback removed: a folded own write vanishes |
| 4b | as 1 | the commit check removed |
| 4c | as 1 | the fallback removed: `500` |
| 5 | accepted or ignored | a check unchecked |

### RA budget

- A `bounded` hit: **1 GET and one round fewer** than `eventual`.
- A miss: the same as `eventual`.
- Memory: one HEAD per engine that has served a `bounded` read.

### Risks

- A cached HEAD can name an object `gc` has since reaped. The fallback turns that error into a
  fresh read.
  - No reaped object is known to serve a wrong answer instead of erroring. A missing centroid
    table degrades to an exact scan of the same rows, which is correct.
  - Missing dictionaries and delete vectors both error (spec review). A new object whose
    absence is silent would reopen this.

## Tasks

- **M11.1** — `session`; criteria 1–9.
- **M11.2** — `bounded`; its criteria 1–6.
