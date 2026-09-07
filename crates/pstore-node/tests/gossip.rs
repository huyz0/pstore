//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "assertions in tests are the reporting mechanism"
)]

//! Two real gossip members, on real sockets, in one process.
//!
//! ⚠️ This is an *integration test of an adopted library*, and the ledger says so: D-4
//! specifies SWIM+Lifeguard and this is `chitchat` (Scuttlebutt + phi-accrual), chosen
//! because `foca` is MPL-2.0 and outside the licence allow-list. What is being checked is
//! the seam we own — that a member advertises the address peers will reach it on, that its
//! own view includes itself, and that `dial` reaches a peer it was never seeded with.
//!
//! ⚠️ Real UDP, not a mock. The bug this seam actually shipped was that `SocketAddr::parse`
//! was called on a container hostname, and every node exited at startup; a mocked transport
//! would have been perfectly happy.

use pstore_node::gossip;

/// A port nothing is listening on, claimed by binding and releasing it.
///
/// ⚠️ Not a fixed constant, and the reason is that these tests do not run alone. `cargo
/// mutants` builds and tests many mutants **concurrently**, each in a separate copy of the
/// tree — fixed ports made those copies fight over the same sockets, which surfaced as tests
/// that HUNG rather than failed, and as mutants recorded `timeout` when the suite would
/// otherwise have caught them in five seconds.
///
/// There is a race between releasing and rebinding, and it is unavoidable: a gossip member
/// must ADVERTISE an address before it binds one, because that is what a peer dials. The
/// window is microseconds; the alternative is a guaranteed collision.
fn free_port() -> u16 {
    std::net::UdpSocket::bind("127.0.0.1:0")
        .expect("the loopback must have a spare port")
        .local_addr()
        .expect("a bound socket has an address")
        .port()
}

async fn member(port: u16, seeds: &[String]) -> gossip::Member {
    gossip::start(
        &format!("node-{port}"),
        &format!("127.0.0.1:{port}"),
        &format!("127.0.0.1:{port}"),
        seeds,
        0.0,
        pstore_node::DEFAULT_GOSSIP_PERIOD,
    )
    .await
    .expect("a member must start on loopback")
}

#[tokio::test]
async fn a_member_advertises_the_address_a_peer_would_dial() {
    // ⚠️ The bug this pins: chitchat advertises a RESOLVED `SocketAddr`, so a node that
    // compares its configured name against its own gossip view never finds itself. Measured
    // on a live fleet as "every node owns 0 of 1000 shards" — a placement bug, apparently,
    // and a naming one in fact.
    let a = free_port();
    let m = member(a, &[]).await;
    assert_eq!(m.self_addr(), format!("127.0.0.1:{a}"));
    assert!(
        m.members().await.contains(&m.self_addr()),
        "a member's own view must include itself, or every node's view is one short and \
         convergence can never be observed to complete"
    );
}

#[tokio::test]
async fn two_members_find_each_other_from_a_seed() {
    let (pa, pb) = (free_port(), free_port());
    let a = member(pa, &[]).await;
    let b = member(pb, &[format!("127.0.0.1:{pa}")]).await;

    let mut seen = false;
    for _ in 0..50 {
        if a.members().await.len() == 2 && b.members().await.len() == 2 {
            seen = true;
            break;
        }
        tokio::time::sleep(pstore_node::DEFAULT_GOSSIP_PERIOD).await;
    }
    assert!(seen, "two seeded members never converged within 50 periods");
    assert!(a.members().await.contains(&b.self_addr()));
}

#[tokio::test]
async fn dialling_a_peer_that_does_not_answer_is_not_an_error() {
    // The backstop dials whatever the roster lists, including entries for nodes that have
    // since died. A dial that returned an error there would turn a stale seed list into a
    // failing heal loop.
    let m = member(free_port(), &[]).await;
    assert!(
        m.dial(&format!("127.0.0.1:{}", free_port())),
        "dialling an address nobody is listening on must still be accepted"
    );
    assert!(
        !m.dial("not-an-address"),
        "an unparseable address must be reported, not silently dialled"
    );
}

#[tokio::test]
async fn traffic_is_counted_from_the_first_round() {
    let (pa, pb) = (free_port(), free_port());
    let a = member(pa, &[]).await;
    let _b = member(pb, &[format!("127.0.0.1:{pa}")]).await;
    let mut moved = false;
    for _ in 0..50 {
        let (sent, recvd, dropped) = a.traffic();
        if sent > 0 && recvd > 0 {
            assert_eq!(
                dropped, 0,
                "no loss was injected, yet {dropped} were dropped"
            );
            moved = true;
            break;
        }
        tokio::time::sleep(pstore_node::DEFAULT_GOSSIP_PERIOD).await;
    }
    assert!(moved, "no gossip bytes were counted in 50 periods");
}
