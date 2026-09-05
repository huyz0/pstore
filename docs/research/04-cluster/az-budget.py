S=2_592_000; XAZ=0.02   # $/GB round trip ($0.01 each direction); NAT=0.045/GB
GB=1e9
def mo(gbs, rate=XAZ): return gbs*S*rate

print("=== 1. Epoch propagation: targeted notify vs full gossip flood ===")
commits_s = 278 + 10_000          # 1M tenants hourly + 10k hot tenants at 1/s
msg = 100                          # bytes per notification
for label, fanout in (("targeted -> R=3 placements", 3), ("full flood -> 10,000 nodes", 10_000)):
    bps = commits_s*fanout*msg
    print(f"  {label:<30} {bps/1e6:9.2f} MB/s fleet"
          f"  | if 2/3 crosses AZ: ${mo(bps*2/3/GB):>12,.0f}/mo")

print("\n=== 2. Cross-AZ cost of replication and fan-out ===")
print("  memtable replication, R=3:")
for gbs in (0.1, 1.0, 10.0):
    print(f"    {gbs:>5} GB/s writes | AZ-spread (2 extra copies): ${mo(gbs*2):>12,.0f}/mo"
          f" | AZ-local: $0")
print("  query fan-out intermediate results (100 hits x 200 B x 16 shards = 320 KB/query):")
for qps in (1_000, 10_000, 100_000):
    gbs = qps*320e3/GB
    print(f"    {qps:>7,} QPS -> {gbs:6.2f} GB/s | cross-AZ: ${mo(gbs):>12,.0f}/mo | AZ-local: $0")

print("\n=== 3. The NAT-gateway landmine (S3 traffic misrouted) ===")
for gbs in (1.0, 10.0):
    print(f"  {gbs:>5} GB/s of blob reads | via NAT @$0.045/GB: ${gbs*S*0.045:>12,.0f}/mo"
          f" | via S3 gateway endpoint: $0")

print("\n=== 4. Cost of per-AZ cache duplication (3 AZs, R=316:1) ===")
for pb in (0.1, 1.0, 10.0):
    hot = pb*1e15/316
    print(f"  {pb:>5} PB data -> hot set {hot/1e12:6.2f} TB -> x3 AZs = {hot*3/1e12:7.2f} TB"
          f" = {hot*3/2.4e12:6.1f} nodes' cache (vs 100-500 nodes needed for QPS)")

print("\n=== 5. Static stability: headroom to survive losing one of N AZs ===")
for n in (2,3,4):
    print(f"  {n} AZs -> run each at <= {(n-1)/n:5.1%} utilisation; "
          f"survivors then carry {n/(n-1):4.2f}x their normal load")
