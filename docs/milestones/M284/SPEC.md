# M284: an open-loop write-throughput ladder

## Serves
D-36's write path and M128's admission, through lesson 8 of
[lessons-from-hsearch-opensearch.md](../../comparisons/lessons-from-hsearch-opensearch.md):
OpenSearch's closed-loop soak test never measured throughput, and an open-loop ladder found
the in-flight cap and a serial flusher.

## Delta
1. **`write_ladder::the_write_ladder`**, `#[ignore]`d, run in release by
   **`scripts/write-ladder.sh`** (outside `cargo test` for `depth.sh`'s reason). Writes arrive
   at a fixed rate whatever the server does; each one's latency counts from when it was due.
   A step holds at 95% acknowledged and a p99 within 5 s; the ladder stops at the first step
   that does not. Three shapes, all `durable`: one tenant, 64 tenants, 64 tenants in bulks of
   10. It prints offered, acknowledged, `429`s, p50, p99, PUTs and GETs a step, and docs/s a
   core at the knee. In process through the router, every store request taking 20 ms.
2. **`write_ladder::a_low_rate_holds_on_every_shape`**, in the suite: 50 writes a second for
   a second holds on each shape, every write acknowledged, so the harness itself is tested.
3. **The flush and its encode, read off the code**: `flush_inner` encodes the bundle under
   the `flushing` lock and then PUTs it, so the next bundle's encode waits for the previous
   PUT. Recorded, with what the ladder says about it, in the decisions doc.

Not changed: any write path. **No setting**: a measurement.

## Acceptance criteria
1. `scripts/write-ladder.sh` runs to the end, every shape holding at least one step; its
   table is in `VERIFIED.md`, `provisional`.
2. `a_low_rate_holds_on_every_shape` passes.
3. Lesson 8's decision, with the numbers, is in the decisions doc.
