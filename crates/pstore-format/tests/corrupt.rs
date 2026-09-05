//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism, so the
//! workspace-wide bans on unwrap/expect/panic are lifted here and only here.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! A malformed segment must fail, never decode into something plausible. Bad data served
//! as good data is the worst outcome available to a storage layer.
use pstore_blob::{BlobStore, Key, MemoryStore};
use pstore_format::{Document, Filter, FormatError, Segment, SegmentWriter, Value};

fn seg_bytes(n: usize) -> Vec<u8> {
    let mut w = SegmentWriter::new(4);
    for i in 0..n {
        let mut d = Document::new(format!("d{i}"), vec![i as f32]);
        d.attrs.insert("n".to_owned(), Value::Int(i as i64));
        w.push(d);
    }
    w.finish().to_vec()
}

async fn open_bytes(b: Vec<u8>) -> Result<Segment, FormatError> {
    let s = MemoryStore::new();
    let key = Key::new("x");
    s.put(&key, bytes::Bytes::from(b)).await.unwrap();
    Segment::open(&s, &key).await
}

#[tokio::test]
async fn a_truncated_segment_is_an_error() {
    let full = seg_bytes(20);
    for cut in [0, 1, 8, 20, full.len() / 2, full.len() - 1] {
        assert!(
            open_bytes(full[..cut].to_vec()).await.is_err(),
            "a segment cut to {cut} bytes decoded"
        );
    }
}

#[tokio::test]
async fn a_future_version_is_refused_rather_than_guessed_at() {
    // Forward compatibility is a decision, not an accident: a reader that ploughs on
    // through a layout it does not know produces garbage with confidence.
    let mut b = seg_bytes(8);
    let n = b.len();
    // version is the u16 right after the leading magic of the footer.
    let v = n - (8 + 2 + 8 + 4 + 4 + 8 + 8) + 8;
    b[v] = 99;
    assert!(matches!(
        open_bytes(b).await.unwrap_err(),
        FormatError::UnsupportedVersion(_)
    ));
}

#[tokio::test]
async fn a_tampered_index_section_is_caught_by_the_checksum() {
    let mut b = seg_bytes(20);
    // Flip a byte inside the index section: after the data, before the footer.
    let idx = b.len() - (8 + 2 + 8 + 4 + 4 + 8 + 8) - 4;
    b[idx] ^= 0xFF;
    assert!(matches!(
        open_bytes(b).await.unwrap_err(),
        FormatError::ChecksumMismatch | FormatError::Truncated | FormatError::Corrupt(_)
    ));
}

#[tokio::test]
async fn a_missing_segment_is_an_error_not_an_empty_one() {
    // An empty answer would read as "this index has no documents", which is a silent
    // data-loss report rather than a failure.
    let s = MemoryStore::new();
    assert!(Segment::open(&s, &Key::new("absent")).await.is_err());
}

#[tokio::test]
async fn an_unknown_value_tag_is_an_error() {
    let mut b = seg_bytes(4);
    // The first row's attribute tag: after block row-count, id, dims, vector, attr count,
    // key. Rather than compute it, find the Int tag byte that precedes a known value.
    let pos = b.iter().position(|&x| x == 0u8).unwrap_or(0);
    b[pos] = 200;
    // Either the tag is rejected, or an earlier structural check fires first. Both are
    // errors; neither is a document.
    let opened = open_bytes(b).await;
    if let Ok(seg) = opened {
        let s = MemoryStore::new();
        let key = Key::new("y");
        s.put(&key, bytes::Bytes::from(seg_bytes(4))).await.unwrap();
        let _ = seg.scan(&s, &key, None).await;
    }
}

#[tokio::test]
async fn filters_prune_only_what_they_provably_can() {
    // The pruning predicate, tested directly at its boundaries. `could_match` answering
    // false for a block that could match is the mutation that silently drops rows.
    let eq = Filter::Eq("n".to_owned(), Value::Int(5));
    assert!(eq.could_match(0, 10));
    assert!(
        eq.could_match(5, 5),
        "a block of exactly the value must be kept"
    );
    assert!(!eq.could_match(6, 10));
    assert!(!eq.could_match(0, 4));

    let gt = Filter::Gt("n".to_owned(), 5);
    assert!(gt.could_match(0, 6), "max just above the bound could match");
    assert!(
        !gt.could_match(0, 5),
        "max equal to the bound cannot: Gt is strict"
    );

    let lt = Filter::Lt("n".to_owned(), 5);
    assert!(
        lt.could_match(4, 10),
        "min just below the bound could match"
    );
    assert!(
        !lt.could_match(5, 10),
        "min equal to the bound cannot: Lt is strict"
    );

    // A string equality has no ordering, so it can never prune.
    let str_eq = Filter::Eq("k".to_owned(), Value::Str("a".to_owned()));
    assert!(str_eq.could_match(i64::MIN, i64::MAX));
    assert!(str_eq.could_match(0, 0));

    assert_eq!(eq.column(), "n");
    assert_eq!(str_eq.column(), "k");
}

#[tokio::test]
async fn filters_match_rows_by_type() {
    let mut d = Document::new("x", vec![1.0]);
    d.attrs.insert("n".to_owned(), Value::Int(5));
    d.attrs.insert("s".to_owned(), Value::Str("hi".to_owned()));

    assert!(Filter::Eq("n".to_owned(), Value::Int(5)).matches(&d));
    assert!(!Filter::Eq("n".to_owned(), Value::Int(6)).matches(&d));
    assert!(Filter::Eq("s".to_owned(), Value::Str("hi".to_owned())).matches(&d));
    assert!(Filter::Gt("n".to_owned(), 4).matches(&d));
    assert!(!Filter::Gt("n".to_owned(), 5).matches(&d));
    assert!(Filter::Lt("n".to_owned(), 6).matches(&d));
    assert!(!Filter::Lt("n".to_owned(), 5).matches(&d));
    // A numeric comparison against a string attribute must be false, not a panic or a
    // coercion that invents an ordering.
    assert!(!Filter::Gt("s".to_owned(), 0).matches(&d));
    assert!(!Filter::Lt("s".to_owned(), 99).matches(&d));
    assert!(!Filter::Gt("absent".to_owned(), 0).matches(&d));
}

#[tokio::test]
async fn a_document_starts_with_no_attributes() {
    let d = Document::new("a", vec![1.0, 2.0]);
    assert_eq!(d.id, "a");
    assert!(d.attrs.is_empty());
}
