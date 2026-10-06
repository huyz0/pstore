//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! M54: a query whose vector legs run on other servers answers exactly as one that runs them
//! all here -- hit for hit, in score bits -- for what the API cannot ask: sparse legs, two
//! dense legs, a shadow of unfolded writes, deleted rows and a filter. Each "server" is an
//! engine over the same store, and a share is run through [`Engine::part`], the call the
//! server's endpoint makes.

use pstore_blob::MemoryStore;
use pstore_engine::{Engine, Part, Peers};
use pstore_format::{Document, Impact, Value, VectorField};
use pstore_query::{Fusion, Op, PartHits, Predicate, Prefetch};
use pstore_types::{LaneId, TenantId};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

const T: TenantId = TenantId(5400);

fn engine(store: &Arc<MemoryStore>, lane: u64) -> Engine<MemoryStore> {
    Engine::new(Arc::clone(store), T, LaneId(lane)).with_index_params(
        pstore_index::cluster::Params {
            // Every segment clustered, so the dense leg reads a centroid table.
            exact_scan_threshold: 8,
            ..pstore_index::cluster::Params::default()
        },
    )
}

fn doc(i: u32) -> Document {
    let x = i as f32;
    let mut d = Document::new(
        format!("d{i:05}"),
        vec![x.sin(), x.cos(), (x * 0.37).sin(), 1.0],
    );
    d.attrs.insert("n".to_owned(), Value::Int(i64::from(i)));
    d.attrs.insert(
        "text".to_owned(),
        Value::Str(format!("common word{} tag{}", i % 5, i % 3)),
    );
    d.vectors.insert(
        "v2".to_owned(),
        VectorField::Dense(vec![vec![(x * 0.11).cos(), (x * 0.7).sin(), 0.5]]),
    );
    d.vectors.insert(
        "s".to_owned(),
        VectorField::Sparse(vec![
            (3, Impact::new(0.5 + (i % 4) as f32 * 0.1)),
            (i % 7 + 10, Impact::new(0.25)),
        ]),
    );
    d
}

/// Three servers by name; this one is the first. Each share runs on its server's engine, and
/// is counted.
struct Three {
    names: Vec<String>,
    engines: Vec<Arc<Engine<MemoryStore>>>,
    filter: Option<Predicate>,
    parts: Arc<AtomicU64>,
}

impl Peers for Three {
    fn servers(&self) -> &[String] {
        &self.names
    }
    fn me(&self) -> usize {
        0
    }
    fn part(
        &self,
        server: usize,
        part: Part,
    ) -> futures_util::future::BoxFuture<'static, Result<PartHits, String>> {
        let engine = Arc::clone(&self.engines[server]);
        let filter = self.filter.clone();
        let parts = Arc::clone(&self.parts);
        Box::pin(async move {
            parts.fetch_add(1, Ordering::SeqCst);
            engine
                .part(&part, filter.as_ref())
                .await
                .map_err(|e| e.to_string())
        })
    }
}

/// Everything an answer says, floats by their bits.
fn exactly(a: &pstore_engine::Answer) -> String {
    let hits: Vec<(usize, usize, u32)> = a
        .hits
        .iter()
        .map(|h| (h.segment, h.row, h.score.to_bits()))
        .collect();
    let dists: Vec<Option<u32>> = a.dists.iter().map(|d| d.map(f32::to_bits)).collect();
    format!("{hits:?} {:?} {:?} {dists:?}", a.ids, a.attributes)
}

fn dense(field: &str, q: Vec<f32>) -> Prefetch {
    Prefetch::Dense {
        field: field.to_owned(),
        query: q,
        limit: 10,
        tune: pstore_index::vec_index::Query::default(),
    }
}

fn sparse() -> Prefetch {
    Prefetch::Sparse {
        field: "s".to_owned(),
        query: vec![(3, 1.0), (12, 0.5)],
        limit: 10,
    }
}

fn text() -> Prefetch {
    Prefetch::Text {
        field: "text".to_owned(),
        query: "word2 tag1".to_owned(),
        limit: 10,
    }
}

#[tokio::test]
async fn a_split_query_equals_the_unsplit_one() {
    let store = Arc::new(MemoryStore::new());
    let coordinator = engine(&store, 1);
    // Eight folded segments of 24 rows.
    for k in 0..8u32 {
        coordinator
            .write("idx", (k * 24..k * 24 + 24).map(doc).collect())
            .await
            .unwrap();
        coordinator.flush().await.unwrap();
        coordinator.fold().await.unwrap();
    }
    // Deleted rows, folded: delete vectors.
    coordinator
        .delete(
            "idx",
            (0..192).step_by(9).map(|i| format!("d{i:05}")).collect(),
        )
        .await
        .unwrap();
    coordinator.flush().await.unwrap();
    coordinator.fold().await.unwrap();
    // Unfolded upserts and deletes: a shadow over every folded segment.
    coordinator
        .write("idx", (0..192).step_by(7).map(|i| doc(i + 1_000)).collect())
        .await
        .unwrap();
    coordinator
        .write(
            "idx",
            (0..192)
                .step_by(5)
                .map(|i| {
                    let mut d = doc(i);
                    d.attrs.insert("n".to_owned(), Value::Int(-1));
                    d
                })
                .collect(),
        )
        .await
        .unwrap();
    coordinator
        .delete(
            "idx",
            (1..192).step_by(11).map(|i| format!("d{i:05}")).collect(),
        )
        .await
        .unwrap();

    let q = vec![0.3, 0.9, -0.2, 1.0];
    let filtered = Predicate::Cmp("n".to_owned(), Op::Gt, Value::Int(40));
    let cases: Vec<(&str, Vec<Prefetch>, Option<Predicate>, Fusion)> = vec![
        (
            "dense",
            vec![dense("vector", q.clone())],
            None,
            Fusion::default(),
        ),
        (
            "dense, filtered",
            vec![dense("vector", q.clone())],
            Some(filtered.clone()),
            Fusion::default(),
        ),
        ("sparse", vec![sparse()], None, Fusion::default()),
        (
            "two dense legs",
            vec![
                dense("vector", q.clone()),
                dense("v2", vec![0.2, -0.4, 0.5]),
            ],
            None,
            Fusion::default(),
        ),
        (
            "dense, sparse and text, filtered",
            vec![dense("vector", q.clone()), sparse(), text()],
            Some(filtered.clone()),
            Fusion::default(),
        ),
    ];
    let store_peer = |lane| Arc::new(engine(&store, lane));
    for (name, legs, filter, fusion) in cases {
        let unsplit = coordinator
            .query_filtered("idx", &legs, filter.as_ref(), fusion, 10)
            .await
            .unwrap();
        let peers = Three {
            names: vec!["http://a".into(), "http://b".into(), "http://c".into()],
            engines: vec![store_peer(10), store_peer(11), store_peer(12)],
            filter: filter.clone(),
            parts: Arc::new(AtomicU64::new(0)),
        };
        let split = coordinator
            .query_split_as(
                "idx",
                &legs,
                filter.as_ref(),
                fusion,
                10,
                pstore_engine::Consistency::Eventual,
                Some(&peers),
            )
            .await
            .unwrap();
        assert!(!unsplit.hits.is_empty(), "{name}: nothing to compare");
        assert_eq!(peers.parts.load(Ordering::SeqCst), 2, "{name}: not split");
        assert_eq!(exactly(&split), exactly(&unsplit), "{name}");
    }
}

#[tokio::test]
async fn a_share_that_fails_is_run_here() {
    // M54 rule 5: an error from a peer costs rounds, never the answer.
    struct Failing(Vec<String>, Arc<AtomicU64>);
    impl Peers for Failing {
        fn servers(&self) -> &[String] {
            &self.0
        }
        fn me(&self) -> usize {
            0
        }
        fn part(
            &self,
            _: usize,
            _: Part,
        ) -> futures_util::future::BoxFuture<'static, Result<PartHits, String>> {
            let n = Arc::clone(&self.1);
            Box::pin(async move {
                n.fetch_add(1, Ordering::SeqCst);
                Err("down".to_owned())
            })
        }
    }
    let store = Arc::new(MemoryStore::new());
    let w = engine(&store, 1);
    for k in 0..8u32 {
        w.write("idx", (k * 24..k * 24 + 24).map(doc).collect())
            .await
            .unwrap();
        w.flush().await.unwrap();
        w.fold().await.unwrap();
    }
    let legs = vec![dense("vector", vec![0.3, 0.9, -0.2, 1.0])];
    let unsplit = w.query("idx", &legs, Fusion::default(), 10).await.unwrap();
    let peers = Failing(
        vec!["http://a".into(), "http://b".into(), "http://c".into()],
        Arc::new(AtomicU64::new(0)),
    );
    let split = w
        .query_split_as(
            "idx",
            &legs,
            None,
            Fusion::default(),
            10,
            pstore_engine::Consistency::Eventual,
            Some(&peers),
        )
        .await
        .unwrap();
    assert_eq!(peers.1.load(Ordering::SeqCst), 2);
    assert_eq!(exactly(&split), exactly(&unsplit));
}

#[test]
fn assignment_is_stable_balanced_and_moves_only_to_a_new_server() {
    use pstore_engine::assign;
    let four: Vec<String> = (0..4).map(|i| format!("http://10.0.0.{i}:8080")).collect();
    let keys: Vec<String> = (0..10_000)
        .map(|i| format!("0001/tnt/1/idx/docs/seg/L0/{i:020}-0000000000000001.seg"))
        .collect();
    // The same server whatever the list order, and with a trailing `/`.
    let mut shuffled = four.clone();
    shuffled.reverse();
    shuffled[1].push('/');
    for k in &keys[..500] {
        let a = &four[assign(k, &four)];
        let b = shuffled[assign(k, &shuffled)].trim_end_matches('/');
        assert_eq!(a, b, "{k}");
    }
    let mut held = [0usize; 4];
    for k in &keys {
        held[assign(k, &four)] += 1;
    }
    for (i, n) in held.iter().enumerate() {
        assert!(
            (2_200..=2_800).contains(n),
            "server {i} holds {n} of 10,000"
        );
    }
    let mut five = four.clone();
    five.push("http://10.0.0.4:8080".to_owned());
    let mut moved = 0;
    for k in &keys {
        let (before, after) = (assign(k, &four), assign(k, &five));
        if before != after {
            assert_eq!(after, 4, "{k} moved between old servers");
            moved += 1;
        }
    }
    assert!((1_700..=2_300).contains(&moved), "{moved} of 10,000 moved");
    assert_eq!(assign("x", &[]), 0);
    // Pinned against an independent model (Python, in the ledger): every coordinator of every
    // build must agree, so the hash is part of the protocol. The sweep found the finaliser's
    // mutants alive -- a weaker mix is still stable and roughly balanced.
    let pinned: Vec<usize> = keys[..16].iter().map(|k| assign(k, &four)).collect();
    assert_eq!(pinned, [2, 3, 3, 3, 3, 1, 0, 3, 3, 0, 0, 2, 1, 3, 2, 3]);
}
