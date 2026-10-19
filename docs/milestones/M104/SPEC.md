# M104 — the fleet harness runs again, and splits a fleet

**Serves:** [M31](../M31/VERIFIED.md)'s fleet run, `NOT-RUN` for want of Docker, and
[M4c](../M4c/VERIFIED.md) criterion 8, `NOT-RUN` because "splitting a real fleet needs network
control this environment does not have on host networking". No backlog row: both were found
by looking for blocked items that need neither a real store nor a device.

## What is true today

These facts come from running `scripts/cluster.sh` in a Linux cloud container with Docker
29.3.1, after M103.

- **A fleet of more than three nodes does not start.** [M53](../M53/SPEC.md) put the gossip
  key in `gossip_auth` and passes it as `-e "$gossip_auth"`, but left it out of the `export`
  that `xargs bash -c 'start_one {}'` depends on. Nodes 1 to 3 start in the script's own shell.
  Every later one gets `-e ""`, and `docker run` refuses it with its usage text. `up 20` gives
  `up: 3 nodes`, and `converge` then reports 3 of 3 as converged. No milestone since M53 has
  run the fleet, so nothing saw it.
- **`views` reads each node's last log line.** A node whose last line is `HEAL`, which carries
  no `members=`, is dropped from the worst view. Under a partition every node heals every 10 s,
  so `converge` can report a fleet converged while a node it did not read is still split.
- **Host networking can be split.** All nodes share the host's loopback, each on gossip port
  `7946 + i`. Two `iptables` DROP rules on `lo` between two port ranges split the fleet in two,
  with no change to the nodes. That needs root on the host namespace.

## Delta

1. `gossip_auth` is exported with the rest.
2. `views` reads each node's last `VIEW` or `VIEWCHANGE` line, not its last line, and prints
   one value per node: `0` for a node it could not read, so an unread node is never converged.
3. **`cluster.sh partition SECONDS`.** It splits the running fleet into nodes 1 to N/2 and the
   rest, by gossip port, UDP (gossip is UDP only). It holds the split for SECONDS and prints the worst and
   best view. It then lifts the split and times until every node sees N.
   - A trap removes the rules on any exit, so a failed run never leaves the host split.
   - ⚠️ **VACUOUS** when the DROP rules counted no packets: a split that never applied would
     otherwise read as one that healed at once. This is `starve`'s control, for a partition.
   - It says which path healed the split. If no node's view fell below N during the hold,
     gossip carried it alone. If one did, the split evicted, and only the roster heal can
     rejoin the halves.
   - It needs root, `iptables` and its `comment` match, and nodes numbered 1 to N with no gap;
     it says so and exits 2 without them. An unread node does not count as an eviction.
   - `PSTORE_SPLIT_PORT` (default 7946) moves the base the two ranges are counted from. It
     exists for criterion 4: aimed at ports no node uses, the rules drop nothing.

**Not changed:** the nodes, the protocol and every other subcommand. The image build in `up`
is unchanged; where `apt-get` cannot reach a mirror, the image is built from a host binary,
and the ledger says so.

## Acceptance criteria

1. `cluster.sh up 20` starts 20 nodes, and `converge` reports 20. Observed red first: 3.
2. **M31, on the fleet.** A whole, steady fleet logs no `HEAL` line: `main.rs` prints one only
   when a heal dials someone or publishes. So 3 of 20 nodes are killed, which leaves them in
   the roster for every survivor to dial. Then every survivor logs `HEAL` every `HEAL_PERIOD`
   (10 s, within 0.5 s). The view number (`VIEW t=`) before each survivor's heals, modulo
   `heal_every` (10), takes more than one value across the fleet: the slot, not the start
   time, spreads them. Every node's `OWNS` lines are 5 s apart and its `VIEW` lines 1 s apart,
   within 0.1 s.
3. **M4c criterion 8, on the fleet.** On 20 nodes:
   - `partition 90` prints a worst view of 10 during the hold, says the split evicted, and
     reconverges to 20 within `HEAL_PERIOD` plus 10 gossip periods of the lift;
   - `partition 1` says no node was evicted, and reconverges within the same bound. One
     second, because at 20 members the suspect timeout is 12 to 15 periods, 2.4 to 3 s.
4. `partition` reports VACUOUS and exits 1 when the rules drop nothing. Observed by pointing
   it at ports no node uses.
5. ⚠️ Every red in this list is taken against the script at `b55eefc`, M104's parent: the
   working tree had deltas 1 and 2 applied before the spec was reviewed.
6. Gates: `scripts/check-portable.sh` (the script stays `portable: no`) and the rest of
   `gates.sh`.

## Test plan

| # | Seen red | What it catches |
|---|---|---|
| 1 | `up 20` before the export: `up: 3 nodes` | a variable `start_one` reads that is not exported |
| 2 | M31's unit tests pin each count; this checks the wiring on real nodes | a cadence counted in polls, as before M31 (50 s) |
| 3 | the old `views` on a split fleet of 20: fewer than 20 values | a converge that skips nodes |
| 4 | the wrong port range | a split that never applied, read as a heal |

## RA budget

Unchanged. A split fleet reads the roster once per node per `HEAL_PERIOD`, as a whole one does.

## Risks

- **Every number is `provisional`**: 20 processes on one loopback, CPU-capped. The criteria
  are counted in heal periods and gossip periods, which survive the transport.
- **A rule left behind splits every later run.** The trap is the guard, and `down` does not
  touch `iptables`.

## Tasks

- **M104.1** — the export, `views`, `partition`, the fleet runs, the ledger.
