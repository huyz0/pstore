# M20 — A read cache that survives a restart

**Serves:** D-23 (the disk cache survives a restart and is validated by key), D-21 (class
quotas, now on disk too), D-22 (`foyer` for the disk tier). It is the tier `hint_cache_warm`
needs, which moves to M21.

## What is true today

- `pstore-cache::Caching` is an in-memory LRU with one quota per class. It caches ranges and
  suffixes, and whole objects only through `get_immutable`. It refuses `get`.
- **The server uses no cache at all.** Every query pays every range read, and a restart has
  nothing to lose.
- There is no disk tier. [`roadmap.md`](../../research/11-design/roadmap.md) names it as not
  built: "a rolling restart still flushes every cache".

## Delta

**A disk tier behind the memory tier.** `Caching` gains an optional `DiskTier`, opened on a
local directory with a byte budget.
- A memory miss reads the disk tier. A disk miss fetches from the store and admits the bytes
  to both tiers. The singleflight is unchanged.
- **One `foyer` hybrid cache per class**, in its own subdirectory, each holding its class's
  share of the disk budget. The shares are the memory tier's: a tenth for pinned, a tenth for
  meta, and the rest for bulk. A bulk burst can evict only bulk entries, on disk as in memory
  (D-21).
- `foyer`'s own memory tier is sized to its minimum; ours stays in front of it.
- Entries are written to disk on admission (`WriteOnInsertion`), so a crash loses only
  writes still in flight.
- **Reopening recovers entries** (`RecoverMode::Quiet`). It reads the tier's own files only,
  never the blob store.

**Validated by key, and by store.** A cached key's bytes never change: see Risks. What does
change is the store behind the directory, so the directory holds an `identity` file. It names
the backend, and the endpoint and bucket for S3.
- An `identity` that differs, or is missing, **empties the tier before it serves anything**.
- The file is written once the tier is empty.

**A broken disk degrades, never fails.** If the directory cannot be opened, or a disk read or
write fails, the tier is bypassed: the read goes to the store, as it would uncached.
- A failure to open leaves the process RAM-only, with `DiskTier::state()` reporting
  `Bypassed(reason)`.
- A failed disk read counts as a miss.

**Shared across tenants, outside each tenant's accounting.** The state moves into a shared
`Arc<CacheCore>`. `Caching::over(inner, core)` wraps one tenant's view.
- Every cached key names its tenant, so one core serves every tenant.
- A hit is never billed to the tenant, because accounting sits under the cache
  ("`Caching<Accounted<_>>`", this crate's own doc).
- `Api::with_cache(store, lane, core)` builds each engine over `Caching<TenantView<S>>`.
  `Api::new` keeps today's uncached stack, so no existing request count moves.

**Configuration.**
- `PSTORE_CACHE_DIR` turns the cache on.
- `PSTORE_CACHE_RAM_BYTES` sets the memory budget (default 256 MiB), and
  `PSTORE_CACHE_DISK_BYTES` the disk budget (default 4 GiB).
- A value that is not a positive integer is refused by name, as `PSTORE_GC_*` values are.
- Without `PSTORE_CACHE_DIR` the server is uncached, as today.
- `deploy.md` gains the three rows, and a section on sizing and on sharing a directory.

**Does not change:** what is cacheable (`get` is still refused, on disk too); any write;
HEAD reads; the request counts of an uncached server; D-50's scan bypass, which the
engine's classes already carry.

## Acceptance criteria

1. **A restart is not a flush.** A server over a cache directory answers a query. A new
   `Api` over a new `CacheCore` on the same directory answers the same query identically. It
   costs 1 HEAD read and **0** range or suffix reads, from `Accounted`'s counters.
2. **Warm answers equal cold ones.** Queries repeated through the cache, before and after
   a reopen, give answers equal to an uncached server's over the same store.
3. **Another store's directory serves nothing.** A directory filled for store A, then opened
   for store B, which holds a different object at the same key: B's answer is B's, and B
   pays the uncached request count.
4. **Quotas hold on disk.** A meta entry is admitted, then a bulk burst of twice the bulk
   share. After a reopen, the meta entry is a hit, at 0 requests.
5. **A broken directory bypasses.** A cache opened on a path that is a regular file
   reports `Bypassed`, and every read answers correctly at the uncached request count.
   `Api::with_cache` starts over it.
6. **A hit is not billed.** Through the API, the second identical query's `cost` shows
   0 range reads.
7. **`get` is never cached, on disk either.** Two `get`s of one key, around a reopen, make
   2 requests.
8. **Configured by name.** `PSTORE_CACHE_DIR` absent means `Config.cache` is `None`. Each
   invalid `PSTORE_CACHE_*_BYTES` is refused, naming the variable. The defaults are as stated.
9. **Sealing is deterministic.** Two engines on the same lane compact identical HEADs
   (same writes, same order, separate stores). Their merged segments, and each sidecar,
   are byte-identical.
10. `./scripts/gates.sh` passes, and `./scripts/mutants.sh` misses 0.

## Test plan

| # | Fails first | Mutation it catches |
|---|---|---|
| 1 | the reopened query reads its ranges again | the disk tier not consulted on a memory miss; not admitted on fetch; recovery off |
| 2 | — (guards 1 against serving wrong bytes) | a disk hit returning another key's or range's bytes |
| 3 | B is served A's bytes | the identity not compared; compared but not emptied |
| 4 | the meta entry is gone | one shared disk instance; a class's share swapped |
| 5 | opening panics or errors | a disk error propagated in place of a bypass |
| 6 | a hit billed a range read | the cache under accounting, not over it |
| 7 | the second `get` is a hit | `get` admitted to disk |
| 8 | the variables are ignored | the parse, the defaults, the refusal |
| 9 | — (a guard: it may pass first; if it fails, sealing is fixed before M20.1) | iteration order of a hash map reaching a sealed byte |

## RA budget

A hit costs 0 requests; a miss costs what it does today. Reopening a directory costs 0 blob
requests and no LIST: it reads local files only. HEAD is never cached, so a query's depth
is unchanged cold, and one round of range reads shallower warm (M4d measured 2 → 1).

## Risks

- **A key whose bytes change would be served stale, for as long as the disk keeps it.** The
  cache relies on three things: a committed key is never rewritten, since epochs only grow;
  an uncommitted one is never read, so it is never cached; and nothing deletes a tenant's
  HEAD. The one exception known is M19's same-lane case, where two compactions write the same
  key. It is safe only if both write the same bytes, which criterion 9 pins. A new writer of a
  derived key must keep that property, or give the key a content hash.
- **`foyer` recovery reads each block header** on open. That is local I/O proportional to the
  device, not a LIST. Measured on this container: 12 ms for 64 MiB. That is `provisional`,
  and it is not the scale D-23 means.
- **`foyer` preallocates its whole budget** as files, so disk use is the budget from the start.
- **Endurance and throttling** (disk-space-management.md §7) are not configured here. Any
  number measured on this container is `provisional`.

## Tasks

- **M20.1** — the disk tier, the identity check and the bypass in `pstore-cache`, with
  `CacheCore` shared.
- **M20.2** — `Api::with_cache`, `Config.cache`, and `main.rs` and `deploy.md`.
