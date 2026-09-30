# M20 — A read cache that survives a restart

**Serves:** D-23 (the disk cache survives a restart), D-21 (class quotas, now on disk too),
D-22 (`foyer` for the disk tier), D-50 (scans bypass the cache). It is the tier
`hint_cache_warm` needs, which moves to M21.

## What is true today

- `pstore-cache::Caching` is an in-memory LRU with one quota per class. It caches ranges and
  suffixes, and whole objects only through `get_immutable`. It refuses `get`.
- **The server uses no cache at all**, and there is no disk tier.
  [`roadmap.md`](../../research/11-design/roadmap.md) says so: "a rolling restart still
  flushes every cache".

## Delta

**A shared core, a disk tier, and one read path.**
- The cache's state moves into `CacheCore`, which is shared. `Caching::over(inner,
  Option<Arc<CacheCore>>)` wraps one tenant's view.
- With no core, it forwards every call verbatim. `Api` always builds engines over
  `Caching<TenantView<S>>`, so that `Api::new` (no core) and `Api::with_cache(store, lane,
  core)` share one engine type.
- `CacheCore::open(dir, ram, disk, identity)` puts the disk tier behind the memory tier:
  - It is **one `foyer` hybrid cache per class**, each in its own subdirectory, with the
    memory tier's shares of the disk budget: a tenth pinned, a tenth meta, the rest bulk
    (D-21). The block size is 4 MiB, and an entry larger than a block is not admitted to
    disk.
  - A memory miss reads the disk tier. A disk miss fetches from the store and admits the bytes
    to both tiers. Disk lookups run **outside** the memory tier's lock, and concurrently for
    the ranges of one call. The singleflight is unchanged.
  - Entries are written on admission (`WriteOnInsertion`).
  - The foyer key is `Id` itself, encoded structurally as a tag plus fields, never as a joined
    string.
- `CacheCore::close()` flushes and closes the tier. `main.rs` calls it on shutdown.

**Recovery, and what it trusts.**
- Reopening recovers entries (`RecoverMode::Quiet`) from the tier's files alone, with no
  blob request.
- foyer checks each entry's checksum on read, and a mismatch is a miss (block engine
  `load`). The tier relies on that for torn and corrupt entries.
- ⚠️ **Deviates from D-23's "index the directory in a small file; do not scan it".** foyer's
  recovery reads every block header. A banner on D-23 records it, and criterion 10 bounds it.

**Validated by store.** The directory holds an `identity` file with the string the caller
passes. The server passes:
- the endpoint, the bucket, and a **store id**: a random 128-bit value at `_pstore/store-id`,
  read once at startup and created if absent with create-if-absent.
  - A bucket wiped and recreated gets a new id, so its old entries are never served.
  - A bucket **restored from backup** keeps the old id; `deploy.md` says to empty the
    directory.
- `Backend::Memory` with `PSTORE_CACHE_DIR` is **refused by name**. Each start is a new empty
  store whose epochs restart, so its keys repeat with other bytes.

An `identity` that differs, or is missing, **empties the tier before it serves anything**:
1. delete the class subdirectories, and sync the directory;
2. then write and sync `identity`.

**One directory per lane.** The tier opens `<dir>/lane-<n>`. A lane has a single writer, so
two processes never share foyer's files, and no lock is needed.

**A broken disk degrades, never fails.**
- A directory that cannot be opened leaves the core RAM-only, reporting
  `DiskState::Bypassed(reason)`.
- A foyer error on lookup is a miss; on insert, it is dropped.

**D-50: scans do not admit.** `Class` gains `Scan`.
- A `Scan` lookup probes the **bulk** arena, in both tiers, so a hit is served. A miss fetches
  **without admitting**, to either tier.
- The engine wraps its store in a `Scanning` adapter for the reads made by compaction, the
  fold's delete and upsert pass, and `Engine::scan`.
  - The adapter re-classes **only bulk and unclassed reads** as `Scan`. Meta and pinned reads
    (a segment's footer, its sidecars, centroids) keep their class and admit as usual,
    since they are small and read again.
  - It overrides `get_range`, `get_ranges`, `get_suffix` and each `_as` form. The trait's
    default `get_ranges` coalesces into a classless `get_range`, which would be admitted as
    bulk.
- The two match sites on `Class` (`Caching::arena`, and pstore-meter's pass-through) gain the
  arm.

**Configuration.**
- `PSTORE_CACHE_DIR` turns the cache on.
- `PSTORE_CACHE_RAM_BYTES` defaults to 256 MiB, and `PSTORE_CACHE_DISK_BYTES` to 4 GiB.
- A value that is not a positive integer is refused by name. A `*_BYTES` value without the
  directory is also refused by name.
- Without the directory the server is uncached, as today.
- `deploy.md` gains the rows, the lane subdirectory, and the backup-restore rule.

**Does not change:** what is cacheable (`get` is still refused, on disk too); any write; HEAD
reads; the request counts of an uncached `Api`. The crate docs (`lib.rs`, `cache.rs`) are
updated to match.

## Acceptance criteria

1. **A restart is not a flush.** `Api::with_cache` answers a query, and its core is closed. A
   new `Api` over a new core on the same directory answers identically, at
   `cost.blob_reads == 1` (HEAD alone), plus one read per segment a vector query opens that
   has no centroid table. ⚠️ Amended at implementation: below `EXACT_SCAN_THRESHOLD` a segment
   has none, the query's `get_immutable` of it is a 404, and no cache keeps a 404 (BACKLOG
   row 46).
2. **Warm answers equal cold ones.** Hits and their order are equal to an uncached `Api`'s,
   before and after a reopen.
3. **Another store's directory serves nothing.**
   - A directory filled under identity A, opened under B: a read of the same key fetches B's
     object.
   - Opened under B, closed with nothing inserted, then reopened under B: still no entry of
     A's.
   - With `identity` deleted, reopened under A: nothing served.
4. **Quotas hold on disk.** A meta entry is admitted, then twice the bulk share of bulk
   entries. After a reopen, the meta entry is a hit at 0 requests.
5. **Corruption is a miss.** One byte inside a cached value is flipped: the value's bytes
   are found by searching the tier's data files, not guessed at in padding. After a reopen,
   the read answers the store's bytes at the uncached count.
6. **A broken disk bypasses.**
   - A directory path that is a regular file: `Bypassed`, and correct reads at the uncached
     count. `Api::with_cache` starts over it.
   - The tier's data files truncated under an open core: correct reads.
7. **A hit is not billed.** Through the API, the second identical query costs what criterion 1
   does.
8. **`get` is never cached, on disk either**: two `get`s around a reopen make 2 requests.
   - `Range(k,0,n)`, `Whole(k)` and `Suffix(k,n)` are three distinct entries.
   - An entry of 4 MiB + 1 byte is a memory hit, and after a reopen a disk miss, with no
     error.
9. **Scans do not admit, and do hit.**
   - A cold compaction, and a cold `Engine::scan`, leave the bulk arena's memory and disk
     entries unchanged. A query admits.
   - A `Scan` read of a range a bulk read admitted costs no request, from memory and from
     disk, and a `Scan` hit on disk is not promoted.
   - ⚠️ **Amended at implementation**, from "a compaction is served what a query warmed": a
     query reads a data section whole, a scan by block, and the cache keys by exact range.
10. **Reopening is bounded.** A full 256 MiB tier reopens in under 1 s here, measured by
    `a_full_tier_reopens_within_a_second`. That test is `#[ignore]`d, because wall-clock time is
    not deterministic and a mutation sweep would rerun it. It is run by hand with
    `--ignored`, and its number is recorded as `provisional`. It is not a gate.
11. **One directory per lane.** Cores for lanes 1 and 2 on one directory share no file.
12. **Configured by name.**
    - No `PSTORE_CACHE_DIR` means `Config.cache` is `None`.
    - Each invalid `*_BYTES`, a `*_BYTES` without the directory, and `Backend::Memory` with
      the directory are each refused by name.
    - The store id is created once and read back unchanged.
13. **Uncached is unchanged.** The server suite's request counts pass under `Api::new`
    unchanged. `the_uncached_api_repeats_its_reads` pins a repeated query at the same count.
14. **Sealing a key twice writes the same bytes.** Two compactions on one lane seal one key:
    the second through `compact_with_interference_for_test`, on one store. The segment and
    each sidecar are byte-identical before and after the loser's PUT. If this fails, M20
    stops.
15. `./scripts/gates.sh` passes, and `./scripts/mutants.sh` misses 0.

## Test plan

| # | Fails first | Mutation it catches |
|---|---|---|
| 1 | the reopened query reads its ranges again | disk not consulted; not admitted; recovery off; `close` not flushing |
| 2 | — (guards 1) | a disk hit returning another entry's bytes |
| 3 | B is served A's bytes | identity not compared; recovery merely disabled rather than files deleted; missing read as a match |
| 4 | the meta entry is gone | one shared disk instance; shares swapped |
| 5 | the flipped entry is served | — (relies on foyer; pins it across upgrades) |
| 6 | open panics or a read errors | a disk error propagated |
| 7 | a hit billed | the cache under accounting |
| 8 | the second `get` hits; ids collide | `get` admitted; a string-joined key |
| 9 | the compaction admits; the warmed compaction reads again | the adapter unused; `Scan` admitted; `Scan` never probing bulk; a `get_ranges` override missing |
| 10 | — (a bound) | — |
| 11 | lanes share files | the lane dropped from the path |
| 12 | the variables are ignored | the parse, the defaults, each refusal |
| 13 | a repeated uncached query costs less | a core-less `Caching` caching |
| 14 | — (a guard) | hash-map order reaching a sealed byte |

## RA budget

- A hit costs 0 requests; a miss costs what it does today.
- Reopening costs 0 blob requests and no LIST.
- Startup with a cache costs 1 GET, plus 1 conditional PUT and 1 GET the first time ever. That
  is per node, never per tenant.
- HEAD is never cached, so cold depth is unchanged. Warm depth is one round shallower (M4d
  measured 2 → 1).

## Risks

- **A key whose bytes change is served stale**, for as long as the disk keeps it. The cache
  relies on three things: a committed key is not rewritten, since epochs only grow; an
  uncommitted key is never read; and nothing deletes HEAD. Two known exceptions:
  - M19's same-lane compaction, which criterion 14 pins;
  - a process paused between its HEAD read and its segment PUT, on a lane whose restarted
    successor has since committed that key. Its unconditional PUT overwrites a live
    segment. That is new [BACKLOG](../BACKLOG.md) row 44, an existing bug of the store: the
    cache makes it last longer, and does not create it. foyer's key check keeps the lane
    directory itself safe.
- **Recovery reads every block header** (criterion 10). At NVMe scale that is D-23's cost,
  and it is left to M21 or later.
- **foyer preallocates its budget**, and endurance throttling (disk-space-management.md §7)
  is not configured. Every number measured here is `provisional`.
- **The memory tier's LRU is a `Vec`**, whose touch is O(entries), and it is now shared. That
  is a cost, not a correctness issue. It is recorded as a backlog row, not changed here.

## Tasks

- **M20.1** — `CacheCore`, the disk tier, the identity check and emptying, the bypass, and
  `close`, in `pstore-cache`. Also the D-23 deviation banner in `affinity-and-coldstart.md`,
  and BACKLOG rows 44 (the paused segment PUT) and 45 (the `Vec` LRU).
- **M20.2** — `Class::Scan`, and the engine's `Scanning` adapter.
- **M20.3** — `Api::with_cache`, `Config.cache`, the store id, `main.rs`, and `deploy.md`.
