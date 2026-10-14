# M73 — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ **Where this ran:** a Linux x86-64 cloud container, in-memory store, the real pyarrow-written
2,600-row Parquet fixture from M70.

Command: `cargo test -p pstore-index --test external`: 10 tests.

1. `a_filter_prunes_before_the_window`: keeping every 5th row, a window over all rows returns the
   exact filtered cosine top 10 and every row satisfies the predicate; at a window of 20 it still
   returns 10 rows, all satisfying it.
2. `a_predicate_that_allows_nothing_reads_nothing`: no rows and 0 bytes of the customer's object.
3. `search_is_search_where_allowing_all`: identical results.
4. **Gates:** green by the pre-commit hook on this commit. **Mutation** (`cargo mutants`,
   `--no-config`, changed lines of `pstore-index/src/external.rs`): **15 mutants, 15 caught.**

**Not covered:** reading attribute columns from the file; a persisted delete vector.
