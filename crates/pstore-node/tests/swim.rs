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

use pstore_node::seal::{self, Header, Keys};
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
        "az-a",
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
    let r = swim::start("not-a-socket-addr", "127.0.0.1:1", "az-a", &[], 0.0, FAST).await;
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
        "az-a",
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

#[tokio::test]
async fn a_member_reports_its_own_zone() {
    // ⚠️ The zone is what makes a cell's roster a cell's roster: `main` keeps only peers whose
    // zone matches its own. Pinned through the member's OWN entry, which is deterministic;
    // `two_members_in_two_zones_learn_each_others_zone` pins a peer's (M24).
    // `contains`, not equality: a reused test port can deliver a stray probe.
    let port = free_port();
    let m = swim::start(
        &format!("127.0.0.1:{port}"),
        &format!("127.0.0.1:{port}"),
        "az-q",
        &[],
        0.0,
        FAST,
    )
    .await
    .expect("a member must start on loopback");
    let zoned = m.members_zoned().await;
    assert!(
        zoned.contains(&(m.self_addr(), "az-q".to_owned())),
        "a member in az-q did not report its own zone: {zoned:?}"
    );
}

#[tokio::test]
async fn two_members_in_two_zones_learn_each_others_zone() {
    // M24: a peer first learned from a seed has an empty zone, and its own record now fills
    // it -- before M24 it never did, and each was missing from the other's cell roster.
    let (pa, pb) = (free_port(), free_port());
    let (a, b) = (format!("127.0.0.1:{pa}"), format!("127.0.0.1:{pb}"));
    let ma = swim::start(&a, &a, "az-a", std::slice::from_ref(&b), 0.0, FAST)
        .await
        .expect("a member must start on loopback");
    let mb = swim::start(&b, &b, "az-b", std::slice::from_ref(&a), 0.0, FAST)
        .await
        .expect("a member must start on loopback");
    for _ in 0..200 {
        let (za, zb) = (ma.members_zoned().await, mb.members_zoned().await);
        if za.contains(&(b.clone(), "az-b".to_owned()))
            && zb.contains(&(a.clone(), "az-a".to_owned()))
        {
            return;
        }
        tokio::time::sleep(FAST).await;
    }
    panic!(
        "zones not learned: {:?} / {:?}",
        ma.members_zoned().await,
        mb.members_zoned().await
    );
}

// ---- M53: sealed gossip ------------------------------------------------------------------

/// Keys of 32 bytes each, every byte the given one: `keyed(&[8, 7])` seals with 8.
fn keyed(k: &[u8]) -> Option<Keys> {
    Keys::new(&k.iter().map(|b| vec![*b; 32]).collect::<Vec<_>>())
}

async fn member_keyed(port: u16, seeds: &[String], keys: Option<Keys>) -> swim::Member {
    let addr = format!("127.0.0.1:{port}");
    swim::start_with(&addr, &addr, "az-a", seeds, 0.0, FAST, keys)
        .await
        .expect("a member must start on loopback")
}

/// Whether `a` and `b` both count two members within 100 periods.
async fn meet(a: &swim::Member, b: &swim::Member) -> bool {
    for _ in 0..100 {
        if a.member_count().await == 2 && b.member_count().await == 2 {
            return true;
        }
        tokio::time::sleep(FAST).await;
    }
    false
}

/// Whether `a` stays alone for 2 s, refusing what it is sent.
async fn stays_alone(a: &swim::Member) -> bool {
    for _ in 0..40 {
        if a.member_count().await != 1 {
            return false;
        }
        tokio::time::sleep(FAST).await;
    }
    true
}

#[tokio::test]
async fn keyed_members_find_each_other() {
    let (pa, pb) = (free_port(), free_port());
    let a = member_keyed(pa, &[], keyed(&[7])).await;
    let b = member_keyed(pb, &[format!("127.0.0.1:{pa}")], keyed(&[7])).await;
    assert!(meet(&a, &b).await, "keyed members never met");
    assert_eq!(a.refused(), 0);
    assert_eq!(b.refused(), 0);
}

#[tokio::test]
async fn a_member_with_another_key_is_refused() {
    let (pa, pb) = (free_port(), free_port());
    let a = member_keyed(pa, &[], keyed(&[7])).await;
    let _b = member_keyed(pb, &[format!("127.0.0.1:{pa}")], keyed(&[8])).await;
    assert!(stays_alone(&a).await, "a member with another key joined");
    assert!(a.refused() > 0, "nothing was refused");
}

#[tokio::test]
async fn keyed_and_unkeyed_members_refuse_each_other() {
    let (pa, pb) = (free_port(), free_port());
    let a = member_keyed(pa, &[], keyed(&[7])).await;
    let _b = member_keyed(pb, &[format!("127.0.0.1:{pa}")], None).await;
    assert!(
        stays_alone(&a).await,
        "an unkeyed member joined a keyed one"
    );
    assert!(a.refused() > 0, "nothing was refused");

    let (pc, pd) = (free_port(), free_port());
    let c = member_keyed(pc, &[], None).await;
    let _d = member_keyed(pd, &[format!("127.0.0.1:{pc}")], keyed(&[7])).await;
    assert!(
        stays_alone(&c).await,
        "a keyed member joined an unkeyed one"
    );
}

#[tokio::test]
async fn a_second_key_lets_a_rotation_roll() {
    // Pass 1: the new key added second, beside a node that holds only the old.
    let (pa, pb) = (free_port(), free_port());
    let a = member_keyed(pa, &[], keyed(&[7, 8])).await;
    let b = member_keyed(pb, &[format!("127.0.0.1:{pa}")], keyed(&[7])).await;
    assert!(meet(&a, &b).await, "pass 1 split the fleet");
    // Pass 2: one node already seals with the new key, the other still with the old, and each
    // opens the other's with its second key.
    let (pa, pb) = (free_port(), free_port());
    let a = member_keyed(pa, &[], keyed(&[8, 7])).await;
    let b = member_keyed(pb, &[format!("127.0.0.1:{pa}")], keyed(&[7, 8])).await;
    assert!(meet(&a, &b).await, "a rotating fleet split");
    assert_eq!(a.refused(), 0);
}

#[test]
fn a_reply_is_sealed_for_its_verified_sender() {
    let mut view = pstore_gossip::Cluster::new([0; 16], "10.0.0.1:7946".to_owned(), "z".to_owned());
    view.join([5; 16], "10.0.0.5:7946".to_owned(), "z".to_owned());
    // A reply to the source of a datagram sealed by S is for S, whatever that address says.
    let s = [9; 16];
    assert_eq!(
        swim::dest_for("192.0.2.1:1", Some(("192.0.2.1:1", s)), &view),
        s
    );
    // Anything else: the member the view holds there, else the id the address derives.
    assert_eq!(
        swim::dest_for("10.0.0.5:7946", Some(("192.0.2.1:1", s)), &view),
        [5; 16]
    );
    assert_eq!(swim::dest_for("10.0.0.5:7946", None, &view), [5; 16]);
    let unknown = swim::dest_for("10.0.0.6:7946", None, &view);
    assert_ne!(unknown, [0; 16]);
    assert_ne!(unknown, [5; 16]);
    let empty = pstore_gossip::Cluster::new([0; 16], "x".to_owned(), "z".to_owned());
    assert_eq!(
        swim::dest_for("10.0.0.6:7946", None, &empty),
        unknown,
        "derived, not looked up"
    );
    // Two ids at one address, the lower declared dead: the live one (code review).
    view.join([3; 16], "10.0.0.5:7946".to_owned(), "z".to_owned());
    view.declare_dead(&[3; 16]);
    assert_eq!(swim::dest_for("10.0.0.5:7946", None, &view), [5; 16]);
    // Both dead: still the one there, not a derived id nobody holds.
    view.declare_dead(&[5; 16]);
    assert_eq!(swim::dest_for("10.0.0.5:7946", None, &view), [3; 16]);
}

#[tokio::test]
async fn the_largest_sealed_part_is_received() {
    // ⚠️ A receive buffer of `MAX_DATAGRAM` truncates this, and its seal fails (spec review).
    let port = free_port();
    let addr = format!("127.0.0.1:{port}");
    let a = member_keyed(port, &[], keyed(&[7])).await;
    let empty = pstore_gossip::Cluster::new([0; 16], "x".to_owned(), "z".to_owned());
    let dest = swim::dest_for(&addr, None, &empty);
    // A `Part` of exactly `MAX_DATAGRAM` bytes: one member whose address fills it.
    let part = |len: usize| pstore_gossip::Message::Part {
        from: [3; 16],
        members: vec![pstore_gossip::Member {
            addr: "a".repeat(len),
            ..pstore_gossip::Cluster::new([4; 16], String::new(), "z".to_owned())
                .members()
                .next()
                .expect("a cluster holds itself")
                .clone()
        }],
    };
    let base = part(0).encode().len();
    let msg = part(pstore_gossip::MAX_DATAGRAM - base);
    let payload = msg.encode();
    assert_eq!(payload.len(), pstore_gossip::MAX_DATAGRAM);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_micros() as u64;
    let h = Header {
        sealer: [3; 16],
        dest,
        epoch: now,
        counter: 0,
        time: now,
    };
    let frame = seal::seal(keyed(&[7]).as_ref().unwrap(), &payload, &h);
    assert_eq!(frame.len(), 65_507);
    let raw = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    raw.send_to(&frame, &addr).unwrap();
    // Waits for the `Part`'s own member by address: counting members would see the sender
    // learned too, and then each probed to death (code review).
    let long = "a".repeat(pstore_gossip::MAX_DATAGRAM - base);
    for _ in 0..100 {
        if a.members().await.contains(&long) {
            assert_eq!(a.refused(), 0);
            return;
        }
        tokio::time::sleep(FAST).await;
    }
    panic!(
        "the largest sealed datagram never arrived: {} refused",
        a.refused()
    );
}

#[tokio::test]
async fn a_frame_sealed_before_the_node_started_is_refused() {
    // Code review: a node keeps its id across a restart, and its replay table does not, so a
    // frame captured in the minute before must not be believed after.
    let micros = || {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_micros() as u64
    };
    let before = micros();
    tokio::time::sleep(Duration::from_millis(5)).await;
    let port = free_port();
    let addr = format!("127.0.0.1:{port}");
    let a = member_keyed(port, &[], keyed(&[7])).await;
    let empty = pstore_gossip::Cluster::new([0; 16], "x".to_owned(), "z".to_owned());
    let msg = pstore_gossip::Message::Part {
        from: [3; 16],
        members: vec![
            pstore_gossip::Cluster::new([4; 16], "10.9.9.9:1".to_owned(), "z".to_owned())
                .members()
                .next()
                .expect("a cluster holds itself")
                .clone(),
        ],
    };
    let raw = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let send = |time: u64, counter: u64| {
        let h = Header {
            sealer: [3; 16],
            dest: swim::dest_for(&addr, None, &empty),
            epoch: before,
            counter,
            time,
        };
        let frame = seal::seal(keyed(&[7]).as_ref().unwrap(), &msg.encode(), &h);
        raw.send_to(&frame, &addr).unwrap();
    };
    send(before, 0);
    for _ in 0..20 {
        if a.refused() == 1 {
            break;
        }
        tokio::time::sleep(FAST).await;
    }
    assert_eq!(
        a.refused(),
        1,
        "a frame sealed before the node started was not refused"
    );
    assert!(!a.members().await.contains(&"10.9.9.9:1".to_owned()));
    // The same, sealed now, is believed.
    send(micros(), 1);
    for _ in 0..100 {
        if a.members().await.contains(&"10.9.9.9:1".to_owned()) {
            return;
        }
        tokio::time::sleep(FAST).await;
    }
    panic!("a fresh frame was refused: {} refused", a.refused());
}

const PERIOD: Duration = Duration::from_millis(100);

async fn member_at(port: u16, zone: &str, seeds: &[String]) -> swim::Member {
    let at = format!("127.0.0.1:{port}");
    // Sealed, as a server's member is (M56 AC3).
    swim::start_with(&at, &at, zone, seeds, 0.0, PERIOD, keyed(&[8]))
        .await
        .expect("a member must start on loopback")
}

#[tokio::test]
async fn a_stopped_member_falls_silent_and_is_declared_dead() {
    // M56.1: a member's tasks ran detached, so a server that stopped serving went on answering
    // probes for as long as its process lived.
    let (a_port, b_port) = (free_port(), free_port());
    let a = member_at(a_port, "az-a", &[]).await;
    let b = member_at(b_port, "az-a", &[format!("127.0.0.1:{a_port}")]).await;
    let mut met = false;
    for _ in 0..40 {
        if a.member_count().await == 2 {
            met = true;
            break;
        }
        tokio::time::sleep(PERIOD).await;
    }
    assert!(met, "the two never met");
    b.stop().await;
    // Let anything in flight land, then the counters must not move again.
    tokio::time::sleep(PERIOD * 2).await;
    let (sent, recvd, _) = b.traffic();
    let mut gone = false;
    for _ in 0..40 {
        if a.member_count().await == 1 {
            gone = true;
            break;
        }
        tokio::time::sleep(PERIOD).await;
    }
    assert!(
        gone,
        "a stopped member was still counted alive after 40 periods"
    );
    tokio::time::sleep(PERIOD * 10).await;
    let (sent2, recvd2, _) = b.traffic();
    assert_eq!(
        (sent2, recvd2),
        (sent, recvd),
        "a stopped member still sent or received"
    );
    std::net::UdpSocket::bind(format!("127.0.0.1:{b_port}"))
        .expect("a stopped member's port must be free again");
}

#[tokio::test]
async fn a_member_restarted_with_a_new_zone_is_believed() {
    // M56.1: a restart began at incarnation 0, which ties the record the others hold, and a
    // zone never moves at an equal incarnation -- so a server restarted under a new URL kept
    // its old one in every other view until something suspected it.
    let (a_port, b_port) = (free_port(), free_port());
    let a = member_at(a_port, "az-a", &[]).await;
    let seeds = [format!("127.0.0.1:{a_port}")];
    let b = member_at(b_port, "az-old", &seeds).await;
    let zone_of_b = |zoned: Vec<(String, String)>| {
        zoned
            .into_iter()
            .find(|(addr, _)| *addr == format!("127.0.0.1:{b_port}"))
            .map(|(_, z)| z)
    };
    let mut met = false;
    for _ in 0..40 {
        if zone_of_b(a.members_zoned().await).as_deref() == Some("az-old") {
            met = true;
            break;
        }
        tokio::time::sleep(PERIOD).await;
    }
    assert!(met, "a never learned b's first zone");
    b.stop().await;
    drop(b);
    // At once, well inside the time a would take to suspect it.
    let b = member_at(b_port, "az-new", &seeds).await;
    let mut moved = false;
    for _ in 0..40 {
        if zone_of_b(a.members_zoned().await).as_deref() == Some("az-new") {
            moved = true;
            break;
        }
        tokio::time::sleep(PERIOD).await;
    }
    assert!(moved, "a still holds b's old zone after 40 periods");
    drop(b);
}

#[tokio::test]
async fn a_dropped_member_falls_silent_too() {
    // M56.1, from the sweep: a member dropped without `stop` ends its tasks as well, or a
    // server that drops its handle on an error path would answer probes for ever.
    let (a_port, b_port) = (free_port(), free_port());
    let a = member_at(a_port, "az-a", &[]).await;
    let b = member_at(b_port, "az-a", &[format!("127.0.0.1:{a_port}")]).await;
    let mut met = false;
    for _ in 0..40 {
        if a.member_count().await == 2 {
            met = true;
            break;
        }
        tokio::time::sleep(PERIOD).await;
    }
    assert!(met, "the two never met");
    drop(b);
    let mut gone = false;
    for _ in 0..40 {
        if a.member_count().await == 1 {
            gone = true;
            break;
        }
        tokio::time::sleep(PERIOD).await;
    }
    assert!(
        gone,
        "a dropped member was still counted alive after 40 periods"
    );
}
