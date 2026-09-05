GB = 1e9
print("=== Per-query peak: accumulate vs score-and-drop ===")
p, blk, seg, shard = 32, 3e6, 10, 4      # lists probed, block bytes, segments, shards/node
total = p*blk
print(f"  bytes fetched per shard          : {total/1e6:7.1f} MB")
print(f"  ACCUMULATE all, {shard} shards/node : {total*shard/1e6:7.1f} MB per query")
for inflight in (32, 16, 8, 4):
    print(f"  SCORE-AND-DROP, {inflight:>2} in flight     : {inflight*blk/1e6:7.1f} MB per query"
          f"   ({p/inflight:.0f} pipelined waves)")

print("\n=== Concurrency the query pool supports (40 GB pool) ===")
POOL = 40*GB
for mb in (400, 96, 48, 24, 12):
    print(f"  {mb:>3} MB/query -> {POOL/(mb*1e6):8.0f} concurrent queries")

print("\n=== Actual concurrency needed ===")
for qps, ms, label in ((200, 10, "warm"), (200, 400, "cold"), (50, 400, "cold, lower QPS")):
    c = qps*ms/1000
    print(f"  {label:<16} {qps} QPS x {ms} ms -> {c:6.1f} concurrent -> {c*24e6/GB:5.2f} GB at 24 MB/q")

print("\n=== 128 GB node budget ===")
rows = [("RAM cache (classes 1-5)",24),("Memtables (freshness)",8),("Query execution pool",40),
        ("Background: compaction/index build",12),("Write/bundle buffers",4),
        ("RPC/connection buffers",4),("Emergency reserve (flush path)",4)]
tot = sum(v for _,v in rows)
for k,v in rows: print(f"  {k:<36} {v:>4} GB")
print(f"  {'--- accounted':<36} {tot:>4} GB")
print(f"  {'allocator overhead/frag (~15%)':<36} {tot*0.15:>4.0f} GB")
print(f"  {'OS + page cache + headroom':<36} {120-tot-tot*0.15:>4.0f} GB")
print(f"  {'= memory.max':<36} {120:>4} GB   memory.high = {120*0.85:.0f} GB")

print("\n=== Per-open-index resident state at 4 KB each ===")
for n in (10_000, 100_000, 1_000_000):
    print(f"  {n:>9,} open indexes -> {n*4096/GB:6.2f} GB")
