# M213: a tenant's buffered rows are charged the memory they hold

## Serves
D-56 (per-tenant memory quotas) in [memory management](../../research/09-rust-stack/memory-management.md),
and workstream A of [the delivery plan](../../comparisons/turbopuffer-next-plan.md): "Bound ...
per-tenant buffered bytes". It closes the gap [M141](../M141/VERIFIED.md) recorded: its
bound "bounds rows, not memory".

## Delta
Today (M141) a pending row is charged `Document::footprint()`, its data's bytes (id, 4 per
dense component, attribute keys and values), and a flushed, unfolded row its bundle's
encoded share. Neither counts the maps, slots and spare capacity the row holds.

**A finding first.** M141's measurement (`buffered_bench.rs`) summed `bytes_allocated` and
`bytes_reallocated`, but `stats_alloc` adds a reallocation's growth to both, so every grown
vector was counted twice: its 4.3x for 768-dimension rows was really about 1.2x. Measured
correctly (allocated less deallocated), a 2-dimension row holds about 700 bytes and is
charged 18, a 768-dimension row holds about 3.8 KB and is charged 3.1 KB. The gap is fixed
per-row and per-map overhead, not vector bytes.

After, `Document::footprint()` is the row's **held bytes**, an upper bound on its heap and
slot, computed from what it holds:
- its slot in the buffer's `Vec`, twice `size_of::<Document>()` (a grown `Vec` is at most
  half empty);
- the capacity of its id, field names, attribute keys and strings, vectors (`capacity()`,
  not length) and arrays, each array's elements at `size_of::<Value>()` plus their own;
- each non-empty `BTreeMap`'s nodes: one leaf of 11 entries up to 11, else a bound on leaves
  and internal nodes, each at its size from `size_of` of its key and value;
- 16 bytes for each of those allocations, for the allocator's header and rounding.
`Value::footprint()` is unchanged (a patch's estimate uses it, trued up when buffered).

A flushed row stays in memory until a fold, so it is charged the same held bytes, not its
bundle's share: `Buffered::unfolded` and the cap check sum the held bytes of every flushed
batch not yet folded. A row's charge no longer changes when it is flushed. The bundle size
still drives scheduled folds (`Engine::unfolded`), and in addition (found in review) a
tenant is due for a scheduled fold once its flushed rows' held bytes reach
`tenant_flush_bytes`: held bytes can fill the cap while the bundles are far below
`PSTORE_FOLD_BYTES`, leaving every batched write refused until the age trigger.

A write is reserved before it is buffered, by an estimate (M141) that must now follow the
same rule, or a write exactly at the bound is refused and a reservation is left over while
its rows are pending. So the server's estimate encodes each operation as the engine stores
it -- `pstore_engine::delete_footprint`, `patch_footprint` and `by_filter_footprint` give a
tombstone's, a patch's and a by-filter operation's row -- and the server drops the spare
capacity a JSON parser left in parsed rows and patches (`Document::shrink_to_fit`) before it
estimates and buffers them: a parsed 2-component vector has a capacity of 4.

Unchanged: the two limits and their defaults (M141), the refusals, flush and fold behavior.
At the same 64 MiB, a tenant of tiny rows now buffers about 70,000 of them, not 3.7 million.

## Acceptance criteria
1. Measured in a test binary of its own, under a counting allocator, for 2-, 64-, 128-, 768-
   and 1,024-dimension rows, rows with 3 and with 20 attributes, string and number arrays,
   and sparse vectors, buffered as batched writes through the HTTP API: the charge is at
   least the heap the rows hold (allocated less deallocated) and at most 2.5 times it.
2. Flushed rows: after a flush, `unfolded` equals the flushed rows' former `pending` charge,
   and a fold returns it to 0.
3. The format's rule, exactly: a document's `footprint()` equals the sum the spec states,
   for a fixed document of each kind (dense, several vectors, sparse, attributes of every
   value type, nested arrays), computed in the test from `size_of` and capacities.
4. M141's bounds, flush and refusals, and its tests that use `footprint()` as their oracle,
   pass unedited. Tests that wrote the old rule as a literal change to the new rule, each
   named in VERIFIED: `footprint.rs`'s exact values (criterion 3 replaces them), the stamp's
   size, a cap written as 1,000 bytes, and a tombstone written as its id and 8 bytes.
6. Each operation's estimate is what buffering it charges: a delete, a patch, a delete by
   filter and a patch by filter, buffered alone, are charged exactly their
   `*_footprint`; and a write whose parsed rows are exactly the bound is admitted.
7. Review: with the fold policy's age and size never reached, a tenant whose flushed rows
   hold one row less than `tenant_flush_bytes` is not folded by a scheduled tick, and one
   at exactly the bound is, which returns its charge to 0.
5. `buffered_bench.rs` sums allocated less deallocated, and its release run is recorded,
   `provisional`.

## Test plan
`crates/pstore-server/tests/buffered_memory.rs` (one test, its own allocator) for 1; engine
`buffered.rs` beside M141's for 2; `pstore-format`'s `footprint.rs` for 3. Red before: 1
fails for every shape (a tiny row is charged 3% of its heap); 2 fails (unfolded is the
bundle's size); 3 fails (the old sums). Mutations, each killed:
- the slot not counted, or a map's node not counted (1, the tiny rows);
- `len` for `capacity` on vectors (1: a 768-dimension vector grown to 1,024);
- the per-allocation 16 bytes dropped (3);
- the flushed batch charged by its bundle again (2);
- a tombstone estimated as its id and 8 bytes, a patch by filter estimated with its `$op`,
  or parsed rows not shrunk (6).

## RA budget
Unchanged: memory arithmetic only.

## Risks
- `size_of` of std's B-tree nodes is not public; the bound is computed from the layout std
  has used since 1.0 (11 entries, a parent pointer, two `u16`s, 12 edges inside). If std
  changes it, criterion 1's measurement says so.
- The applied stamp's estimate is not a bound: a row of 10 or 11 attributes grows past one
  map leaf when stamped, about 1.4 KB more than estimated. The excess is charged as rows
  past the reservation, unchecked (M141's rule), so one large applied write can pass the cap
  by that much; it is then counted exactly and returns to 0.
- Heap is not RSS: allocator fragmentation is still not counted; the 16 bytes an allocation
  stand in for glibc's header and size classes, measured nowhere here.

## Tasks
- M213.1: the rule, the flushed charge, the measurement fix, tests, docs.
