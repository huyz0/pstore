//! M286: `object_store`'s S3 client and pstore's own, on pstore's access patterns.
//!
//!   cargo run --release -p pstore-blob --features object_store,s3-native --example s3_clients
//!
//! The fake S3 runs in a **child process** (`s3_clients serve`), so this process's CPU time
//! and allocations are the client's alone. Loopback has no network latency: what this
//! measures is each client's own cost a request, which is the only thing that differs
//! between them. ⚠️ `provisional` on any machine that is not the deployment's.

#![allow(
    clippy::print_stdout,
    clippy::print_stderr,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::indexing_slicing,
    reason = "a benchmark: its output is its product, and a failure should stop it loudly"
)]

use bytes::Bytes;
use futures_util::StreamExt;
use pstore_blob::s3::S3Backend;
use pstore_blob::{BlobStore, Key, ObjectStoreBackend, Precondition};
use pstore_testkit::fake_s3::{BUCKET, DISCARD, FakeS3, KEY_ID, REGION, SECRET};
use stats_alloc::{INSTRUMENTED_SYSTEM, Region, StatsAlloc};
use std::alloc::System;
use std::io::BufRead;
use std::sync::Arc;
use std::time::Instant;

#[global_allocator]
static GLOBAL: &StatsAlloc<System> = &INSTRUMENTED_SYSTEM;

/// This process's user + system CPU, in seconds, from `/proc/self/stat` (Linux).
fn cpu_seconds() -> f64 {
    let s = std::fs::read_to_string("/proc/self/stat").unwrap_or_default();
    // Fields after the parenthesised command name; utime and stime are the 12th and 13th.
    let rest = s.rsplit(") ").next().unwrap_or_default();
    let f: Vec<u64> = rest.split(' ').filter_map(|x| x.parse().ok()).collect();
    // `rest` starts at field 3 (state, not a number, so skipped): utime is field 14.
    let ticks = f.get(10).copied().unwrap_or(0) + f.get(11).copied().unwrap_or(0);
    ticks as f64 / 100.0
}

struct Pattern {
    name: &'static str,
    ops: usize,
    concurrency: usize,
}

const OBJECT: usize = 8 << 20;

async fn one(store: &dyn BlobStore, p: &str, i: usize, big: &Key, wal: &Bytes, seg: &Bytes) -> u64 {
    match p {
        "wal_put_16k" => {
            let k = Key::new(format!("{DISCARD}wal/{i}"));
            store
                .put_conditional(&k, wal.clone(), Precondition::NotExists)
                .await
                .unwrap();
            1
        }
        "range_get_64k" => {
            let off = (i * 7919 * 4096) % (OBJECT - 65536);
            let off = off as u64;
            store.get_range(big, off..off + 65536).await.unwrap();
            1
        }
        "suffix_get_64k" => {
            store.get_suffix(big, 65536).await.unwrap();
            1
        }
        "fanout_256x4k" => {
            let reads = (0..256u64).map(|j| {
                let off = (j * 31 + i as u64 * 4096) % (OBJECT as u64 - 4096);
                store.get_range(big, off..off + 4096)
            });
            futures_util::future::try_join_all(reads).await.unwrap();
            256
        }
        "segment_put_32m" => {
            store
                .put(&Key::new(format!("{DISCARD}seg/{i}")), seg.clone())
                .await
                .unwrap();
            1
        }
        "head" => {
            store.stat(big).await.unwrap();
            1
        }
        "delete_1000" => {
            let keys: Vec<Key> = (0..1000)
                .map(|j| Key::new(format!("gone/{i}/{j}")))
                .collect();
            store.delete_batch(&keys).await.unwrap();
            1
        }
        _ => unreachable!(),
    }
}

struct Row {
    requests: u64,
    secs: f64,
    cpu: f64,
    allocs: usize,
    bytes: usize,
}

async fn run(store: Arc<dyn BlobStore>, p: &Pattern, big: &Key, wal: &Bytes, seg: &Bytes) -> Row {
    // Warm the pool and the code.
    futures_util::stream::iter(0..p.concurrency)
        .for_each_concurrent(p.concurrency, |i| {
            let s = Arc::clone(&store);
            async move {
                one(&*s, p.name, 1_000_000 + i, big, wal, seg).await;
            }
        })
        .await;
    let region = Region::new(GLOBAL);
    let (t0, c0) = (Instant::now(), cpu_seconds());
    let requests: u64 = futures_util::stream::iter(0..p.ops)
        .map(|i| {
            let s = Arc::clone(&store);
            async move { one(&*s, p.name, i, big, wal, seg).await }
        })
        .buffer_unordered(p.concurrency)
        .fold(0, |a, n| async move { a + n })
        .await;
    let (secs, cpu) = (t0.elapsed().as_secs_f64(), cpu_seconds() - c0);
    let st = region.change();
    Row {
        requests,
        secs,
        cpu,
        allocs: st.allocations + st.reallocations,
        bytes: st.bytes_allocated + st.bytes_reallocated.max(0) as usize,
    }
}

fn serve() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let fake = FakeS3::start().await.unwrap();
        println!("{}", fake.endpoint());
        // Until the parent closes stdin.
        let _ = tokio::task::spawn_blocking(|| {
            let mut s = String::new();
            let _ = std::io::stdin().read_line(&mut s);
        })
        .await;
        drop(fake);
    });
}

fn main() {
    if std::env::args().nth(1).as_deref() == Some("serve") {
        return serve();
    }
    let scale: usize = std::env::var("PSTORE_BENCH_SCALE")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1);
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .arg("serve")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut endpoint = String::new();
    std::io::BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut endpoint)
        .unwrap();
    let endpoint = endpoint.trim().to_owned();

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let cfg = pstore_blob::s3::S3Config {
            endpoint: endpoint.clone(),
            bucket: BUCKET.to_owned(),
            region: REGION.to_owned(),
            credentials: pstore_blob::s3::sign::Credentials {
                key_id: KEY_ID.to_owned(),
                secret: SECRET.to_owned(),
                token: None,
            },
            virtual_hosted: false,
        };
        let native: Arc<dyn BlobStore> =
            Arc::new(S3Backend::new(cfg, S3Backend::unprobed("native")).unwrap());
        let os = object_store::aws::AmazonS3Builder::new()
            .with_endpoint(&endpoint)
            .with_bucket_name(BUCKET)
            .with_region(REGION)
            .with_access_key_id(KEY_ID)
            .with_secret_access_key(SECRET)
            .with_allow_http(true)
            .build()
            .unwrap();
        let os: Arc<dyn BlobStore> =
            Arc::new(ObjectStoreBackend::new(Arc::new(os), ObjectStoreBackend::unprobed("os")));

        let big = Key::new("bench/object");
        let body: Bytes = (0..OBJECT).map(|i| (i % 251) as u8).collect::<Vec<_>>().into();
        native.put(&big, body).await.unwrap();
        let wal: Bytes = vec![7u8; 16 << 10].into();
        let seg: Bytes = vec![9u8; 32 << 20].into();
        let patterns = [
            Pattern { name: "wal_put_16k", ops: 4000 * scale, concurrency: 16 },
            Pattern { name: "range_get_64k", ops: 4000 * scale, concurrency: 16 },
            Pattern { name: "suffix_get_64k", ops: 4000 * scale, concurrency: 16 },
            Pattern { name: "fanout_256x4k", ops: 40 * scale, concurrency: 1 },
            Pattern { name: "segment_put_32m", ops: 16 * scale, concurrency: 4 },
            Pattern { name: "head", ops: 8000 * scale, concurrency: 16 },
            Pattern { name: "delete_1000", ops: 200 * scale, concurrency: 4 },
        ];
        println!(
            "| pattern | client | requests/s | client CPU µs/request | allocations/request | bytes allocated/request |"
        );
        println!("|---|---|---:|---:|---:|---:|");
        for p in &patterns {
            // Two rounds, the order reversed in the second, so neither client always runs
            // second on a warm fake.
            for (name, store) in [
                ("object_store", &os),
                ("native", &native),
                ("native", &native),
                ("object_store", &os),
            ] {
                let r = run(Arc::clone(store), p, &big, &wal, &seg).await;
                let n = r.requests as f64;
                println!(
                    "| {} | {} | {:.0} | {:.1} | {:.1} | {:.0} |",
                    p.name,
                    name,
                    n / r.secs,
                    r.cpu * 1e6 / n,
                    r.allocs as f64 / n,
                    r.bytes as f64 / n
                );
            }
        }
    });
    drop(child.stdin.take());
    let _ = child.wait();
}
