# M283 verification

The spec was not sent to a separate reviewer before code; the PR's review covers it.

1. PASS. `cargo run --release -p pstore-index --example ivf_tuning`, in a cloud container,
   about 70 s. ⚠️ `provisional`: synthetic corpora, relative numbers only.

   Centroid ranking against code scoring, 384d, p = 8, per query:

   | centroids | list rows | rank us | score us | ranking share |
   |---|---|---|---|---|
   | 100 | 200 | 20 | 416 | 4.7% |
   | 100 | 4,000 | 20 | 8,495 | 0.2% |
   | 6,437 | 200 | 1,387 | 435 | 76% |
   | 6,437 | 4,000 | 1,383 | 8,379 | 14% |

   A final unconstrained Lloyd round, 20,000 x 384d, lists of 200:

   | corpus | build | p=2 | p=4 | p=8 | candidates p=8 | largest list | moved |
   |---|---|---|---|---|---|---|---|
   | clustered, bounded | 5.2 s | 0.902 | 0.987 | 1.000 | 1,657 | 500 | |
   | clustered, + Lloyd | 6.8 s | 0.902 | 0.987 | 1.000 | 1,657 | 500 | 0.0% |
   | uniform, bounded | 5.3 s | 0.054 | 0.115 | 0.205 | 1,613 | 231 | |
   | uniform, + Lloyd | 6.8 s | 0.055 | 0.113 | 0.200 | 1,612 | 230 | 0.7% |

   (Candidate recall: the share of the true top 10 among the probed lists' rows. Uniform
   384d data has no neighbourhood structure for any index to find; it is the control.)

   Filtered probing, 20,000 rows in 100 lists. Today a filtered dense leg reads every
   list's codes and int8 records, 9.28 MB a segment:

   | filter | admitted | lists holding one (clustered / uniform) | admitted vectors |
   |---|---|---|---|
   | random | 6 | 6 / 6 | 0.009 MB |
   | random | 92 | 47 / 57 | 0.141 MB |
   | random | 283 | 79 / 94 | 0.435 MB |
   | random | 984 | 94 / 100 | 1.51 MB |
   | random | 4,992 | 100 / 100 | 7.67 MB |
   | one mixture component | 500 | 2 / n.a. | 0.768 MB |
2. PASS. Lesson 7's section of `docs/comparisons/lessons-decisions-2026-11-17.md`, each decision citing the table above it comes from: `grep -c "not built" docs/comparisons/lessons-decisions-2026-11-17.md`.
3. PASS. `cargo clippy -p pstore-index --all-targets -- -D warnings`.
