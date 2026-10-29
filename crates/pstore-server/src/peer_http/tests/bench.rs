//! Isolated, ignored transport campaign. No CI latency threshold.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "measurement fixtures report failure through assertions"
)]
use super::{Handler, PeerTransport, Pool, Protocol, serve_typed};
use crate::{
    PeerTls,
    peer_wire::{self, Reply},
};
use serde::{Deserialize, Serialize};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use std::time::{Duration, Instant};

#[derive(Clone, Serialize, Deserialize)]
struct Payload {
    query: Vec<u32>,
    padding: String,
}

fn digest(payload: &Payload) -> u64 {
    payload
        .query
        .iter()
        .flat_map(|bits| bits.to_le_bytes())
        .chain(payload.padding.bytes())
        .fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x100_0000_01b3)
        })
}

fn cpu_ticks() -> Option<u64> {
    let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
    let rest: Vec<_> = stat.rsplit_once(')')?.1.split_whitespace().collect();
    Some(rest.get(11)?.parse::<u64>().ok()? + rest.get(12)?.parse::<u64>().ok()?)
}

#[derive(Default)]
struct Traffic {
    packets: AtomicU64,
    bytes: AtomicU64,
    dropped: AtomicU64,
    every: AtomicU64,
}
struct Relay {
    address: std::net::SocketAddr,
    traffic: Arc<Traffic>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Relay {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn relay(server: std::net::SocketAddr) -> Relay {
    let socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let address = socket.local_addr().unwrap();
    let traffic = Arc::new(Traffic::default());
    let counters = traffic.clone();
    let task = tokio::spawn(async move {
        let mut client = None;
        let mut buffer = vec![0; 65536];
        let mut directions = [0_u64; 2];
        loop {
            let (size, source) = socket.recv_from(&mut buffer).await.unwrap();
            let (direction, destination) = if source == server {
                (1, client)
            } else {
                client = Some(source);
                (0, Some(server))
            };
            let Some(destination) = destination else {
                continue;
            };
            directions[direction] += 1;
            counters.packets.fetch_add(1, Ordering::Relaxed);
            counters.bytes.fetch_add(size as u64, Ordering::Relaxed);
            let every = counters.every.load(Ordering::Relaxed);
            if every != 0 && directions[direction].is_multiple_of(every) {
                counters.dropped.fetch_add(1, Ordering::Relaxed);
            } else {
                socket.send_to(&buffer[..size], destination).await.unwrap();
            }
        }
    });
    Relay {
        address,
        traffic,
        task,
    }
}

fn traffic(value: Option<&Traffic>) -> (u64, u64, u64) {
    value.map_or((0, 0, 0), |v| {
        (
            v.packets.load(Ordering::Relaxed),
            v.bytes.load(Ordering::Relaxed),
            v.dropped.load(Ordering::Relaxed),
        )
    })
}

async fn call(pool: &Arc<Pool>, url: &str, body: Payload, expected: u64) {
    let reply: Reply<Payload> = pool
        .call_with_timeout(url, pstore_types::TenantId(9), body, Duration::from_secs(5))
        .await
        .unwrap();
    assert_eq!(reply.status, 200);
    assert_eq!(digest(&reply.body), expected);
}

async fn sample(
    row: (usize, &str, usize, usize),
    pool: Arc<Pool>,
    url: &str,
    payload: &Payload,
    wire: Option<&Traffic>,
) {
    let (sample, case, iterations, concurrency) = row;
    let expected = digest(payload);
    let before = traffic(wire);
    let cpu_before = cpu_ticks();
    let started = Instant::now();
    let mut latencies = Vec::with_capacity(iterations);
    for batch in (0..iterations).step_by(concurrency) {
        let mut requests = futures_util::stream::FuturesUnordered::new();
        for _ in batch..(batch + concurrency).min(iterations) {
            let body = payload.clone();
            let pool = pool.clone();
            requests.push(async move {
                let at = Instant::now();
                call(&pool, url, body, expected).await;
                at.elapsed()
            });
        }
        use futures_util::StreamExt;
        while let Some(elapsed) = requests.next().await {
            latencies.push(elapsed);
        }
    }
    let elapsed = started.elapsed();
    let cpu = cpu_ticks()
        .zip(cpu_before)
        .map(|(after, before)| after - before);
    let after = traffic(wire);
    latencies.sort();
    let encoded = peer_wire::encode(&peer_wire::Request {
        version: peer_wire::VERSION,
        id: 1,
        tenant: "9".to_owned(),
        body: payload,
    })
    .unwrap()
    .len();
    println!(
        "{sample},{case},{iterations},{concurrency},{encoded},{:.2},{:.2},{:.2},{},{},{},{},{expected}",
        elapsed.as_nanos() as f64 / iterations as f64,
        latencies[iterations / 2].as_nanos(),
        latencies[(iterations * 95 / 100).min(iterations - 1)].as_nanos(),
        cpu.map_or_else(|| "NA".to_owned(), |ticks| ticks.to_string()),
        after.0 - before.0,
        after.1 - before.1,
        after.2 - before.2
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "isolated provisional transport campaign; run this test alone in release"]
async fn peer_transport_microbench() {
    let ca = crate::tls_for_tests::Authority::new();
    let material = ca.issue(&["127.0.0.1"], false);
    let tls = PeerTls::from_pem(
        material.ca.as_bytes(),
        material.cert.as_bytes(),
        material.key.as_bytes(),
    )
    .unwrap();
    let (tcp, udp) = super::tests::same_port().await;
    let address = tcp.local_addr().unwrap();
    let calls = Arc::new([AtomicU64::new(0), AtomicU64::new(0)]);
    let handler: Handler<Payload, Payload> = {
        let calls = calls.clone();
        Arc::new(move |protocol, req| {
            calls[match protocol {
                Protocol::H2 => 0,
                Protocol::H3 => 1,
            }]
            .fetch_add(1, Ordering::Relaxed);
            Box::pin(async move {
                Reply {
                    version: peer_wire::VERSION,
                    id: req.id,
                    status: 200,
                    body: req.body,
                }
            })
        })
    };
    let server_tls = tls.clone();
    let server = tokio::spawn(async move {
        serve_typed(tcp, udp, server_tls, handler).await.unwrap();
    });
    let url = format!("https://{address}");
    let payload = Payload {
        query: (0..768)
            .map(|i| ((i as f32 * 0.125).sin()).to_bits())
            .collect(),
        padding: String::new(),
    };
    let stranger = crate::tls_for_tests::Authority::new();
    let wrong = stranger.issue(&["127.0.0.1"], false);
    let untrusted = PeerTls::from_pem(
        wrong.ca.as_bytes(),
        wrong.cert.as_bytes(),
        wrong.key.as_bytes(),
    )
    .unwrap();
    let large = Payload {
        padding: "x".repeat(128 * 1024),
        ..payload.clone()
    };
    let clock_hz = std::process::Command::new("getconf")
        .arg("CLK_TCK")
        .output()
        .ok()
        .filter(|result| result.status.success())
        .and_then(|result| String::from_utf8(result.stdout).ok())
        .unwrap_or_else(|| "NA".to_owned());
    println!(
        "provisional loopback,2 runtime workers; process CPU ticks/second={}",
        clock_hz.trim()
    );
    println!(
        "sample,case,iterations,concurrency,request_frame_bytes,ns_per_rpc,p50_ns,p95_ns,process_cpu_ticks,udp_packets,udp_bytes,udp_dropped,digest"
    );
    for sample_id in 0..5 {
        let cases = if sample_id % 2 == 0 {
            [("h2", PeerTransport::H2), ("h3", PeerTransport::H3)]
        } else {
            [("h3", PeerTransport::H3), ("h2", PeerTransport::H2)]
        };
        for (label, mode) in cases {
            let before =
                calls[if mode == PeerTransport::H2 { 0 } else { 1 }].load(Ordering::Relaxed);
            let cold = Instant::now();
            for _ in 0..10 {
                let pool = Pool::new(tls.clone(), mode, Duration::from_secs(5)).unwrap();
                call(&pool, &url, payload.clone(), digest(&payload)).await;
            }
            println!(
                "cold,{sample_id},{label},10,{:.2}",
                cold.elapsed().as_nanos() as f64 / 10.0
            );
            let pool = Pool::new(tls.clone(), mode, Duration::from_secs(5)).unwrap();
            for _ in 0..32 {
                call(&pool, &url, payload.clone(), digest(&payload)).await;
            }
            sample(
                (sample_id, &format!("{label}_warm"), 256, 1),
                pool.clone(),
                &url,
                &payload,
                None,
            )
            .await;
            sample(
                (sample_id, &format!("{label}_multiplex"), 256, 32),
                pool.clone(),
                &url,
                &payload,
                None,
            )
            .await;
            sample(
                (sample_id, &format!("{label}_large"), 32, 1),
                pool.clone(),
                &url,
                &large,
                None,
            )
            .await;
            assert_eq!(
                calls[if mode == PeerTransport::H2 { 0 } else { 1 }].load(Ordering::Relaxed)
                    - before,
                10 + 32 + 256 + 256 + 32
            );
        }
        let proxy = relay(address).await;
        let pool = Pool::new(tls.clone(), PeerTransport::H3, Duration::from_secs(5)).unwrap();
        let loss_url = format!("https://{}", proxy.address);
        for _ in 0..32 {
            call(&pool, &loss_url, payload.clone(), digest(&payload)).await;
        }
        sample(
            (sample_id, "h3_relay_control", 128, 1),
            pool.clone(),
            &loss_url,
            &payload,
            Some(&proxy.traffic),
        )
        .await;
        proxy.traffic.every.store(20, Ordering::Relaxed);
        sample(
            (sample_id, "h3_loss_every20", 128, 1),
            pool,
            &loss_url,
            &payload,
            Some(&proxy.traffic),
        )
        .await;
        assert!(proxy.traffic.dropped.load(Ordering::Relaxed) > 0);
        let blackhole = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let blackhole_address = blackhole.local_addr().unwrap();
        let tcp = tokio::net::TcpListener::bind(blackhole_address)
            .await
            .unwrap();
        let udp = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let tls_server = tls.clone();
        let handler: Handler<Payload, Payload> = Arc::new(|_, req| {
            Box::pin(async move {
                Reply {
                    version: peer_wire::VERSION,
                    id: req.id,
                    status: 200,
                    body: req.body,
                }
            })
        });
        let fallback_server = tokio::spawn(async move {
            serve_typed(tcp, udp, tls_server, handler).await.unwrap();
        });
        let pool = Pool::new(tls.clone(), PeerTransport::Auto, Duration::from_secs(2)).unwrap();
        let fallback_url = format!("https://{blackhole_address}");
        sample(
            (sample_id, "auto_udp_unavailable_cold", 1, 1),
            pool.clone(),
            &fallback_url,
            &payload,
            None,
        )
        .await;
        sample(
            (sample_id, "auto_cached_h2", 128, 1),
            pool.clone(),
            &fallback_url,
            &payload,
            None,
        )
        .await;
        assert_eq!((pool.stats().probes, pool.stats().fallbacks), (1, 1));
        fallback_server.abort();
        let before = [
            calls[0].load(Ordering::Relaxed),
            calls[1].load(Ordering::Relaxed),
        ];
        let rejected = Pool::new(
            untrusted.clone(),
            PeerTransport::Auto,
            Duration::from_secs(2),
        )
        .unwrap();
        let at = Instant::now();
        let result: Result<Reply<Payload>, _> = rejected
            .call_with_timeout(
                &url,
                pstore_types::TenantId(9),
                payload.clone(),
                Duration::from_secs(2),
            )
            .await;
        let elapsed = at.elapsed();
        assert!(matches!(result, Err(super::Error::Authentication)));
        assert_eq!(
            (
                rejected.stats().probes,
                rejected.stats().fallbacks,
                rejected.stats().h2
            ),
            (1, 0, 0)
        );
        assert_eq!(
            [
                calls[0].load(Ordering::Relaxed),
                calls[1].load(Ordering::Relaxed)
            ],
            before
        );
        println!("refusal,{sample_id},auto_wrong_ca,1,{}", elapsed.as_nanos());
    }
    server.abort();
}
