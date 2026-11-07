//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::cast_precision_loss,
    clippy::float_cmp,
    reason = "assertions in tests are the reporting mechanism"
)]

//! M199: a document export at one epoch -- an order by id, `as_of` that epoch, with every
//! vector field, paged by `id > last`.

use pstore_blob::MemoryStore;
use pstore_engine::{Engine, Metric};
use pstore_format::{Document, Impact, Value, VectorField};
use pstore_index::vec_index::Query;
use pstore_query::{Fusion, Op, OrderBy, Predicate, Prefetch};
use pstore_types::{Epoch, LaneId, TenantId};
use std::sync::Arc;

const NAMED: &str = "emb";
const SPARSE: &str = "body_sparse";

fn by_id() -> OrderBy {
    OrderBy {
        attr: "id".to_owned(),
        desc: false,
    }
}

/// A row with `vector`, a named dense field, a sparse field and an attribute; `salt` makes a
/// replacement differ in every one of them.
fn row(i: usize, salt: f32) -> Document {
    let x = i as f32 + salt;
    let mut d = Document::new(format!("d{i:02}"), vec![1.0 + x * 0.25, x.sin(), 0.5 - x]);
    d.vectors
        .insert(NAMED.into(), VectorField::dense(vec![x * 0.5, 2.0 - x]));
    d.vectors.insert(
        SPARSE.into(),
        VectorField::Sparse(vec![
            ((i % 5).to_string(), Impact::new(0.5 + salt)),
            ((10 + i % 2).to_string(), Impact::new(0.25)),
        ]),
    );
    d.attrs.insert(
        "k".to_owned(),
        Value::Int(i64::try_from(i).unwrap() + 100 * salt as i64),
    );
    d
}

/// One page of `index` as of `epoch`, every vector field projected.
async fn page_at(
    e: &Engine<MemoryStore>,
    index: &str,
    epoch: Epoch,
    after: Option<&str>,
    limit: usize,
) -> Vec<Document> {
    let filter = after.map(|c| Predicate::Cmp("id".to_owned(), Op::Gt, Value::Str(c.to_owned())));
    e.ordered_projected_as_of(
        index,
        &by_id(),
        filter.as_ref(),
        0,
        limit,
        epoch,
        true,
        &[NAMED.to_owned()],
        true,
    )
    .await
    .unwrap()
    .rows
}

/// Every row of `index` as of `epoch`, paged 7 at a time as the docs' export loop does.
async fn export(e: &Engine<MemoryStore>, index: &str, epoch: Epoch) -> Vec<Document> {
    let mut out: Vec<Document> = Vec::new();
    loop {
        let last = out.last().map(|d| d.id.clone());
        let page = page_at(e, index, epoch, last.as_deref(), 7).await;
        if page.is_empty() {
            return out;
        }
        out.extend(page);
    }
}

/// The present's rows, unpaged, every vector field projected.
async fn now(e: &Engine<MemoryStore>, index: &str) -> Vec<Document> {
    e.ordered_projected(
        index,
        &by_id(),
        None,
        0,
        1000,
        pstore_engine::Consistency::Eventual,
        true,
        &[NAMED.to_owned()],
        true,
    )
    .await
    .unwrap()
    .rows
}

/// AC 1's index: 30 rows over two folds, `E` the second, then a delete, a replacement, new
/// rows and a compaction after it. Returns the engine, `E`, and the rows at `E`.
async fn changed_after_e(
    metric: Metric,
) -> (Arc<MemoryStore>, Engine<MemoryStore>, Epoch, Vec<Document>) {
    let store = Arc::new(MemoryStore::new());
    let e = Engine::new(Arc::clone(&store), TenantId(19_900), LaneId(1));
    for chunk in (0..30).collect::<Vec<usize>>().chunks(15) {
        e.write_as("src", chunk.iter().map(|i| row(*i, 0.0)).collect(), metric)
            .await
            .unwrap();
        e.flush().await.unwrap();
        e.fold().await.unwrap();
    }
    let epoch = e.index_stats_at("src").await.unwrap().0;
    let at_e = now(&e, "src").await;
    assert_eq!(at_e.len(), 30, "the fixture's rows");
    // After `E`: a delete, a replacement, new rows, and a compaction.
    e.delete("src", vec!["d03".to_owned()]).await.unwrap();
    e.write_as("src", vec![row(5, 1.0), row(40, 0.0), row(41, 0.0)], metric)
        .await
        .unwrap();
    e.flush().await.unwrap();
    e.fold().await.unwrap();
    e.compact("src").await.unwrap().expect("compacted");
    let present = now(&e, "src").await;
    assert_ne!(
        present, at_e,
        "the changes after E changed nothing the export reads"
    );
    (store, e, epoch, at_e)
}

#[tokio::test]
async fn an_order_pages_one_past_epoch_with_its_vectors() {
    let (_, e, epoch, at_e) = changed_after_e(Metric::DotProduct).await;
    let got = export(&e, "src", epoch).await;
    assert_eq!(
        got.len(),
        at_e.len(),
        "the export holds E's rows, no more, no fewer"
    );
    assert_eq!(got, at_e, "the export is E's rows, attributes and vectors");
    // Every field is there: `vector`, the named field, the sparse field.
    for d in &got {
        assert_eq!(d.vectors.len(), 3, "{} lost a vector field", d.id);
    }
}

/// Every row's `vector` as of `epoch`, with its width.
async fn vectors_at(e: &Engine<MemoryStore>, index: &str, epoch: Epoch) -> Vec<(String, Vec<f32>)> {
    e.ordered_projected_as_of(index, &by_id(), None, 0, 100, epoch, true, &[], false)
        .await
        .unwrap()
        .rows
        .into_iter()
        .map(|d| {
            let v = first(&d);
            (d.id, v)
        })
        .collect()
}

/// A row's `vector`.
fn first(d: &Document) -> Vec<f32> {
    d.vectors
        .get(pstore_format::DEFAULT_FIELD)
        .expect("a vector")
        .as_dense()[0]
        .clone()
}

fn written(n: usize) -> Vec<Document> {
    (0..n)
        .map(|i| Document::new(format!("d{i}"), vec![1.0 + i as f32, -2.0, 0.5 * i as f32]))
        .collect()
}

fn assert_as_written(got: &[(String, Vec<f32>)], want: &[Document], what: &str) {
    assert_eq!(got.len(), want.len(), "{what}: rows");
    for ((id, v), d) in got.iter().zip(want) {
        assert_eq!(id, &d.id, "{what}: order");
        let w = &first(d);
        assert_eq!(v.len(), w.len(), "{what}: {id} has an extra dimension");
        assert!(
            v.iter()
                .zip(w)
                .all(|(a, b)| (a - b).abs() <= 1e-5 * (1.0 + b.abs())),
            "{what}: {id} is {v:?}, written {w:?}"
        );
    }
}

#[tokio::test]
async fn a_past_epoch_gives_vectors_back_in_its_own_metric() {
    let e = Engine::new(Arc::new(MemoryStore::new()), TenantId(19_901), LaneId(1));
    let want = written(5);
    // `x`: euclidean at E, dropped after it, re-created under dot product.
    // `y`: euclidean at E, dropped, re-created under dot product, dropped again.
    for index in ["x", "y"] {
        e.write_as(index, want.clone(), Metric::EuclideanSquared)
            .await
            .unwrap();
    }
    e.flush().await.unwrap();
    e.fold().await.unwrap();
    let epoch = e.index_stats_at("x").await.unwrap().0;
    for index in ["x", "y"] {
        e.delete_index(index).await.unwrap().expect("dropped");
        e.write_as(index, written(2), Metric::DotProduct)
            .await
            .unwrap();
    }
    e.flush().await.unwrap();
    e.fold().await.unwrap();
    e.delete_index("y").await.unwrap().expect("dropped again");

    assert_as_written(&vectors_at(&e, "x", epoch).await, &want, "re-created");
    assert_as_written(&vectors_at(&e, "y", epoch).await, &want, "dropped twice");
}

#[tokio::test]
async fn a_restore_of_a_paged_export_answers_as_its_epoch() {
    let (store, e, epoch, at_e) = changed_after_e(Metric::DotProduct).await;
    let pages = export(&e, "src", epoch).await;
    // The schema to restore under is the summary's, read with `E`.
    let schema = e
        .index_stats_at("src")
        .await
        .unwrap()
        .1
        .and_then(|s| s.schema)
        .expect("a folded index records its schema");
    let fresh = Engine::new(Arc::clone(&store), TenantId(19_902), LaneId(1));
    fresh.write_as("dst", pages, schema.metric).await.unwrap();
    fresh.flush().await.unwrap();
    fresh.fold().await.unwrap();

    // The full digest: every row by id, its attributes and every vector field.
    assert_eq!(now(&fresh, "dst").await, at_e, "the restore's rows are E's");

    // Its exact-vector and filter answers are the source's at E.
    let leg = vec![Prefetch::Dense {
        field: pstore_format::DEFAULT_FIELD.to_owned(),
        query: vec![0.3, -1.0, 2.0],
        limit: 10,
        tune: Query {
            exact: true,
            ..Query::default()
        },
    }];
    let filter = Predicate::Cmp("k".to_owned(), Op::Lt, Value::Int(12));
    let source = e
        .query_as_of_filtered("src", epoch, &leg, Some(&filter), Fusion::default(), 10)
        .await
        .unwrap();
    let restored = fresh
        .query_filtered("dst", &leg, Some(&filter), Fusion::default(), 10)
        .await
        .unwrap();
    let ids = |e: &Engine<MemoryStore>, a| {
        e.resolve(a)
            .into_iter()
            .map(|(id, _)| id)
            .collect::<Vec<_>>()
    };
    let (s, r) = (ids(&e, &source), ids(&fresh, &restored));
    assert_eq!(
        s.len(),
        10,
        "the filter left fewer rows than the query asks"
    );
    assert_eq!(r, s, "the restore answers the exact query as E did");
}
