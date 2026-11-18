# M283: lucene-rust's IVF tuning findings, measured on pstore's index

## Serves
D-8 (balanced clustering) and D-34 (the round-trip budget), through lesson 7 of
[lessons-from-hsearch-opensearch.md](../../comparisons/lessons-from-hsearch-opensearch.md),
which asks for a pstore measurement of each finding before any becomes a setting.

## Delta
1. **`crates/pstore-index/examples/ivf_tuning.rs`**, outside `cargo test` for `recall.sh`'s
   reason, at the recall gate's scale (20,000 x 384d, lists of 200, no replicas as the engine
   builds). It prints:
   - centroid ranking against code scoring, per query, at 100 and 6,437 centroids and lists
     of 200 and 4,000 rows;
   - candidate recall@10 at p = 2, 4, 8, candidates read, the largest list and the vectors
     moved, with and without an unconstrained final Lloyd round, on a clustered and a
     uniform corpus;
   - for filters of 10 to 5,000 admitted rows, random and vector-correlated, how many lists
     hold an admitted row, and the bytes of reading the admitted rows' vectors against the
     bytes a filtered dense leg reads today.
2. **A decision per finding** in `docs/comparisons/lessons-decisions-2026-11-17.md`.

Not changed: any index, query or build path. **No setting**: nothing is built.

## Acceptance criteria
1. `cargo run --release -p pstore-index --example ivf_tuning` runs to the end and prints the
   three tables; its numbers are in `VERIFIED.md`, marked `provisional`.
2. Each of the four findings has a decision with its numbers in the decisions doc.
3. `cargo clippy -p pstore-index --all-targets -- -D warnings` passes.
