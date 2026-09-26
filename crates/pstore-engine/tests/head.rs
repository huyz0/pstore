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
    // M9c: a segment's delete vector.
    h.deletes
        .insert("a/1.seg".to_owned(), ("a/1.seg.dv".to_owned(), 3));
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
fn a_truncated_head_is_refused_at_every_length_but_a_section_boundary() {
    // A partially-decoded HEAD would be a partially-visible index: some segments present,
    // others silently absent, reported as success.
    //
    // ⚠️ **Four cuts now decode, and they are named rather than tolerated.** M7d added the
    // schema and reject sections as OPTIONAL trailing ones, M7e added the reap horizon and
    // M9c the delete vectors,
    // because every HEAD written before each of them ends where it ends and reading that as
    // corrupt would make every existing store unreadable. The price is that a HEAD truncated
    // *exactly* at one of those boundaries is indistinguishable from an older, complete one.
    //
    // ⚠️ Enumerating **every** cut is what keeps this a stated trade rather than a growing
    // one: one more decodable offset than there are optional sections means a section stopped
    // being checked. It also proves what is NOT tolerated -- a HEAD cut one to seven bytes
    // into the horizon is absent from this list, so a short tail is `CorruptHead` and never a
    // horizon of zero, which would refuse every time-travel query.
    let bytes = populated().encode();
    let decodable: Vec<usize> = (0..bytes.len())
        .filter(|cut| Head::decode(&bytes[..*cut]).is_ok())
        .collect();
    let no_schemas = encode_without_schemas(&populated()).len();
    assert_eq!(
        decodable,
        vec![no_schemas, no_schemas + 4, no_schemas + 8, no_schemas + 16],
        "the only decodable truncations must be the four optional-section boundaries"
    );
    // And what they decode to is the older HEAD, not a partial one.
    let at_boundary = Head::decode(&bytes[..no_schemas]).unwrap();
    assert_eq!(at_boundary.indexes, populated().indexes);
    assert!(at_boundary.schemas.is_empty());
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

/// ⚠️ **M7d.1, and the criterion that breaks a deployment rather than a test.** Every HEAD in
/// every existing store was written before the schema section existed, so the decoder must
/// read the end of the buffer at that boundary as "no schemas recorded" — never as a decode
/// error, and never as a schema of zero, which would refuse every later fold.
///
/// Asserted against **bytes built without the section**, not against a round trip through the
/// new encoder, which would pass however the boundary is handled.
#[test]
fn a_head_without_a_schema_section_decodes_as_no_schemas() {
    let old = encode_without_schemas(&populated());
    let decoded = Head::decode(&old).expect("a HEAD from the old encoder must still decode");
    let expected = populated();
    assert_eq!(decoded.epoch, expected.epoch);
    assert_eq!(decoded.indexes, expected.indexes);
    assert_eq!(decoded.watermarks, expected.watermarks);
    assert!(
        decoded.schemas.is_empty(),
        "an absent section is no schemas, not a schema of zero: {:?}",
        decoded.schemas
    );
    assert!(decoded.schema_rejects.is_empty());
}

#[test]
fn a_schema_round_trips_with_its_reject_count() {
    let mut h = populated();
    h.schemas.insert(
        "alpha".to_owned(),
        pstore_engine::IndexSchema {
            dims: 384,
            text_field: "body".to_owned(),
        },
    );
    h.schemas.insert(
        "beta".to_owned(),
        pstore_engine::IndexSchema {
            dims: 4,
            text_field: String::new(),
        },
    );
    h.schema_rejects.insert("alpha".to_owned(), 17);
    let back = Head::decode(&h.encode()).unwrap();
    assert_eq!(back, h);
    // ⚠️ And the section really is trailing: an old decoder reads everything before it, which
    // is what makes this a compatible addition rather than a format break.
    assert!(h.encode().starts_with(&encode_without_schemas(&h)));
}

/// HEAD as the encoder wrote it before M7d: everything up to and including the graveyard.
fn encode_without_schemas(h: &Head) -> Vec<u8> {
    fn put_str(out: &mut Vec<u8>, s: &str) {
        out.extend_from_slice(&(s.len() as u32).to_le_bytes());
        out.extend_from_slice(s.as_bytes());
    }
    let mut out = Vec::new();
    out.extend_from_slice(&h.epoch.0.to_le_bytes());
    out.extend_from_slice(&h.nonce.to_le_bytes());
    out.extend_from_slice(&(h.indexes.len() as u32).to_le_bytes());
    for (name, segs) in &h.indexes {
        put_str(&mut out, name);
        out.extend_from_slice(&(segs.len() as u32).to_le_bytes());
        for s in segs {
            put_str(&mut out, &s.key);
            out.extend_from_slice(&s.rows.to_le_bytes());
        }
    }
    out.extend_from_slice(&(h.watermarks.len() as u32).to_le_bytes());
    for (lane, seq) in &h.watermarks {
        out.extend_from_slice(&lane.to_le_bytes());
        out.extend_from_slice(&seq.to_le_bytes());
    }
    out.extend_from_slice(&(h.graveyard.len() as u32).to_le_bytes());
    for (epoch, keys) in &h.graveyard {
        out.extend_from_slice(&epoch.to_le_bytes());
        out.extend_from_slice(&(keys.len() as u32).to_le_bytes());
        for k in keys {
            put_str(&mut out, k);
        }
    }
    out
}

/// ⚠️ M7e's section, on M7d's mechanism: a HEAD written before the horizon existed reads back
/// with a horizon of **zero**, which is "nothing has been reaped" and therefore refuses
/// nothing. Decoding it as an error would make every existing store unreadable; decoding a
/// short tail as zero would be worse, because it would look like a working store that has
/// silently lost its bound.
#[test]
fn a_head_without_a_reaped_marker_decodes_as_zero() {
    let mut h = populated();
    h.schemas.insert(
        "alpha".to_owned(),
        pstore_engine::IndexSchema {
            dims: 4,
            text_field: String::new(),
        },
    );
    h.reaped_before = 77;
    // The horizon as the LAST section, as it was written before M9c.
    h.deletes.clear();
    let full = &h.encode()[..h.encode().len() - 4];
    let without = &full[..full.len() - 8];
    let decoded = Head::decode(without).expect("a HEAD from before the horizon must decode");
    assert_eq!(decoded.reaped_before, 0);
    assert_eq!(
        decoded.schemas, h.schemas,
        "the sections before it still read"
    );
    // And a short tail is refused rather than read as zero.
    for cut in 1..8 {
        assert!(
            Head::decode(&full[..full.len() - cut]).is_err(),
            "a horizon cut short by {cut} bytes decoded"
        );
    }
    assert_eq!(Head::decode(full).unwrap().reaped_before, 77);
}

#[test]
fn a_head_without_delete_vectors_decodes_as_none() {
    // M9c's trailing section: a HEAD written before it ends at the horizon.
    let h = populated();
    let full = h.encode();
    let mut older = h.clone();
    older.deletes.clear();
    let without = &older.encode()[..older.encode().len() - 4];
    let decoded = Head::decode(without).expect("a HEAD from before M9c must decode");
    assert!(decoded.deletes.is_empty());
    assert_eq!(decoded.indexes, h.indexes);
    // A section cut anywhere inside is refused, never read as fewer vectors.
    let start = without.len();
    for cut in start + 1..full.len() {
        assert!(Head::decode(&full[..cut]).is_err(), "cut at {cut} decoded");
    }
    assert_eq!(Head::decode(&full).unwrap().deletes, h.deletes);
}
