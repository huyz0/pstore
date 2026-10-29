#!/usr/bin/env python3
"""How many CI shards a PR's mutants need. Prints `shards=<k>` and the shard matrix.

Every shard builds the workspace before it tests a mutant, so a fixed matrix of twelve held
twelve runners for a diff of three mutants, or of none, while other PRs queued behind it. One
shard per `MUTANTS_PER_SHARD` mutants (default 8: about three minutes a mutant, two to a
runner, is about the build's own length again), at least one when there is any mutant, and
never more than 12, the matrix that held PR #8's hundreds under GitHub's six-hour job limit.
No mutant, no shard: the `mutants` job reads `shards=0` as passed.

  mutants-plan.py N          plan for N mutants (`scripts/mutants.sh --list | grep -c .`)
  mutants-plan.py --self-test
"""
import json, os, sys

MAX_SHARDS = 12


def shards(mutants: int, per: int) -> int:
    if mutants <= 0:
        return 0
    return min(MAX_SHARDS, -(-mutants // max(per, 1)))


def self_test() -> int:
    cases = [(0, 8, 0), (1, 8, 1), (8, 8, 1), (9, 8, 2), (84, 8, 11), (500, 8, 12), (3, 0, 3)]
    bad = [(n, p, want, shards(n, p)) for n, p, want in cases if shards(n, p) != want]
    for n, p, want, got in bad:
        print(f"FAIL {n} mutants at {p} a shard: expected {want}, got {got}")
    print("ok mutants-plan self-test" if not bad else f"{len(bad)} failing case(s)")
    return 1 if bad else 0


def main(argv: list) -> int:
    if argv[1:] == ["--self-test"]:
        return self_test()
    if len(argv) != 2:
        print(__doc__, file=sys.stderr)
        return 2
    k = shards(int(argv[1]), int(os.environ.get("MUTANTS_PER_SHARD", "8")))
    print(f"shards={k}")
    # Never empty: GitHub refuses an empty matrix even on a job its `if` skips.
    print(f"matrix={json.dumps(list(range(max(k, 1))))}")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
