//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::float_cmp,
    reason = "assertions in tests are the reporting mechanism"
)]

//! Named vector fields **in the segment**, which is the half D-28 calls a rewrite if
//! deferred: "retrofitting 'documents have n vectors' into a one-row-one-vector layout".

use pstore_blob::{Accounted, BlobStore, Key, MemoryStore};
use pstore_format::{DEFAULT_FIELD, Document, Section, Segment, SegmentWriter, VectorField};
use pstore_types::TenantId;
use std::collections::BTreeMap;

fn doc(i: usize, fields: &[(&str, Vec<Vec<f32>>)]) -> Document {
    Document {
        id: format!("d{i}"),
        vectors: fields
            .iter()
            .map(|(n, v)| ((*n).to_owned(), VectorField::Dense(v.clone())))
            .collect(),
        attrs: BTreeMap::new(),
    }
}

async fn round_trip(docs: Vec<Document>) -> (Segment, MemoryStore, Key, Vec<Document>) {
    let s = MemoryStore::new();
    let key = Key::new("seg");
    let mut w = SegmentWriter::new(8);
    for d in &docs {
        w.push(d.clone());
    }
    s.put(&key, w.try_finish().unwrap()).await.unwrap();
    let seg = Segment::open(&s, &key).await.unwrap();
    let out = seg.scan(&s, &key, None).await.unwrap();
    (seg, s, key, out)
}

#[tokio::test]
async fn a_document_round_trips_several_named_fields() {
    let docs: Vec<Document> = (0..40)
        .map(|i| {
            doc(
                i,
                &[
                    ("body", vec![vec![i as f32, 1.0]]),
                    ("title", vec![vec![2.0, i as f32]]),
                ],
            )
        })
        .collect();
    let (_, _, _, out) = round_trip(docs.clone()).await;
    assert_eq!(out.len(), 40);
    for (a, b) in out.iter().zip(&docs) {
        assert_eq!(a.field("body"), b.field("body"), "{}", a.id);
        assert_eq!(a.field("title"), b.field("title"), "{}", a.id);
    }
}

#[tokio::test]
async fn field_names_survive_a_new_field_being_added() {
    // Names, not positions. A field that sorts first must not become "field 0" for a reader
    // that addresses by index -- every stored vector would shift to the wrong name, and a
    // vector of the right length is always plausible.
    let one = round_trip(
        (0..16)
            .map(|i| doc(i, &[("zzz", vec![vec![i as f32]])]))
            .collect(),
    )
    .await;
    assert_eq!(one.3[3].field("zzz"), [vec![3.0]]);

    let two = round_trip(
        (0..16)
            .map(|i| {
                doc(
                    i,
                    &[("aaa", vec![vec![99.0]]), ("zzz", vec![vec![i as f32]])],
                )
            })
            .collect(),
    )
    .await;
    assert_eq!(
        two.3[3].field("zzz"),
        [vec![3.0]],
        "adding a field that sorts first changed what `zzz` means"
    );
    assert_eq!(two.3[3].field("aaa"), [vec![99.0]]);
}

#[tokio::test]
async fn a_field_may_hold_many_vectors_per_document() {
    // ⚠️ D-28. A late-interaction field holds one vector per token, so the count varies by
    // row and the section cannot be fixed-width.
    let docs: Vec<Document> = (0..20)
        .map(|i| {
            doc(
                i,
                &[(
                    "late",
                    (0..=i % 3).map(|t| vec![i as f32, t as f32]).collect(),
                )],
            )
        })
        .collect();
    let (_, _, _, out) = round_trip(docs.clone()).await;
    for (a, b) in out.iter().zip(&docs) {
        assert_eq!(a.field("late").len(), b.field("late").len(), "{}", a.id);
        assert_eq!(a.field("late"), b.field("late"), "{}", a.id);
    }
    // And at least one row really did carry several, or this asserts nothing.
    assert!(out.iter().any(|d| d.field("late").len() > 1));
}

#[tokio::test]
async fn every_vector_section_has_a_fields_row() {
    // A section nothing describes is a section only its writer can read.
    let (seg, _, _, _) = round_trip(
        (0..16)
            .map(|i| doc(i, &[("a", vec![vec![i as f32]]), ("b", vec![vec![1.0]])]))
            .collect(),
    )
    .await;
    let names: Vec<&str> = seg.fields().iter().map(|f| f.name.as_str()).collect();
    assert_eq!(names, ["a", "b"], "the Fields table does not describe both");
    for f in seg.fields() {
        assert!(
            seg.field_section(&f.name, Section::Vectors).is_some(),
            "{} has no vector section",
            f.name
        );
    }
}

#[tokio::test]
async fn a_segment_without_a_fields_section_still_reads() {
    // ⚠️ Forward compatibility, direction one. `modalities-and-sequencing.md` §3 promises
    // old segments stay valid forever, and a version bump would break every one of them --
    // which is why the Fields table is an additive section rather than a wider directory
    // entry. A segment with no such section is exactly today's: one dense field.
    let s = MemoryStore::new();
    let key = Key::new("legacy");
    let mut w = SegmentWriter::new(8);
    for i in 0..16 {
        w.push(Document::new(format!("d{i}"), vec![i as f32, 1.0]));
    }
    // Written through the path that emits no Fields section.
    s.put(&key, w.without_fields_section_for_test().finish())
        .await
        .unwrap();

    let seg = Segment::open(&s, &key).await.unwrap();
    assert!(seg.section(Section::Fields).is_none());
    let out = seg.scan(&s, &key, None).await.unwrap();
    assert_eq!(out.len(), 16);
    assert_eq!(out[3].vector(), [3.0, 1.0]);
    assert_eq!(out[3].field(DEFAULT_FIELD), [vec![3.0, 1.0]]);
}

#[tokio::test]
async fn a_fields_blind_reader_sees_field_zero_not_an_arbitrary_one() {
    // ⚠️ Forward compatibility, direction two, and the one that fails SILENTLY. A reader
    // that predates the Fields table looks up `Section::Vectors`; `Segment::open` stores
    // sections in a map keyed by id, so if every field shared that id the last one written
    // would win and such a reader would return ANOTHER FIELD'S VECTORS as the segment's.
    // A wrong answer, not an error. Fields after the first therefore use their own ids.
    let (seg, _, _, _) = round_trip(
        (0..16)
            .map(|i| {
                doc(
                    i,
                    &[("aaa", vec![vec![i as f32]]), ("zzz", vec![vec![99.0]])],
                )
            })
            .collect(),
    )
    .await;
    let legacy = seg.section(Section::Vectors).expect("no legacy section");
    let first = seg
        .field_section("aaa", Section::Vectors)
        .expect("field 0 has no section");
    assert_eq!(
        legacy, first,
        "the legacy Vectors id does not point at field 0, so an old reader gets another \
         field's vectors and cannot tell"
    );
}

#[tokio::test]
async fn reading_one_field_moves_no_bytes_of_another() {
    // ⚠️ Criterion 4, and the reason named fields are worth having: a second embedding must
    // cost the first one's queries nothing. Measured at a 256-byte coalescing gap -- the
    // default 64 KiB merges adjacent sections and would make this pass by measuring the
    // coalescer, which is the mistake M3's zone-map and exact-rerank tests both made.
    let s = Accounted::new(MemoryStore::with_coalesce_gap(256));
    let t = TenantId(1);
    let v = s.as_tenant(t);
    let key = Key::new("seg");
    let mut w = SegmentWriter::new(64);
    for i in 0..600 {
        w.push(doc(
            i,
            &[("a", vec![vec![i as f32; 16]]), ("b", vec![vec![1.0; 16]])],
        ));
    }
    v.put(&key, w.try_finish().unwrap()).await.unwrap();
    let seg = Segment::open(&v, &key).await.unwrap();
    let b_span = seg.field_section("b", Section::Vectors).unwrap();

    s.record_ranges();
    let got = seg.field_vectors(&v, &key, "a").await.unwrap();
    assert_eq!(got.len(), 600);
    assert_eq!(
        s.bytes_in(&key, b_span),
        0,
        "reading field `a` moved bytes of field `b`"
    );
}

#[tokio::test]
async fn an_absent_field_is_an_error_not_an_empty_answer() {
    // A miss that returns zero rows is indistinguishable from "the field is empty", and a
    // caller cannot tell a typo from a legitimately unpopulated field.
    let (seg, s, key, _) =
        round_trip((0..8).map(|i| doc(i, &[("a", vec![vec![1.0]])])).collect()).await;
    assert!(seg.field_vectors(&s, &key, "nope").await.is_err());
    assert!(seg.field_vectors(&s, &key, "a").await.is_ok());
}

#[tokio::test]
async fn eight_fields_still_open_in_one_round() {
    // ⚠️ Criterion 7. Each field adds three directory entries and a Fields row, all inside
    // the same meta region the block index already shares — and `SegmentWriter`'s fitting
    // loop can only shrink the BLOCK index, not the directory. Past some field count a cold
    // open silently becomes two round trips and every query becomes four, with nothing
    // reporting it. M3's budget said "3 depth" with nothing bounding the field count at all.
    let s = MemoryStore::new();
    let key = Key::new("wide");
    let mut w = SegmentWriter::new(64);
    for i in 0..400 {
        let fields: Vec<(String, VectorField)> = (0..8)
            .map(|f| (format!("field_{f}"), VectorField::dense(vec![i as f32; 8])))
            .collect();
        w.push(Document {
            id: format!("d{i}"),
            vectors: fields.into_iter().collect(),
            attrs: BTreeMap::new(),
        });
    }
    s.put(&key, w.try_finish().unwrap()).await.unwrap();

    let acc = Accounted::new(MemoryStore::new());
    let v = acc.as_tenant(TenantId(2));
    v.put(&key, s.get(&key).await.unwrap()).await.unwrap();
    let before = acc.count(TenantId(2), pstore_blob::OpClass::Read);
    let seg = Segment::open(&v, &key).await.unwrap();
    assert_eq!(
        acc.count(TenantId(2), pstore_blob::OpClass::Read) - before,
        1,
        "an 8-field segment cost a second read to open"
    );
    assert_eq!(seg.fields().len(), 8);
}

#[tokio::test]
async fn a_segment_too_wide_to_open_in_one_round_is_refused() {
    // The other half of criterion 7: beyond what fits, the writer must say so rather than
    // quietly spending a round trip on every query for the life of the segment.
    let mut w = SegmentWriter::new(64);
    for i in 0..50 {
        // Long names, so the Fields table outgrows the budget on field count alone.
        let fields: Vec<(String, VectorField)> = (0..200)
            .map(|f| {
                (
                    format!("a_deliberately_long_field_name_number_{f:04}"),
                    VectorField::dense(vec![i as f32; 4]),
                )
            })
            .collect();
        w.push(Document {
            id: format!("d{i}"),
            vectors: fields.into_iter().collect(),
            attrs: BTreeMap::new(),
        });
    }
    let err = w.try_finish().unwrap_err();
    assert!(
        format!("{err}").contains("round trip") || format!("{err}").contains("too wide"),
        "a segment that cannot open in one round was accepted: {err}"
    );
}

#[tokio::test]
async fn a_fixed_width_field_has_no_offset_table() {
    // ⚠️ Criterion 9. A row-offset table on a single-vector field costs four bytes a row and
    // buys nothing — and changes neither recall nor query bytes, so no existing gate sees
    // it. Asserted as an exact size instead.
    let s = MemoryStore::new();
    let key = Key::new("fixed");
    let mut w = SegmentWriter::new(64);
    for i in 0..200 {
        w.push(doc(i, &[("a", vec![vec![i as f32; 8]])]));
    }
    s.put(&key, w.try_finish().unwrap()).await.unwrap();
    let seg = Segment::open(&s, &key).await.unwrap();
    let span = seg.field_section("a", Section::Vectors).unwrap();
    assert_eq!(
        span.end - span.start,
        (200 * 8 * 4) as u64,
        "the section is bigger than its vectors, so it carries a table it does not need"
    );
    assert_eq!(seg.field_layout("a").unwrap().per_row, 1);
}
