#!/usr/bin/env python3
"""Turn a running fleet's container logs into M4b's numbers.

⚠️ Timing comes from `VIEWCHANGE` lines, not from polling. A harness that polls every second
resolves to five gossip periods, and criterion 2's bound is eight of them — such an
instrument cannot tell a pass from a failure, whatever number it prints.

⚠️ Convergence is measured from the LAST node's join, not from the first. The harness starts
nodes staggered, and a node cannot list a peer that does not yet exist; measuring from the
first join reports the stagger.
"""

import concurrent.futures
import datetime
import pathlib
import re
import subprocess
import sys

STAMP = re.compile(r"^(\S+)\s")


def logs(container: str) -> list[tuple[datetime.datetime, str]]:
    out = subprocess.run(
        ["docker", "logs", "-t", container],
        capture_output=True, text=True, check=False,
    )
    rows = []
    for line in (out.stdout + out.stderr).splitlines():
        m = STAMP.match(line)
        if not m:
            continue
        try:
            when = datetime.datetime.fromisoformat(m.group(1).replace("Z", "+00:00"))
        except ValueError:
            continue
        rows.append((when, line[m.end():]))
    return rows


def containers() -> list[str]:
    out = subprocess.run(
        ["docker", "ps", "--format", "{{.Names}}", "--filter", "name=pstore-n"],
        capture_output=True, text=True, check=True,
    )
    return [c for c in out.stdout.split() if c != "pstore-rustfs"]


def detect(killed_at: str, survivors: int, period_ms: int) -> int:
    """Time from a kill to every survivor listing the smaller fleet."""
    t0 = datetime.datetime.fromisoformat(killed_at.replace("Z", "+00:00"))
    seen = []
    for c in containers():
        row = next(
            (
                t
                for t, l in logs(c)
                if l.startswith("VIEWCHANGE") and f"members={survivors} " in l and t >= t0
            ),
            None,
        )
        if row:
            seen.append(row)
    if len(seen) < survivors:
        print(f"NOT detected: {survivors - len(seen)} survivors never dropped to {survivors}")
        return 1
    worst = (max(seen) - t0).total_seconds()
    median = (sorted(seen)[len(seen) // 2] - t0).total_seconds()
    print(
        f"DETECTION, kill -> every survivor lists {survivors}: "
        f"{worst:.2f}s = {worst * 1000 / period_ms:.0f} periods of {period_ms}ms "
        f"(median {median:.2f}s) (provisional)"
    )
    return 0


def default_period() -> int:
    """The period `cluster.sh up` actually used.

    ⚠️ Read, not assumed. Above 100 nodes the harness scales the gossip period with the
    fleet, and a reporter that assumed 200ms would silently divide by the wrong number and
    report periods nobody used.
    """
    try:
        return int(pathlib.Path("/tmp/pstore-cluster-period").read_text(encoding="utf-8").strip())
    except (OSError, ValueError):
        return 200


def main() -> int:
    if len(sys.argv) > 1 and sys.argv[1] == "--detect":
        return detect(sys.argv[2], int(sys.argv[3]), int(sys.argv[4]))
    period_ms = int(sys.argv[1]) if len(sys.argv) > 1 else default_period()
    names = containers()
    joins, fulls, traffic = [], [], []
    n = len(names)
    with concurrent.futures.ThreadPoolExecutor(max_workers=32) as pool:
        all_rows = dict(zip(names, pool.map(logs, names), strict=True))
    for c in names:
        rows = all_rows[c]
        join = next((t for t, l in rows if l.startswith("JOIN")), None)
        # The first moment this node's view covered the whole fleet.
        full = next(
            (t for t, l in rows if l.startswith("VIEWCHANGE") and f"members={n} " in l),
            None,
        )
        if join:
            joins.append(join)
        if full:
            fulls.append(full)
        last = [l for _, l in rows if l.startswith("OWNS")]
        if last:
            m = re.search(r"sent=(\d+) recvd=(\d+) dropped=(\d+)", last[-1])
            if m:
                traffic.append(tuple(int(g) for g in m.groups()))

    print(f"nodes: {n}, joined: {len(joins)}, reached a full view: {len(fulls)}")
    if len(fulls) < n:
        print(f"NOT CONVERGED: {n - len(fulls)} nodes never listed all {n}")
        return 1

    stagger = (max(joins) - min(joins)).total_seconds()
    conv = (max(fulls) - max(joins)).total_seconds()
    print(f"harness stagger (first join -> last join): {stagger:.1f}s")
    print(
        f"CONVERGENCE, last join -> every node lists {n}: "
        f"{conv:.2f}s = {conv * 1000 / period_ms:.0f} periods of {period_ms}ms (provisional)"
    )
    if traffic:
        s = sum(t[0] for t in traffic)
        r = sum(t[1] for t in traffic)
        d = sum(t[2] for t in traffic)
        # Bytes since each node started, so the denominator is that node's own uptime.
        span = max((max(fulls) - min(joins)).total_seconds(), 1.0)
        # ⚠️ Cumulative since each node started, so this window INCLUDES the cold-start
        # state exchange and the staggered join — it is not the steady-state rate, and it
        # will not match `cluster.sh traffic`, which samples a window on a settled fleet.
        # Criterion 5 uses the latter; this line is here to show the loss injection is live.
        print(
            f"gossip traffic since start (NOT steady state; use `cluster.sh traffic` for "
            f"that): {(s + r) / span / n:.0f} bytes/s/node "
            f"({s} sent, {r} recvd, {d} datagrams dropped, over {span:.0f}s, n={len(traffic)})"
        )
    return 0


if __name__ == "__main__":
    sys.exit(main())
