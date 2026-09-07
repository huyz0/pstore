//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "assertions in tests are the reporting mechanism"
)]

//! The UDP driver, on real sockets, in one process.
//!
//! ⚠️ The protocol itself is tested in `pstore-gossip` without any I/O at all. What is left
//! here is the part a pure test cannot reach — binding, encoding onto a socket, and the
//! accounting — and it is the part that shipped a hostname to `SocketAddr::parse` in M4b and
//! killed every node in the fleet at startup. A mocked transport would have been happy.

use pstore_node::swim;
use std::time::Duration;

/// A port nothing is listening on. Not a constant: `cargo mutants` runs mutants concurrently
/// and fixed ports made those copies fight, which surfaced as tests that hung rather than
/// failed.
fn free_port() -> u16 {
    std::net::UdpSocket::bind("127.0.0.1:0")
        .expect("the loopback must have a spare port")
        .local_addr()
        .expect("a bound socket has an address")
        .port()
}

const FAST: Duration = Duration::from_millis(50);

async fn member(port: u16, seeds: &[String]) -> swim::Member {
    swim::start(
        &format!("127.0.0.1:{port}"),
        &format!("127.0.0.1:{port}"),
        seeds,
        0.0,
        FAST,
    )
    .await
    .expect("a member must start on loopback")
}

#[tokio::test]
async fn a_member_advertises_the_address_a_peer_would_dial() {
    let p = free_port();
    let m = member(p, &[]).await;
    assert_eq!(m.self_addr(), format!("127.0.0.1:{p}"));
    assert_eq!(
        m.member_count().await,
        1,
        "a fresh member counts itself and nobody else"
    );
}

#[tokio::test]
async fn a_hostname_is_not_parsed_as_a_socket_address() {
    // ⚠️ M4b's most expensive one-line bug, pinned here so it cannot come back: a container
    // fleet advertises a NAME, and `SocketAddr::parse` fails on it. Every node exited at
    // startup, which reads as a crash loop rather than an address that was never resolved.
    // The listen address must be refused clearly rather than panicking.
    let r = swim::start("not-a-socket-addr", "127.0.0.1:1", &[], 0.0, FAST).await;
    assert!(r.is_err(), "an unparseable listen address was accepted");
}

#[tokio::test]
async fn two_members_find_each_other_from_a_seed() {
    let (a_port, b_port) = (free_port(), free_port());
    let a = member(a_port, &[]).await;
    let b = member(b_port, &[format!("127.0.0.1:{a_port}")]).await;

    for _ in 0..100 {
        if a.member_count().await == 2 && b.member_count().await == 2 {
            let members = a.members().await;
            assert!(
                members.contains(&b.self_addr()),
                "the view counted two members but did not name the peer"
            );
            return;
        }
        tokio::time::sleep(FAST).await;
    }
    panic!(
        "two seeded members never found each other: a saw {}, b saw {}",
        a.member_count().await,
        b.member_count().await
    );
}

#[tokio::test]
async fn dialling_teaches_a_member_about_a_peer() {
    // The roster backstop's whole mechanism: a node learns an address out of band and must be
    // able to act on it, because gossip alone cannot reach a node nobody knows.
    let m = member(free_port(), &[]).await;
    let peer = format!("127.0.0.1:{}", free_port());
    assert!(m.dial(&peer).await, "a valid address was refused");
    assert_eq!(m.member_count().await, 2, "a dialled peer was not learned");
    assert!(m.members().await.contains(&peer));
}

#[tokio::test]
async fn traffic_is_counted_in_both_directions() {
    let (a_port, b_port) = (free_port(), free_port());
    let a = member(a_port, &[]).await;
    let _b = member(b_port, &[format!("127.0.0.1:{a_port}")]).await;

    for _ in 0..100 {
        let (sent, recvd, dropped) = a.traffic();
        if sent > 0 && recvd > 0 {
            assert_eq!(
                dropped, 0,
                "no loss was injected, yet {dropped} were dropped"
            );
            return;
        }
        tokio::time::sleep(FAST).await;
    }
    panic!("no gossip bytes were counted in 100 periods");
}

#[tokio::test]
async fn total_loss_stops_the_bytes_but_not_the_node() {
    // ⚠️ Loss is applied OUTBOUND only. Dropping inbound would count the bytes before
    // discarding them and inflate every traffic figure by exactly the loss rate.
    let m = swim::start(
        &format!("127.0.0.1:{}", free_port()),
        &format!("127.0.0.1:{}", free_port()),
        &[format!("127.0.0.1:{}", free_port())],
        1.0,
        FAST,
    )
    .await
    .expect("a member must start under total loss");

    for _ in 0..20 {
        tokio::time::sleep(FAST).await;
    }
    let (sent, _, dropped) = m.traffic();
    assert_eq!(sent, 0, "{sent} bytes reached the wire under total loss");
    assert!(dropped > 0, "total loss dropped nothing at all");
}
