# M106 — what a node holds for an index it is not serving

**Serves:** [M6i](../M6i/VERIFIED.md) criterion 7, whose node half is `NOT-RUN`: *"it is zero by
construction because nodes own nothing, and neither harness instantiates a serving node. A
zero asserted by a harness with no node in it would be evidence of nothing."* No backlog row.
This harness has a serving node in it.

## What is true today

These facts come from reading `pstore-server`, `pstore-engine` and `pstore-cache` at M105.

- A server holds one `Engine` per tenant a request named (`Api::engines`, M26). It holds
  nothing for a tenant no request named.
- An engine reads HEAD whole and keeps it decoded (`head_cache`, `schemas`). So **the node
  holds every index of a tenant it serves**, as that index's HEAD entry. This is not a cache
  that can be evicted per index: [M6i](../M6i/VERIFIED.md) criterion 5 measured the HEAD's
  bytes per index.
- The read cache never holds HEAD: HEAD is read with `get` and `get_with_tag`, which the cache
  passes through (only `get_immutable` and ranged reads are admitted). It holds what a query
  read: one index's segments and sidecars.

So the honest answer has two halves, and only one of them is zero.

## Delta

- `crates/pstore-server/tests/idle.rs`: a writer `Api` seeds a `MemoryStore`. A second
  `Api`, the serving node, shares the store and a `CacheCore`, and answers a fixed set of
  queries on one index of one tenant, over a decorator that logs every key it asks about by
  any method, found or not. ⚠️ Not `Accounted::ranges`: that logs only successful `get`,
  `get_range` and `get_suffix` reads, so a sidecar read or a 404 probe would pass unseen. The
  arms are compared by the keys touched, what the node holds (`Api::engines`, the cache's
  `resident_bytes`) and the HEAD's size.

**Not changed:** the server, the engine, the cache. If an idle index costs more than its HEAD
entry, this milestone reports it; it does not fix it.

## Acceptance criteria

1. **An index of a tenant the node does not serve costs it nothing.** The node serves tenant
   A in two arms: a store holding A alone, and one also holding 100 other tenants with 2
   indexes each. In both arms, after the same queries:
   - `engines()` is 1;
   - every key the node touched is A's HEAD, under A's WAL (`/wal/A/`) or under the served
     index (`/tnt/A/idx/served/`);
   - the cache's resident bytes are equal, and above zero.
2. **An idle index of a tenant the node serves costs it no read and no cache.** The node
   serves index `served` of tenant A in two arms: A with no other index, and A with 200 idle
   sibling indexes. The predicate of criterion 1 holds in both, so no key is touched under a
   sibling's `/tnt/A/idx/{sibling}/`.
3. **It does cost the HEAD entry**, and the ledger reports that number rather than zero: the
   HEAD's bytes in the 200-sibling arm minus the bytes in the bare arm, divided by 200, in
   bytes per idle index. Each sibling has one segment; the entry grows with segment count. The
   engine holds that HEAD decoded for as long as the tenant is in the registry.
4. ⚠️ **A control for every clause.** The predicate is run on a node that also queried one
   other tenant, and on one that also queried one sibling. Each must fail every clause that
   applies, each asserted separately: the engine count, the key set and the cache bytes for the
   other tenant; the key set and the cache bytes for the sibling. Otherwise a clause that
   checks nothing would pass.
5. **M6i criterion 7** says the node half is measured, and links here.
6. Gates: `scripts/gates.sh`.

## Test plan

| # | Seen red | What it catches |
|---|---|---|
| 1, 2 | the control of criterion 4 | a node that reads or caches an index it was not asked for |
| 3 | not red: a measurement, asserted above zero | an idle index reported as free while the HEAD carries it |

## RA budget

Unchanged: no shipping code changes.

## Risks

- **`resident_bytes` is the memory tier only.** The test's cache has no disk tier, so that is
  all of it.
- **The decoded HEAD's heap size is not measured.** No crate may hold `unsafe`, so there is
  no counting allocator, and RSS on a test binary is noise. The encoded size is what is
  reported, and the ledger says so.

## Tasks

- **M106.1**: the test, the M6i ledger line, this ledger.
