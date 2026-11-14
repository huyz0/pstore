//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! A target's `shadowed` flag decides whether the query's shadow hides its rows (M233).
//!
//! ⚠️ CI's mutation review of #58: the late visit's `t.shadowed && !shadow.is_empty()` as `||`
//! survived. No engine path pairs an unshadowed target with a non-empty shadow today, so only
//! the visit's own contract, held here, tells the two apart.

use pstore_blob::{BlobStore, Key, MemoryStore};
use pstore_format::{Document, SegmentWriter, Value};
use pstore_query::{OrderBy, Selector, Target, select_held};
use std::collections::HashSet;

#[tokio::test]
async fn the_shadow_hides_rows_only_in_a_shadowed_target() {
    let store = MemoryStore::new();
    let key = Key::new("0000/seg/shadow");
    let mut w = SegmentWriter::new(8);
    for i in 0..4 {
        let mut d = Document::new(format!("d{i}"), vec![1.0]);
        d.attrs.insert("p".to_owned(), Value::Int(i));
        w.push(d);
    }
    store.put(&key, w.finish()).await.unwrap();
    let shadow: HashSet<String> = HashSet::from(["d1".to_owned()]);
    for shadowed in [false, true] {
        let target = Target {
            segment_len: None,
            centroids: None,
            deleted: None,
            sparse_dict: false,
            text_dict: false,
            fde_clusters: false,
            shadowed,
            segment: key.clone(),
        };
        let mut s = Selector::new(
            OrderBy {
                attr: "p".to_owned(),
                desc: false,
            },
            10,
        );
        select_held(&store, &[target], None, &shadow, &mut s)
            .await
            .unwrap();
        let ids: Vec<String> = s.into_sorted().into_iter().map(|d| d.id).collect();
        let want: &[&str] = if shadowed {
            &["d0", "d2", "d3"]
        } else {
            &["d0", "d1", "d2", "d3"]
        };
        assert_eq!(ids, want, "shadowed: {shadowed}");
    }
}
