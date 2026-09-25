//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! Resolving a hit to a document id — M7c.
//!
//! ⚠️ **Why this had to exist before a server could.** `Engine::query` answers with
//! `(segment, row)` pairs and a score; the only public way to turn a row ordinal into an id
//! was `scan`, which reads every block the filter does not prune. An API that resolved its
//! results that way would read a whole segment per query — a request that scales with
//! **documents**, which `AGENTS.md` lists under Never.

use pstore_blob::{Accounted, BlobStore, Key, MemoryStore, OpClass};
use pstore_format::{Document, Segment, SegmentWriter};
use pstore_types::TenantId;
use std::sync::Arc;

const BILL: TenantId = TenantId(0);

async fn seeded(rows: usize, rows_per_block: usize) -> (Arc<Accounted<MemoryStore>>, Key) {
    let acct = Arc::new(Accounted::new(MemoryStore::new()));
    let store = acct.as_tenant(BILL);
    let mut w = SegmentWriter::new(rows_per_block);
    for i in 0..rows {
        w.push(Document::new(format!("d{i:04}"), vec![i as f32, 1.0, 0.5]));
    }
    let key = Key::new("0000/seg/1");
    store.put(&key, w.finish()).await.unwrap();
    (acct, key)
}

#[tokio::test]
async fn resolving_a_row_reads_only_the_block_that_holds_it() {
    // 20 blocks of 10 rows. One hit must not cost twenty blocks.
    let (acct, key) = seeded(200, 10).await;
    let store = acct.as_tenant(BILL);
    let seg = Segment::open(&store, &key).await.unwrap();
    assert_eq!(seg.block_count(), 20);

    let before = acct.bytes(BILL, OpClass::Read);
    let ids = seg.ids_at(&store, &key, &[37]).await.unwrap();
    let one_row = acct.bytes(BILL, OpClass::Read) - before;
    assert_eq!(ids, vec![Some("d0037".to_owned())]);

    let before = acct.bytes(BILL, OpClass::Read);
    let all = seg.scan(&store, &key, None).await.unwrap();
    let whole_segment = acct.bytes(BILL, OpClass::Read) - before;
    assert_eq!(all.len(), 200);

    // ⚠️ The **relationship**, not a byte count: resolving one hit reads a block, scanning
    // reads every block and every vector. A resolver that fell back to `scan` would make
    // these equal, which is the defect this exists to prevent.
    assert!(
        one_row * 10 < whole_segment,
        "resolving one row read {one_row} bytes against a full scan's {whole_segment}"
    );
    assert_eq!(acct.count(BILL, OpClass::List), 0);
}

#[tokio::test]
async fn hits_in_several_blocks_cost_one_round_and_out_of_range_rows_are_none() {
    let (acct, key) = seeded(200, 10).await;
    let store = acct.as_tenant(BILL);
    let seg = Segment::open(&store, &key).await.unwrap();

    let before = acct.count(BILL, OpClass::Read);
    // Rows in four different blocks, plus one past the end.
    let ids = seg
        .ids_at(&store, &key, &[5, 55, 199, 100, 999])
        .await
        .unwrap();
    assert_eq!(
        acct.count(BILL, OpClass::Read) - before,
        1,
        "the blocks holding several hits must be fetched in ONE coalesced round"
    );
    assert_eq!(
        ids,
        vec![
            Some("d0005".to_owned()),
            Some("d0055".to_owned()),
            Some("d0199".to_owned()),
            Some("d0100".to_owned()),
            // ⚠️ A row the segment does not have is `None`, not an error and not another
            // row's id: a wrong id here is a wrong search result, silently.
            None,
        ]
    );
}

#[tokio::test]
async fn resolving_nothing_costs_nothing() {
    let (acct, key) = seeded(50, 10).await;
    let store = acct.as_tenant(BILL);
    let seg = Segment::open(&store, &key).await.unwrap();
    let before = acct.count(BILL, OpClass::Read);
    assert!(seg.ids_at(&store, &key, &[]).await.unwrap().is_empty());
    assert_eq!(
        acct.count(BILL, OpClass::Read) - before,
        0,
        "an empty hit list issued a request"
    );
}

/// A row as `rows_at` answers it: its id and its attributes, in name order.
type Row = (String, Vec<(String, pstore_format::Value)>);

#[tokio::test]
async fn rows_at_returns_each_rows_id_and_attributes_in_one_round() {
    // M9a. The block that resolves a row's id carries its attributes; returning them must
    // not cost a second fetch, and each row must get its OWN attributes.
    let acct = Arc::new(Accounted::new(MemoryStore::new()));
    let store = acct.as_tenant(BILL);
    let mut w = SegmentWriter::new(10);
    for i in 0..50i64 {
        let mut d = Document::new(format!("d{i:04}"), vec![i as f32, 1.0]);
        d.attrs
            .insert("n".to_owned(), pstore_format::Value::Int(i * 3));
        if i % 2 == 0 {
            d.attrs.insert(
                "even".to_owned(),
                pstore_format::Value::Str(format!("e{i}")),
            );
        }
        w.push(d);
    }
    let key = Key::new("0000/seg/attrs");
    store.put(&key, w.finish()).await.unwrap();
    let seg = Segment::open(&store, &key).await.unwrap();

    let before = acct.count(BILL, OpClass::Read);
    let rows = seg.rows_at(&store, &key, &[7, 42, 999, 8]).await.unwrap();
    assert_eq!(acct.count(BILL, OpClass::Read) - before, 1);
    let got: Vec<Option<Row>> = rows
        .into_iter()
        .map(|r| r.map(|d| (d.id, d.attrs.into_iter().collect())))
        .collect();
    use pstore_format::Value::{Int, Str};
    assert_eq!(
        got,
        vec![
            Some(("d0007".to_owned(), vec![("n".to_owned(), Int(21))])),
            Some((
                "d0042".to_owned(),
                vec![
                    ("even".to_owned(), Str("e42".to_owned())),
                    ("n".to_owned(), Int(126))
                ]
            )),
            None,
            Some((
                "d0008".to_owned(),
                vec![
                    ("even".to_owned(), Str("e8".to_owned())),
                    ("n".to_owned(), Int(24))
                ]
            )),
        ]
    );
    // And `ids_at` is the same answer, ids only.
    assert_eq!(
        seg.ids_at(&store, &key, &[7, 42, 999, 8]).await.unwrap(),
        vec![
            Some("d0007".to_owned()),
            Some("d0042".to_owned()),
            None,
            Some("d0008".to_owned())
        ]
    );
}
