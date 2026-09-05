//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism, so the
//! workspace-wide bans on unwrap/expect/panic are lifted here and only here.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! HEAD is the one mutable object in the system, so its encoding must survive a round
//! trip exactly and refuse anything it does not understand.
use pstore_engine::{Head, SegmentRef};
use pstore_types::{Epoch, LaneId, Seq, TenantId};

fn populated() -> Head {
    let mut h = Head {
        epoch: Epoch(42),
        nonce: 0xDEAD_BEEF_CAFE_1234,
        ..Head::default()
    };
    h.indexes.insert(
        "alpha".to_owned(),
        vec![
            SegmentRef {
                key: "a/1.seg".to_owned(),
                rows: 100,
            },
            SegmentRef {
                key: "a/2.seg".to_owned(),
                rows: 7,
            },
        ],
    );
    h.indexes.insert("beta".to_owned(), vec![]);
    h.watermarks.insert(1, 9);
    h.watermarks.insert(77, 0);
    h
}

#[test]
fn head_round_trips_exactly() {
    // Every field, not just a length: losing the nonce would silently disable the ABA
    // guard, and losing a watermark would replay folded rows.
    let h = populated();
    assert_eq!(Head::decode(&h.encode()).unwrap(), h);
}

#[test]
fn an_empty_head_round_trips() {
    let h = Head::default();
    assert_eq!(Head::decode(&h.encode()).unwrap(), h);
    assert_eq!(h.epoch, Epoch::ZERO);
}

#[test]
fn a_truncated_head_is_refused_at_every_length() {
    // A partially-decoded HEAD would be a partially-visible index: some segments present,
    // others silently absent, reported as success.
    let bytes = populated().encode();
    for cut in 0..bytes.len() {
        assert!(
            Head::decode(&bytes[..cut]).is_err(),
            "decoded a HEAD cut to {cut} bytes"
        );
    }
    assert!(Head::decode(&bytes).is_ok());
}

#[test]
fn a_head_with_a_bad_string_length_is_refused() {
    let mut bytes = populated().encode();
    // The first index-name length lives just after epoch, nonce and the index count.
    let at = 8 + 8 + 4;
    bytes[at..at + 4].copy_from_slice(&u32::MAX.to_le_bytes());
    assert!(Head::decode(&bytes).is_err());
}

#[test]
fn the_watermark_says_what_has_been_folded() {
    // Off by one here either replays folded rows (duplicates) or skips unfolded ones
    // (silent loss), so the boundary is asserted rather than assumed.
    let mut h = Head::default();
    h.watermarks.insert(1, 3);
    assert!(h.is_folded(LaneId(1), Seq(0)));
    assert!(h.is_folded(LaneId(1), Seq(2)));
    assert!(
        !h.is_folded(LaneId(1), Seq(3)),
        "seq 3 is the next unfolded one"
    );
    assert!(!h.is_folded(LaneId(1), Seq(9)));
    assert!(
        !h.is_folded(LaneId(2), Seq(0)),
        "an unknown lane has folded nothing"
    );
}

#[test]
fn the_head_key_is_derived_from_the_tenant_alone() {
    // No lookup, no listing, no catalog on the hot path: the key IS the identifier.
    let k = Head::key(TenantId(7));
    assert!(k.as_str().contains("/tnt/7/HEAD"), "got {k}");
    assert_ne!(Head::key(TenantId(7)), Head::key(TenantId(8)));
    // Deterministic, so two processes derive the same key without agreeing on anything.
    assert_eq!(Head::key(TenantId(7)), Head::key(TenantId(7)));
}
