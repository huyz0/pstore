S=2_592_000; XAZ=0.02; PUT=5e-6; GET=4e-7; GB=1e9
print("=== Cross-AZ probe mesh cost (the observability we deliberately removed) ===")
for probers, hz, sz in ((3,2,2048),(5,5,2048),(10,10,4096)):
    az=3; probes_s = probers*az*(az-1)*hz          # each prober probes each other AZ
    bps = probes_s*sz
    print(f"  {probers:>2} probers/AZ @ {hz:>2} Hz, {sz:>4} B -> {probes_s:>5.0f} probes/s,"
          f" {bps/1e3:7.1f} KB/s -> ${bps*S/GB*XAZ:8.2f}/month")

print("\n=== Blob-store health bulletin (partition-tolerant, no cross-AZ traffic) ===")
for period in (2,5,15):
    puts_s = 3/period                       # one PUT per AZ per period
    gets_s = 3*3/period                     # each AZ reads all 3 summaries
    print(f"  every {period:>2}s -> {puts_s:5.2f} PUT/s + {gets_s:5.2f} GET/s"
          f" -> ${puts_s*S*PUT + gets_s*S*GET:7.2f}/month")

print("\n=== Detection latency budget ===")
for name, period, conf in (("probe mesh",0.5,3),("blob bulletin",5,2),("passive outlier",10,2)):
    print(f"  {name:<16} sample {period:>4}s x {conf} confirmations -> {period*conf:5.1f}s to suspect")

print("\n=== What a gray AZ costs while undetected (3 AZs, 1/3 of traffic degraded) ===")
for qps, p50, degraded in ((10_000,10,500),(10_000,10,2000)):
    frac=1/3
    eff = p50*(1-frac) + degraded*frac
    print(f"  {qps:,} QPS, p50 {p50} ms, degraded AZ at {degraded} ms"
          f" -> effective mean {eff:6.1f} ms ({eff/p50:4.1f}x), {qps*frac:,.0f} QPS affected")
