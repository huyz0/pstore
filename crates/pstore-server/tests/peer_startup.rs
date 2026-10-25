//! A standalone process must serve both encrypted peer protocols and shut them down.
#![cfg(unix)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "fixture and assertion failures report through panic"
)]
#[path = "support/peer.rs"]
mod peer_fixture;
use serde_json::json;
use std::net::{SocketAddr, TcpListener, UdpSocket};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct Process(Child);
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct Directory(std::path::PathBuf);
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

async fn ready(process: &mut Process, address: SocketAddr) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        assert!(
            process.0.try_wait().unwrap().is_none(),
            "standalone server exited before binding"
        );
        if tokio::net::TcpStream::connect(address).await.is_ok() {
            return;
        }
        assert!(Instant::now() < deadline, "standalone server did not bind");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn standalone_serves_http2_and_http3_then_releases_both_listeners_on_sigterm() {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let directory = Directory(std::env::temp_dir().join(format!(
        "pstore-peer-startup-{}-{unique}",
        std::process::id()
    )));
    std::fs::create_dir(&directory.0).unwrap();
    let [ca, cert, key] = peer_fixture::write_identity(&directory.0);
    let public = TcpListener::bind("127.0.0.1:0").unwrap();
    let peer = TcpListener::bind("127.0.0.1:0").unwrap();
    let public_address = public.local_addr().unwrap();
    let peer_address = peer.local_addr().unwrap();
    let udp = UdpSocket::bind(peer_address).unwrap();
    drop((public, peer, udp));
    let mut process = Process(
        Command::new(env!("CARGO_BIN_EXE_pstore-server"))
            .env_clear()
            .env("PSTORE_ALLOW_UNAUTHENTICATED", "1")
            .env("PSTORE_LANE", "1")
            .env("PSTORE_BACKEND", "memory")
            .env("PSTORE_BIND", public_address.to_string())
            .env("PSTORE_PEER_BIND", peer_address.to_string())
            .env("PSTORE_PEER_CA_FILE", ca)
            .env("PSTORE_PEER_CERT_FILE", cert)
            .env("PSTORE_PEER_KEY_FILE", key)
            .env("PSTORE_PEER_TRANSPORT", "h2")
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    ready(&mut process, public_address).await;
    ready(&mut process, peer_address).await;
    let part = json!({"protocol":1,"index":"docs","fts":pstore_format::text::FullText::default().encode(),"filters":null,"shadow":0,"legs":[],"targets":[]});
    let url = format!("https://{peer_address}");
    for (status, body) in [
        peer_fixture::call(&url, "9", part.clone()).await,
        peer_fixture::call_h3(&url, "9", part).await,
    ] {
        assert_eq!(status, 200, "{body}");
        assert_eq!(body, json!({"hits":[]}));
    }
    assert!(
        Command::new("kill")
            .args(["-TERM", &process.0.id().to_string()])
            .status()
            .unwrap()
            .success()
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = process.0.try_wait().unwrap() {
            assert!(status.success(), "{status}");
            break;
        }
        assert!(
            Instant::now() < deadline,
            "SIGTERM did not stop both server futures"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(TcpListener::bind(public_address).is_ok());
    assert!(TcpListener::bind(peer_address).is_ok());
    assert!(UdpSocket::bind(peer_address).is_ok());
}
