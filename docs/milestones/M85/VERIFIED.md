# M85 — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ **Where this ran, and what it is not:** a Linux x86-64 cloud container against **moto 5**, an
S3-API *emulator* (MinIO's download is blocked here), over loopback HTTP. **Provisional**: request
and byte counts are exact properties of the planner and the layout; **no latency, throughput or
real-S3 behaviour is claimed**, and backlog row 78 stays open for a real bucket.

Reproduce (any S3-compatible endpoint with a bucket `pstore`; the access log is the server's):

```
moto_server -p 5555 > moto.log &                       # create the bucket `pstore` once
RUN=r1 cargo run -p pstore-blob --features compat --example coalesce_s3 -- put
RUN=r1 cargo run -p pstore-blob --features compat --example coalesce_s3 -- plan
RUN=r1 cargo run -p pstore-blob --features compat --example coalesce_s3 -- read 65536
RUN=r1 cargo run -p pstore-blob --features compat --example coalesce_s3 -- read 1048576
grep -c "GET /pstore/m85/r1" moto.log                  # before and after each read
```

1. `cargo run -p pstore-blob --features compat --example coalesce_s3 -- plan`, on two runs (`r1`, `r2`): `lists 12  object 3072000 bytes  rows 1500`, and
   `asked 1024000 bytes in 4 ranges`.
2. `cargo run -p pstore-blob --features compat --example coalesce_s3 -- plan` prints, on both runs: `plan gap 65536: 3 fetches, 1024000 bytes` and
   `plan gap 1048576: 2 fetches, 1536000 bytes`.
3. `cargo run -p pstore-blob --features compat --example coalesce_s3 -- read 65536` and `-- read 1048576`: the server's GET count rose by 3 for the 64 KiB read and by 2 for the 1 MiB read, on both runs;
   both reads returned the same top ids `[3, 891, 543, 1395, 759, 27, 1419, 159, 903, 1191]`.
   **What it says:** four of twelve lists are 1.0 MB of a 3.07 MB object (a third); at the
   product's 1 MiB gap they cost two requests and 1.5 MB (+50% bytes, -1 request against 64 KiB's
   three). The sidecar's saving over reading the object is 2-3x in bytes at this layout.
4. `the_probe_order_is_the_order_the_search_reads_in`
   (`cargo test -p pstore-index --test fde_clusters`): 9 tests in the file, this one included.
5. **Gates:** `./scripts/gates.sh`, green by the pre-commit hook on this commit; `cargo clippy -p
   pstore-blob --features compat --all-targets -- -D warnings` clean; `cargo deny check licenses
   bans sources` passes with the new dev-dependency edge. **Mutation** (`cargo mutants`,
   `--no-config`, changed lines of `fde_clusters.rs`): **14 mutants, 14 caught, 0 missed.**
   ⚠️ `cargo deny check advisories` still fails on `paste` (via `foyer`), as before.

**Not covered:** a real bucket (latency, throughput, S3's behaviour); concurrency; the lists of a
segment with replicas.
