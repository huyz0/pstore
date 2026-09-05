DEV_TB = 3.75                      # NVMe instance store per node
DEV = DEV_TB * 1e12

print("=== Device budget (planning assumption: 2 DWPD, DLWA 1.3 at ~65% util) ===")
for dwpd in (1, 2, 3):
    dev_writes = DEV * dwpd                       # bytes/day the device may absorb
    for dlwa in (1.3, 2.0, 3.5):
        host = dev_writes / dlwa
        print(f"  DWPD={dwpd}  DLWA={dlwa:<4} -> host writes {host/1e12:5.2f} TB/day"
              f" = {host/86400/1e6:6.1f} MB/s sustained")

print("\n=== Capacity split for a 3.75 TB device ===")
res, cap = 100e9, 0.64*DEV
print(f"  hard reserve (OS/logs/scratch) {res/1e9:6.0f} GB  ({res/DEV:5.1%})")
print(f"  cache max                      {cap/1e12:6.2f} TB  ({cap/DEV:5.1%})")
print(f"  unallocated over-provisioning  {(DEV-res-cap)/1e12:6.2f} TB  ({(DEV-res-cap)/DEV:5.1%})")

print("\n=== Time to warm 2.4 TB at the endurance budget ===")
for mbs in (67, 150, 500):
    print(f"  {mbs:>3} MB/s -> {cap/(mbs*1e6)/3600:6.2f} h to fill cache")

print("\n=== Warming only classes 1-4 (metadata/centroids), 0.1%-1% of bytes ===")
for frac in (0.001, 0.01):
    b = cap*frac
    print(f"  {frac:.1%} of cache = {b/1e9:6.2f} GB -> {b/(67e6):7.1f} s at 67 MB/s")

print("\n=== Vectors held: 1-bit RaBitQ @768d = 96 B/vector ===")
print(f"  2.4 TB cache -> {cap/96/1e9:.1f} billion vectors of scan tier per node")
