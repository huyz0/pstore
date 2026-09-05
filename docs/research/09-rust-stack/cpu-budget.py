GB=1e9; TB=1e12
print("=== A. Bytes stored on S3 per 768-d vector (with ~500B doc, ~500B attrs) ===")
tiers=[("RaBitQ 1-bit codes (+factors)",104),("SPANN boundary duplication (~1x)",104),
       ("int8 rerank tier",768),("f16 exact-rerank tier",1536),
       ("attributes, ~3x compressed",170),("FTS/sparse postings",100),("stored doc fields",500)]
S3=sum(v for _,v in tiers)
for k,v in tiers: print(f"  {k:<34} {v:>6} B")
print(f"  {'--- total on S3':<34} {S3:>6} B")
CACHE_PER_VEC=104
print(f"  cached (scan tier only)            {CACHE_PER_VEC:>6} B")
print(f"  >>> representation ratio           {S3/CACHE_PER_VEC:>6.1f}x")

print("\n=== B. Effective ratio R = 31.6 / f   (f = hot fraction of vectors) ===")
for f in (1.0,0.3,0.1,0.03,0.01):
    print(f"  f={f:<5} -> R = {S3/CACHE_PER_VEC/f:8.0f}:1")

print("\n=== C. Data managed per node (2.4 TB NVMe cache) ===")
for R in (32,100,316,1000):
    print(f"  R={R:>5}:1 -> {2.4*TB*R/1e15:7.2f} PB of S3 data per node")

print("\n=== D. Is capacity or QPS the binding constraint? (1 PB dataset) ===")
for R in (32,100,316):
    print(f"  R={R:>4}: {1e15/(2.4*TB*R):6.2f} nodes for CAPACITY", end="")
    for qps in (200,1000):
        print(f" | {1e6/qps if False else '':0}", end="")
    print()
for qps_node in (200,500,1000):
    for target in (10_000, 100_000):
        print(f"  {target:>7,} QPS at {qps_node:>4} QPS/node -> {target/qps_node:7.0f} nodes for THROUGHPUT")

print("\n=== E. Scan CPU: 1-bit Hamming is memory-bandwidth bound ===")
BPV=96
for nvec,label in ((128_000,"32 lists x 4k"),(1_000_000,"1% of 100M"),(10_000_000,"1% of 1B")):
    b=nvec*BPV
    for bw,src in ((10e9,"1 core from RAM"),(60e9,"16 cores from RAM"),(6e9,"from NVMe")):
        print(f"  {label:<14} {nvec:>10,} vec = {b/1e6:7.1f} MB | {src:<18} {b/bw*1000:7.2f} ms")
    print()

print("=== F. Achievable QPS per node, scan-only, 16 cores @ ~60 GB/s effective ===")
for nvec in (128_000, 1_000_000, 10_000_000):
    ms = nvec*BPV/60e9*1000
    print(f"  {nvec:>10,} vec/query -> {ms:7.2f} ms -> {1000/ms:9.0f} QPS scan-bound"
          f" -> ~{1000/ms/4:8.0f} QPS with 4x overhead")
