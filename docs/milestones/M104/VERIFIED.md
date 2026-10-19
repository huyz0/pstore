# M104 — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ **Where this ran:** a Linux x86-64 cloud container, 4 cores, Docker 29.3.1 with
`rustfs/rustfs:1.0.0`. Every time below is `provisional`: 20 processes on one loopback.

⚠️ **The image was not built by `up`.** Here `apt-get` in `dev/Dockerfile.node` cannot reach a
Debian mirror, so the build fails. The image was built from a host `cargo build --release -p
pstore-node` on `ubuntu:24.04`, with the host's CA bundle, and `cluster.sh` was run from a copy
with only the build line removed. The nodes are the same code; the base image is not.

1. **20 nodes start.** `./scripts/cluster.sh up 20` → `up: 20 nodes`, and `converge` → 20 in
   7 periods. Red first, against `b55eefc`: `up: 3 nodes`, the rest refused by `docker run` with its
   usage text, and `converge` reported 3 of 3.
2. **M31 on the fleet.** `./scripts/cluster.sh up 20`, then `docker kill` of `pstore-n18` to
   `pstore-n20`, then 75 s of `docker logs -t` from the 17 survivors:
   - `HEAL`: 17 of 17 survivors, 103 gaps, median 10.07 s, from 10.06 to 10.10.
   - Slots: the `VIEW t=` before each heal, mod 10, took 8 values (1, 2, 3, 5, 6, 7, 8, 9).
   - `OWNS`: 238 gaps, from 5.03 to 5.07 s. `VIEW`: 1,255 gaps, from 1.00 to 1.04 s.
   - **OBSERVED-NOT** against a pre-M31 build. The 50 s cadence it would show is the arithmetic
     M31's unit tests already pin.
3. **M4c criterion 8 on the fleet.** On 20 nodes, with the split UDP only (gossip is UDP):
   - `./scripts/cluster.sh partition 90`: lowest view 10 during the hold, 2,212 packets
     dropped, `evicted: yes`, and `healed: every node sees 20 1610ms after the lift = 8 periods`. That is within 12 s.
   - `./scripts/cluster.sh partition 1`: 163 packets dropped, `evicted: no`, healed in 267 ms.
   - A first pair of runs, with TCP rules as well, gave the same shape: lowest 10, 2,102
     dropped, healed in 1,110 ms; and 164 dropped, no eviction, 281 ms.
   - An earlier run by hand, before `partition` existed, rejoined a 90 s split in 1.4 s, with
     `HEAL dialled=10` on each side for the whole hold. The `partition` runs did not correlate
     `HEAL` lines: that the roster heal rejoined them is inferred from `evicted: yes`.
   - **The old `views` on a split fleet** read 16 to 19 values for 20 nodes in six samples of
     the hand-run split, and 20 in one sample during `partition 90`. The red is intermittent:
     it needs a node whose last line is `HEAL` at the instant of the sample.
   - **Refusals.** With `pstore-n10` killed, `partition 1` → `needs running nodes numbered 1 to
     N with no gap`, exit 2, before any rule. **The trap:** `partition 60` interrupted by
     SIGINT 5 s into the hold had 2 rules in place, exited 130, and left 0.
4. **VACUOUS.** `./scripts/cluster.sh partition 3` with `PSTORE_SPLIT_PORT=20000` →
   `0 packets dropped`, `partition: VACUOUS`, exit 1. `iptables -L INPUT` then listed no `pstore-split` rule.
5. Every red above was taken by `./scripts/cluster.sh` as it stood at `b55eefc`.
6. **Gates.** `./scripts/gates.sh`, by the pre-commit hook on this commit.
