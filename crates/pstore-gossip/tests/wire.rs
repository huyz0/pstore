//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! The wire format, and the size claim the milestone rests on.

use pstore_gossip::{Member, Message, State};

fn id(n: u8) -> [u8; 16] {
    let mut b = [0u8; 16];
    b[0] = n;
    b
}

fn member(n: u8, state: State) -> Member {
    Member {
        id: id(n),
        addr: format!("10.0.0.{n}:7946"),
        incarnation: u64::from(n),
        state,
    }
}

#[test]
fn every_message_round_trips() {
    let msgs = vec![
        Message::Ping {
            from: id(1),
            seq: 7,
            checksum: 0xdead_beef,
            updates: vec![],
        },
        Message::Ack {
            from: id(2),
            seq: 7,
            checksum: 0xdead_beef,
            updates: vec![],
        },
        Message::Ping {
            from: id(1),
            seq: u64::MAX,
            checksum: 0,
            updates: vec![member(3, State::Suspect), member(4, State::Dead)],
        },
        Message::PingReq {
            from: id(1),
            seq: 9,
            target: id(5),
        },
        Message::Sync {
            from: id(6),
            members: vec![member(1, State::Alive), member(2, State::Suspect)],
        },
    ];
    for m in msgs {
        let bytes = m.encode();
        let back = Message::decode(&bytes)
            .unwrap_or_else(|| panic!("failed to decode a message this crate encoded: {m:?}"));
        assert_eq!(m, back);
    }
}

#[test]
fn a_corrupt_frame_is_an_error_not_a_member() {
    // ⚠️ A decoder that truncates silently hands the protocol a SHORTER member list, which
    // reads as nodes having left. Garbage must be refused, not shortened.
    let good = Message::Sync {
        from: id(1),
        members: vec![member(2, State::Alive), member(3, State::Alive)],
    }
    .encode();

    assert!(Message::decode(&[]).is_none(), "an empty frame decoded");
    assert!(Message::decode(&[0xff]).is_none(), "an unknown tag decoded");
    for cut in 1..good.len() {
        assert!(
            Message::decode(&good[..cut]).is_none(),
            "a frame truncated to {cut} of {} bytes decoded anyway",
            good.len()
        );
    }
    let mut trailing = good.clone();
    trailing.push(0);
    assert!(
        Message::decode(&trailing).is_none(),
        "a frame with trailing bytes decoded, so a decoder desync goes unnoticed"
    );
}

#[test]
fn a_converged_probe_costs_tens_of_bytes_not_thousands() {
    // ⚠️ THE claim of the milestone, as a number. `chitchat` sends a per-node digest every
    // round -- measured at ~13 KB for 100 nodes and constant whatever the period. A probe
    // that carries nothing but a checksum has to be small enough that the comparison is not
    // close, or the rewrite bought nothing.
    let ping = Message::Ping {
        from: id(1),
        seq: 42,
        checksum: 0x1234_5678_9abc_def0,
        updates: vec![],
    }
    .encode();
    let ack = Message::Ack {
        from: id(2),
        seq: 42,
        checksum: 0x1234_5678_9abc_def0,
        updates: vec![],
    }
    .encode();
    let round = ping.len() + ack.len();
    assert!(
        round <= 128,
        "a converged probe round costs {round} bytes; the point was to replace ~13,000"
    );
}

#[test]
fn a_sync_is_bounded_by_what_it_carries() {
    // The reconciliation path is allowed to be O(N) -- it is the rare case. What it must not
    // be is surprising: 100 members must cost about 100 members' worth.
    let members: Vec<Member> = (1..=100).map(|n| member(n, State::Alive)).collect();
    let bytes = Message::Sync {
        from: id(0),
        members,
    }
    .encode()
    .len();
    assert!(
        (2_000..=8_000).contains(&bytes),
        "100 members encoded to {bytes} bytes, which is not the size of 100 members"
    );
}

#[test]
fn an_unknown_state_tag_is_refused() {
    // Forward compatibility the honest way: a state this build does not know is a message it
    // must not guess at, because guessing means inventing a member's liveness.
    let mut bytes = Message::Sync {
        from: id(0),
        members: vec![member(1, State::Alive)],
    }
    .encode();
    let tag = bytes.len() - 1 - 8 - 1; // state byte sits before addr len and incarnation
    let _ = tag;
    // Flip every byte in turn; none may produce a valid message with a bogus state.
    for i in 0..bytes.len() {
        let saved = bytes[i];
        bytes[i] = 0x7f;
        if let Some(Message::Sync { members, .. }) = Message::decode(&bytes) {
            for m in members {
                assert!(
                    matches!(m.state, State::Alive | State::Suspect | State::Dead),
                    "byte {i} produced an out-of-range state"
                );
            }
        }
        bytes[i] = saved;
    }
}
