S = 2_592_000          # seconds per 30-day month
PUT = 5e-6             # $ per S3 Standard PUT
B   = 8*1024*1024      # bundle target bytes

def money(x): return f"${x:,.0f}" if x >= 1 else f"${x:,.4f}"

print("=== A) per-index flush at T=1s, by active-indexes-per-second ===")
for A in (100_000, 300_000, 1_000_000, 5_000_000):
    print(f"  A={A:>9,}  PUTs/mo={A*S:.3e}  {money(A*S*PUT)}")

print("\n=== B) node/cohort bundling: time-driven floor = W/T ===")
for W in (10_000, 1_000, 600, 256):
    for T in (1, 5):
        p = W*S/T
        print(f"  W={W:>6,} T={T}s  PUTs/mo={p:.3e}  {money(p*PUT)}")

print("\n=== C) data-driven term = bytes_per_s / 8MiB ===")
for gbs in (0.1, 1.0, 10.0):
    p = gbs*1e9/B*S
    print(f"  {gbs:>5} GB/s   PUTs/mo={p:.3e}  {money(p*PUT)}")

print("\n=== D) W that balances the two terms:  W* = bytes_per_s * T / B ===")
for gbs in (0.1, 1.0, 10.0):
    for T in (1,5):
        print(f"  {gbs:>5} GB/s T={T}s -> W*={gbs*1e9*T/B:8.0f}")

print("\n=== E) fold cost: CAS unit = index (50M) vs tenant (1M), hourly ===")
for label, n in (("per-index (50M)", 50_000_000), ("per-tenant (1M)", 1_000_000)):
    for puts, how in ((1,"inline, 1 PUT"), (3,"segment+manifest+CAS")):
        p = n*720*puts
        print(f"  {label:<17} {how:<22} PUTs/mo={p:.3e}  {money(p*PUT)}")

print("\n=== F) memtable memory: 1 GB/s fleet writes, R=3, buffered for T ===")
for T in (5, 60):
    tot = 1e9*T*3
    print(f"  T={T:>2}s -> {tot/1e9:6.1f} GB fleet, {tot/10_000/1e6:6.2f} MB per node (10k nodes)")
