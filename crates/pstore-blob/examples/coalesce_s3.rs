//! M85: what the blob layer's coalescing gap does to the multi-vector lists, over S3's HTTP API.
//!
//! Clusters 1,500 encoded rows of 2 KB into 12 lists, puts the `.fdev` object in a bucket, and
//! reads 4 of the 12 lists (a probe of four) through `ObjectStoreBackend` under each gap. Prints
//! the lists' bytes, the planned fetches and the bytes they transfer. The runner
//! (`scripts/coalesce-s3.sh`) counts the GETs the server saw and compares.
//!
//! ⚠️ **Provisional (non-negotiable 7).** Against an S3-API *emulator* (moto) this measures how
//! many requests and bytes the planner produces for this layout, which is deterministic. It says
//! nothing about latency, throughput or S3's own behaviour; that needs a real bucket.

#![allow(
    clippy::print_stdout,
    clippy::print_stderr,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::cast_precision_loss,
    clippy::indexing_slicing,
    reason = "this example's output IS its product"
)]

use pstore_blob::{BlobStore, Key, ObjectStoreBackend};
use pstore_index::cluster::Params;
use pstore_index::fde_clusters::{self, FdeClusters};
use std::sync::Arc;

fn env(k: &str, d: &str) -> String {
    std::env::var(k).unwrap_or_else(|_| d.to_owned())
}

fn rnd(s: &mut u64) -> f32 {
    *s = s
        .wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(1_442_695_040_888_963_407);
    ((*s >> 40) as f32 / (1u64 << 24) as f32) * 2.0 - 1.0
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let s3 = object_store::aws::AmazonS3Builder::new()
        .with_endpoint(env("PSTORE_S3_ENDPOINT", "http://127.0.0.1:5555"))
        .with_bucket_name(env("PSTORE_S3_BUCKET", "pstore"))
        .with_access_key_id(env("PSTORE_ACCESS_KEY", "x"))
        .with_secret_access_key(env("PSTORE_SECRET_KEY", "y"))
        .with_allow_http(true)
        .with_region("us-east-1")
        .build()
        .expect("an S3 client");
    let inner: Arc<dyn object_store::ObjectStore> = Arc::new(s3);

    // 1,500 rows of 512 floats (2 KB, an FDE row's size), in 12 topics.
    let dim = 512;
    let mut state = 85u64;
    let topics: Vec<Vec<f32>> = (0..12)
        .map(|_| (0..dim).map(|_| 2.0 * rnd(&mut state)).collect())
        .collect();
    let rows: Vec<Vec<f32>> = (0..1_500)
        .map(|i| {
            topics[i % 12]
                .iter()
                .map(|x| x + 0.3 * rnd(&mut state))
                .collect()
        })
        .collect();
    let params = Params {
        target_list_size: 125,
        ..Params::default()
    };
    let (clusters, blob) = FdeClusters::build("x", &rows, params);
    let seg = Key::new(format!("m85/{}.seg", env("RUN", "run")));
    let caps = |gap: u64| {
        let mut c = ObjectStoreBackend::unprobed("moto");
        c.coalesce_gap = gap;
        c
    };
    let mode = std::env::args().nth(1).unwrap_or_default();
    let query: Vec<f32> = topics[3].clone();
    println!(
        "lists {}  object {} bytes  rows {}",
        clusters.lists(),
        blob.len(),
        rows.len()
    );
    match mode.as_str() {
        // Writes the object once.
        "put" => {
            let writer = ObjectStoreBackend::new(Arc::clone(&inner), caps(1 << 20));
            writer
                .put(&fde_clusters::vectors_key(&seg), blob.into())
                .await
                .unwrap();
            println!("put");
        }
        // Reads four lists under the gap given in bytes.
        "read" => {
            let gap: u64 = std::env::args().nth(2).unwrap().parse().unwrap();
            let store = ObjectStoreBackend::new(Arc::clone(&inner), caps(gap));
            let got = clusters
                .search_vector_at(&store, &seg, 0, &query, 4, 10)
                .await
                .unwrap();
            println!(
                "gap {gap}  top ids {:?}",
                got.iter().map(|g| g.0).collect::<Vec<_>>()
            );
        }
        // What each gap plans for those four lists, from the planner the store uses.
        _ => {
            let width = dim as u64 * 4;
            let sizes: Vec<u64> = (0..clusters.lists())
                .map(|i| clusters.list_len(i) as u64)
                .collect();
            let mut starts = Vec::new();
            let mut at = 0u64;
            for s in &sizes {
                starts.push(at);
                at += s;
            }
            let ranges: Vec<std::ops::Range<u64>> = clusters
                .probe_order(&query)
                .iter()
                .take(4)
                .map(|i| starts[*i] * width..(starts[*i] + sizes[*i]) * width)
                .collect();
            let asked: u64 = ranges.iter().map(|r| r.end - r.start).sum();
            println!("asked {asked} bytes in {} ranges", ranges.len());
            for gap in [64u64 << 10, 1 << 20] {
                let plan = pstore_blob::coalesce(&ranges, gap);
                let moved: u64 = plan.iter().map(|f| f.span.end - f.span.start).sum();
                println!("plan gap {gap}: {} fetches, {moved} bytes", plan.len());
            }
        }
    }
}
