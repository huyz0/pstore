# M85 — The multi-vector lists' coalescing, over an S3 API

**Serves:** backlog row 78. [M71](../M71/SPEC.md) measured bytes in memory at a 64 KB gap; the
product's stores merge ranges closer than 1 MB (`coalesce_gap`). This records what that does to
four of twelve lists, through the real `ObjectStoreBackend` and its S3 client.

⚠️ **Provisional, and by design not a real-store number** (non-negotiable 7). The store is moto, an
S3-API emulator (MinIO's download is blocked from this container). Requests and bytes are
properties of the planner and the layout and are exact here; latency, throughput and S3's own
behaviour are not measured and are not claimed. Row 78 stays open for a real bucket.

## What is measured

1,500 encoded rows of 2,048 bytes (512 floats) in 12 topics, clustered into 12 lists by
`FdeClusters` (`target_list_size` 125); the `.fdev` object (3,072,000 bytes) put in a bucket; the
four best lists for a query read through `ObjectStoreBackend` twice, once with
`coalesce_gap` = 64 KiB and once 1 MiB. The server's access log counts the GETs it saw.

## Acceptance criteria

1. The example prints `lists 12`, `object 3072000 bytes`, and the four lists ask for 1,024,000
   bytes in 4 ranges.
2. The planner, which is the store's own, plans 3 fetches / 1,024,000 bytes at a 64 KiB gap and
   2 fetches / 1,536,000 bytes at a 1 MiB gap.
3. The server saw exactly those numbers of GETs (3 and 2) for the two reads, and both reads return
   the same top 10.
4. `the_probe_order_is_the_order_the_search_reads_in` (`pstore-index`): the new accessors the
   example uses (`probe_order`, `list_len`) return every list once, best first, and zero for a
   list that does not exist.
5. **Gates:** `./scripts/gates.sh` passes; the mutation sweep of the changed library lines misses 0.

**Not covered:** a real bucket; latency; concurrency; the lists of a segment with replicas.
