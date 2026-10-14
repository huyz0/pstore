# M74 — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ **Where this ran:** a Linux x86-64 cloud container, in-memory store; bytes are provisional.

1. `a_set_round_trips_and_refuses_damage` (`cargo test -p pstore-index --test fde_clusters`): the set
   round-trips; a second field's vectors start at a nonzero offset inside the blob; an empty,
   truncated, trailing-byte, wrong-magic, count-above-the-bytes and duplicated-field blob are each
   refused; a one-cluster set is not worth writing.
2. `each_field_searches_its_own_vectors`: with two different corpora in one set, each field's
   search through its offset equals a single-field sidecar's for the same rows, for five queries.
3. `two_fields_in_one_segment_each_use_their_clusters` (`cargo test -p pstore-engine --test
   fde_skip`): a 9,000-document segment with two multi-vector fields has one sidecar naming both,
   and for each field one probe moves fewer `.fdev` bytes than every list.
4. **Gates:** green by the pre-commit hook on this commit. **Mutation** (`cargo mutants`,
   `--no-config`, changed lines of `fde_clusters.rs`): **46 mutants, 39 caught, 7 unviable,
   0 missed.**

**Not covered:** the in-memory fresh segment still scans flat. ⚠️ A `.fdec` in M71.2's
single-field form is read as damaged; none exists outside this branch.
