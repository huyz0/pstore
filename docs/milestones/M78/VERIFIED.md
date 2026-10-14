# M78 — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ **Where this ran:** a Linux x86-64 cloud container, in-memory store, three in-process peers.

1. `a_split_query_equals_the_unsplit_one` (`cargo test -p pstore-engine --test split`): the two new
   named-field cases (plain and filtered) answer identically split and unsplit; every case asserts
   the query really was split (two exchanges).
2. `a_split_named_dense_query_ranks_that_field`: the split top 10 over 100 documents equals the
   exact dot-product ranking of `v2`. Seen red first: with M77's branch disabled the ids were
   `d00010, d00012, d00035, ...` against the wanted `d00061, d00060, d00052, ...`; restored, green.
3. **Gates:** `./scripts/gates.sh`, green by the pre-commit hook on this commit. This milestone changes tests and docs
   only: `git diff` touches no `src/` file, so there is nothing to mutate.

**Not covered:** an approximate (clustered) index per named field.
