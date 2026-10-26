//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::panic,
    reason = "assertions in tests are the reporting mechanism"
)]

//! M94: which legs run on another server in one exchange. A leg that scores each row by
//! itself does (dense, sparse, `multi`, `multi_fde`); one that needs statistics summed over
//! every segment (text), or does not run at all (trigram), does not.

use pstore_query::{Prefetch, splittable};

#[test]
fn only_legs_that_score_a_row_alone_split_in_one_exchange() {
    let legs = [
        (
            Prefetch::Dense {
                field: "vector".to_owned(),
                query: vec![1.0],
                limit: 1,
                tune: pstore_index::vec_index::Query::default(),
            },
            true,
        ),
        (
            Prefetch::Sparse {
                field: "s".to_owned(),
                query: vec![(1, 1.0)],
                limit: 1,
            },
            true,
        ),
        (
            Prefetch::Multi {
                field: "late".to_owned(),
                query: vec![vec![1.0]],
                limit: 1,
            },
            true,
        ),
        (
            Prefetch::MultiFde {
                field: "late".to_owned(),
                query: vec![vec![1.0]],
                limit: 1,
                rerank: None,
                probes: None,
            },
            true,
        ),
        (
            Prefetch::Text {
                field: "text".to_owned(),
                query: "a".to_owned(),
                limit: 1,
            },
            false,
        ),
        (
            Prefetch::Trigram {
                field: "text".to_owned(),
                pattern: "a".to_owned(),
                limit: 1,
            },
            false,
        ),
    ];
    for (leg, want) in &legs {
        assert_eq!(splittable(leg), *want, "{leg:?}");
    }
}
