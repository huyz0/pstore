print("=== A. Quantized code size vs SIMD register width (AVX-512 ZMM = 64 B) ===")
for dims in (512, 768, 1024, 1536, 3072):
    b = dims//8
    print(f"  {dims:>4} dims -> {b:>4} B/vector = {b/64:5.2f} ZMM", end="")
    for batch in (1,2,4):
        tot=b*batch
        exact = "EXACT" if tot % 64 == 0 else "     "
        if tot % 64 == 0:
            print(f" | batch {batch}: {tot:>4} B = {tot//64} ZMM {exact}", end="")
    print()

print("\n=== B. Roofline: is the scan compute- or memory-bound? (768d, 96 B/vec) ===")
B=96
compute_cyc = 4            # ~4 cycles/vector: xor+vpopcntq+add over 12 u64, batched
ghz = 3.0
print(f"  compute: {ghz*1e9/compute_cyc/1e6:8.1f} M vectors/s per core")
for bw,label in ((10e9,"per core"),(60e9,"per node, 16 cores")):
    print(f"  memory : {bw/B/1e6:8.1f} M vectors/s {label}  ({bw/1e9:.0f} GB/s)")
print(f"  -> per core, memory-bound by {(ghz*1e9/compute_cyc)/(10e9/B):5.1f}x")
print("  => once memory-bound, WIN = fewer bytes, not faster instructions.")

print("\n=== C. TLB pressure on a 96 MB scan ===")
for page,name in ((4096,"4 KiB"),(2*1024*1024,"2 MiB huge")):
    n=96e6/page
    print(f"  {name:<10} -> {n:9,.0f} page entries touched"
          f"  (L2 TLB is ~1,536-2,048 entries)"
          f" {'THRASH' if n>2048 else 'fits'}")

print("\n=== D. Bytes per vector by quantization (why B matters more than the kernel) ===")
for name,b in (("f32",3072),("f16",1536),("int8",768),("RaBitQ 1-bit",96)):
    print(f"  {name:<14} {b:>5} B -> {60e9/b/1e6:8.1f} M vectors/s at 60 GB/s")
