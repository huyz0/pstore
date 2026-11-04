//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::float_cmp,
    reason = "assertions in tests are the reporting mechanism"
)]

//! M177: a sparse dimension is its key's bytes. No mapping to numbers, so nothing for two
//! writers to agree on and nothing for a hash to merge -- and the encodings written before it,
//! with `u32` dimensions, still read.

use pstore_blob::{BlobStore, Key, MemoryStore};
use pstore_format::sparse::{self, Dictionary, ImpactEncoding};
use pstore_format::{
    Document, Impact, SPARSE_KEY_MAX, Section, Segment, SegmentWriter, VectorField, check_storable,
    decode_docs, encode_docs,
};
use std::collections::{BTreeMap, BTreeSet};

const FIELD: &str = "s";

/// Keys a mapping to numbers would merge, truncate or misorder: spellings of one number, both
/// normal forms of one letter, long keys differing only in their last byte, keys sharing their
/// first 8 bytes, the longest a key may be -- and 70,000 random ones, past the point where a
/// 32-bit hash of them collides more often than not.
fn keys() -> Vec<String> {
    let long = "k".repeat(199);
    let mut out = vec![
        "1".to_owned(),
        "01".to_owned(),
        "1.0".to_owned(),
        "\u{e9}".to_owned(),
        "e\u{301}".to_owned(),
        format!("{long}a"),
        format!("{long}b"),
        "prefix00-x".to_owned(),
        "prefix00-y".to_owned(),
        "z".repeat(SPARSE_KEY_MAX),
    ];
    let mut rng: u64 = 0x9e37_79b9_7f4a_7c15;
    let mut seen: BTreeSet<String> = out.iter().cloned().collect();
    while out.len() < 70_010 {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        let width = 1 + (rng >> 60) as usize;
        let k = format!("{rng:016x}")[..width].to_owned();
        if seen.insert(k.clone()) {
            out.push(k);
        }
    }
    out
}

/// Row `i` names keys `i`, `i + 1` and `i + 7` of `keys`, with weights telling them apart.
fn rows(keys: &[String], n: usize) -> Vec<Document> {
    (0..n)
        .map(|i| {
            let mut pairs: Vec<(String, Impact)> = [0, 1, 7]
                .iter()
                .map(|o| {
                    let k = &keys[(i + o) % keys.len()];
                    #[expect(clippy::cast_precision_loss, reason = "a fixture weight")]
                    let w = 1.0 + *o as f32;
                    (k.clone(), Impact::new(w))
                })
                .collect();
            pairs.sort_by(|a, b| a.0.cmp(&b.0));
            let mut d = Document::new(format!("r{i:05}"), Vec::new());
            d.vectors.insert(FIELD.into(), VectorField::Sparse(pairs));
            d
        })
        .collect()
}

/// A segment of `docs`, F32, with its dictionary beside it.
async fn put(docs: &[Document]) -> (MemoryStore, Key, Dictionary, Vec<u8>) {
    let p = sparse::build(docs, FIELD, ImpactEncoding::F32);
    let dict = Dictionary::decode(&p.dictionary).unwrap();
    let section = p.section.clone();
    let mut w = SegmentWriter::new(64);
    for d in docs {
        w.push(d.clone());
    }
    let bytes = w
        .with_section(Section::SparsePostings, p.section)
        .try_finish()
        .unwrap();
    let s = MemoryStore::new();
    let key = Key::new("t/i/keys.seg");
    s.put(&key, bytes).await.unwrap();
    s.put(&sparse::dict_key(&key), bytes::Bytes::from(p.dictionary))
        .await
        .unwrap();
    (s, key, dict, section)
}

/// Each document's pairs, as a scan answers them: in key byte order.
fn pairs_of(docs: &[Document]) -> Vec<Vec<(String, f32)>> {
    docs.iter()
        .map(|d| match d.vectors.get(FIELD) {
            Some(VectorField::Sparse(p)) => {
                let mut p: Vec<(String, f32)> =
                    p.iter().map(|(k, w)| (k.clone(), w.get())).collect();
                p.sort_by(|a, b| a.0.cmp(&b.0));
                p
            }
            _ => Vec::new(),
        })
        .collect()
}

#[tokio::test]
async fn every_key_round_trips_and_keeps_its_own_postings() {
    // Criterion 1: through a bundle, a segment scan, and a compaction's rebuild of what the
    // scan answered; then each key's list is exactly the rows naming it.
    let keys = keys();
    assert!(keys.len() >= 70_010);
    let docs = rows(&keys, keys.len());
    let want = pairs_of(&docs);

    let bundled = decode_docs(&encode_docs(&docs)).unwrap();
    assert_eq!(pairs_of(&bundled), want, "through a bundle");

    let (s, key, _, _) = put(&docs).await;
    let seg = Segment::open(&s, &key).await.unwrap();
    let scanned = seg.scan(&s, &key, None).await.unwrap();
    assert_eq!(pairs_of(&scanned), want, "through a scan");

    let (s2, key2, dict, section) = put(&scanned).await;
    let seg2 = Segment::open(&s2, &key2).await.unwrap();
    assert_eq!(
        pairs_of(&seg2.scan(&s2, &key2, None).await.unwrap()),
        want,
        "through a compaction"
    );

    let mut naming: BTreeMap<&str, Vec<u32>> = BTreeMap::new();
    for (row, p) in want.iter().enumerate() {
        for (k, _) in p {
            naming.entry(k.as_str()).or_default().push(row as u32);
        }
    }
    assert_eq!(dict.len(), keys.len(), "a key merged with another");
    for (k, rows) in &naming {
        let e = dict
            .lookup(k)
            .unwrap_or_else(|| panic!("{:?} lost", k.get(..12)));
        let list = dict.decode_list(&e, &section[e.offset as usize..][..e.bytes as usize]);
        let got: Vec<u32> = list.iter().map(|(r, _)| *r).collect();
        assert_eq!(&got, rows, "{:?}'s postings", k.get(..12));
    }
}

/// Today's dictionary encoder before M177: version 1, `u32` dimensions, 24-byte entries.
fn v1_dictionary(lists: &[(u32, Vec<(u32, f32)>)]) -> (Vec<u8>, Vec<u8>) {
    let mut section = Vec::new();
    let mut dict = Vec::new();
    dict.extend_from_slice(b"PSTORESD");
    dict.extend_from_slice(&1u16.to_le_bytes());
    dict.push(4);
    dict.extend_from_slice(&(lists.len() as u32).to_le_bytes());
    for (dim, list) in lists {
        let offset = section.len() as u64;
        sparse::write_list(&mut section, list, ImpactEncoding::F32, 0.0);
        dict.extend_from_slice(&dim.to_le_bytes());
        dict.extend_from_slice(&offset.to_le_bytes());
        dict.extend_from_slice(&((section.len() as u64 - offset) as u32).to_le_bytes());
        dict.extend_from_slice(&(list.len() as u32).to_le_bytes());
        dict.extend_from_slice(&1.0f32.to_le_bytes());
    }
    (dict, section)
}

/// Today's run encoder before M177: version 2, a sparse field as kind 1 `(u32, f32)` pairs.
fn v2_run(docs: &[(&str, Vec<(u32, f32)>)]) -> Vec<u8> {
    let mut out = Vec::new();
    let bytes = |out: &mut Vec<u8>, b: &[u8]| {
        out.extend_from_slice(&(b.len() as u32).to_le_bytes());
        out.extend_from_slice(b);
    };
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&(docs.len() as u32).to_le_bytes());
    for (id, pairs) in docs {
        bytes(&mut out, id.as_bytes());
        out.extend_from_slice(&1u32.to_le_bytes());
        bytes(&mut out, FIELD.as_bytes());
        out.push(1);
        out.extend_from_slice(&(pairs.len() as u32).to_le_bytes());
        for (d, w) in pairs {
            out.extend_from_slice(&d.to_le_bytes());
            out.extend_from_slice(&w.to_le_bytes());
        }
        out.extend_from_slice(&0u32.to_le_bytes());
    }
    out
}

/// Today's run decoder before M177, as far as it decides: a run that is not version 2 is
/// refused (`UnsupportedVersion`) before any field is read.
fn old_decoder_reads(run: &[u8]) -> bool {
    u16::from_le_bytes([run[0], run[1]]) == 2
}

#[test]
fn a_u32_dimension_written_before_m177_reads_as_its_spelling_and_only_that() {
    // Criterion 4. In byte order "10" sorts before "9": a table left in numeric order would
    // binary-search past one of them.
    let lists = [
        (7, vec![(0, 1.0)]),
        (9, vec![(1, 2.0)]),
        (10, vec![(2, 3.0)]),
        (300, vec![(0, 4.0), (2, 5.0)]),
    ];
    let (raw, section) = v1_dictionary(&lists);
    let dict = Dictionary::decode(&raw).unwrap();
    assert_eq!(dict.keys().collect::<Vec<_>>(), ["10", "300", "7", "9"]);
    for (dim, list) in &lists {
        let e = dict.lookup(&dim.to_string()).unwrap();
        let got = dict.decode_list(&e, &section[e.offset as usize..][..e.bytes as usize]);
        assert_eq!(&got, list, "{dim}");
    }
    for other in ["07", "+7", "7 ", "0x7", "9.0", ""] {
        assert!(dict.lookup(other).is_none(), "{other:?} matched");
    }
    // A dimension twice is two lists for one key: refused, however the table is ordered.
    let twice = [(7, vec![(0, 1.0)]), (7, vec![(1, 2.0)])];
    assert!(Dictionary::decode(&v1_dictionary(&twice).0).is_none());

    let run = v2_run(&[("a", vec![(7, 0.5), (10, 0.25)]), ("b", vec![])]);
    let docs = decode_docs(&run).unwrap();
    assert_eq!(
        pairs_of(&docs),
        [
            vec![("10".to_owned(), 0.25), ("7".to_owned(), 0.5)],
            Vec::new()
        ]
    );
}

#[tokio::test]
async fn a_compaction_of_old_and_new_segments_writes_the_new_dictionary() {
    // Criterion 4: what a v1 segment scans to is keyed by spellings, so rebuilding it with a
    // v2 segment's rows writes one v2 dictionary naming both.
    let (raw, section) = v1_dictionary(&[(7, vec![(0, 1.0)]), (10, vec![(1, 2.0)])]);
    let mut old: Vec<Document> = (0..2)
        .map(|i| Document::new(format!("o{i}"), Vec::new()))
        .collect();
    old[0].vectors.insert(
        FIELD.into(),
        VectorField::Sparse(vec![("7".to_owned(), Impact::new(1.0))]),
    );
    old[1].vectors.insert(
        FIELD.into(),
        VectorField::Sparse(vec![("10".to_owned(), Impact::new(2.0))]),
    );
    let mut w = SegmentWriter::new(8);
    for d in &old {
        w.push(d.clone());
    }
    let bytes = w
        .with_section(Section::SparsePostings, section)
        .try_finish()
        .unwrap();
    let s = MemoryStore::new();
    let key = Key::new("t/i/old.seg");
    s.put(&key, bytes).await.unwrap();
    s.put(&sparse::dict_key(&key), bytes::Bytes::from(raw))
        .await
        .unwrap();
    let mut merged = Segment::open(&s, &key)
        .await
        .unwrap()
        .scan(&s, &key, None)
        .await
        .unwrap();
    assert_eq!(pairs_of(&merged), pairs_of(&old));
    merged.extend(rows(&["1.0".to_owned(), "\u{e9}".to_owned()], 2));
    let p = sparse::build(&merged, FIELD, ImpactEncoding::F32);
    assert_eq!(
        &p.dictionary[8..10],
        &2u16.to_le_bytes(),
        "not written as v2"
    );
    let dict = Dictionary::decode(&p.dictionary).unwrap();
    assert_eq!(
        dict.keys().collect::<Vec<_>>(),
        ["1.0", "10", "7", "\u{e9}"]
    );
}

#[test]
fn what_no_version_wrote_is_refused() {
    // Criterion 4: a v3 dictionary, a kind-3 field, kind 2 in a version-2 run.
    let p = sparse::build(&rows(&keys()[..20], 20), FIELD, ImpactEncoding::F32);
    let mut v3 = p.dictionary.clone();
    v3[8..10].copy_from_slice(&3u16.to_le_bytes());
    assert!(Dictionary::decode(&v3).is_none());

    let run = encode_docs(&rows(&keys()[..3], 2));
    assert_eq!(u16::from_le_bytes([run[0], run[1]]), 3);
    // The field's kind byte follows the version, the count, the id and the field's name.
    let kind_at = 2 + 4 + (4 + "r00000".len()) + 4 + (4 + FIELD.len());
    assert_eq!(run[kind_at], 2);
    let mut kind3 = run.clone();
    kind3[kind_at] = 3;
    assert!(decode_docs(&kind3).is_err());
    let mut as_v2 = run.clone();
    as_v2[0..2].copy_from_slice(&2u16.to_le_bytes());
    assert!(
        decode_docs(&as_v2).is_err(),
        "a kind-2 field read in a version-2 run"
    );
}

#[test]
fn a_run_holding_string_keys_is_refused_by_an_older_reader_and_no_other_is() {
    // Criterion 4: only a run with a sparse field is version 3, so a rollback still replays
    // every other run -- each index's run of a bundle is decoded on its own.
    let sparse_run = encode_docs(&rows(&keys()[..3], 2));
    let dense_run = encode_docs(&[Document::new("d", vec![1.0, 2.0])]);
    assert!(!old_decoder_reads(&sparse_run));
    assert!(old_decoder_reads(&dense_run));
    assert_eq!(decode_docs(&dense_run).unwrap().len(), 1);
}

#[test]
fn a_repeated_empty_or_overlong_key_is_refused_at_the_door() {
    // Criterion 5.
    let with = |pairs: Vec<(String, f32)>| {
        let mut d = Document::new("x", Vec::new());
        d.vectors.insert(
            FIELD.into(),
            VectorField::Sparse(
                pairs
                    .into_iter()
                    .map(|(k, w)| (k, Impact::new(w)))
                    .collect(),
            ),
        );
        check_storable(&d)
    };
    assert!(with(vec![("a".to_owned(), 1.0), ("b".to_owned(), 1.0)]).is_ok());
    assert!(with(vec![("z".repeat(SPARSE_KEY_MAX), 1.0)]).is_ok());
    assert!(with(vec![("a".to_owned(), 1.0), ("a".to_owned(), 2.0)]).is_err());
    assert!(
        with(vec![
            ("b".to_owned(), 1.0),
            ("a".to_owned(), 1.0),
            ("b".to_owned(), 1.0)
        ])
        .is_err(),
        "a repeat not adjacent in the order written"
    );
    assert!(with(vec![(String::new(), 1.0)]).is_err());
    assert!(with(vec![("z".repeat(SPARSE_KEY_MAX + 1), 1.0)]).is_err());
}
