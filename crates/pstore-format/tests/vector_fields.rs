//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::float_cmp,
    reason = "assertions in tests are the reporting mechanism"
)]

//! The v1 data model: named, plural, kind-tagged vector fields.
//!
//! `modalities-and-sequencing.md` §3 names three properties the model must have "regardless
//! of what is implemented", and calls a singular `vector` field "the migration trap". D-28
//! says retrofitting *n* vectors into a one-row-one-vector layout is a rewrite. Both are
//! about the shape, not the retriever — which is why these tests exist before sparse or
//! late-interaction search does.

use pstore_format::{DEFAULT_FIELD, Document, Impact, VectorField};
use std::collections::BTreeMap;

#[test]
fn a_document_carries_several_named_fields() {
    // ⚠️ The trap itself. A model with one vector cannot express "one dense embedding and
    // one sparse expansion of the same document", which is the shape hybrid retrieval needs
    // and the shape Qdrant's named vectors already have.
    let d = Document {
        id: "d1".to_owned(),
        vectors: BTreeMap::from([
            ("body_dense".to_owned(), VectorField::dense(vec![1.0, 2.0])),
            ("title_dense".to_owned(), VectorField::dense(vec![3.0, 4.0])),
        ]),
        attrs: BTreeMap::new(),
    };
    assert_eq!(d.field("body_dense"), [vec![1.0, 2.0]]);
    assert_eq!(d.field("title_dense"), [vec![3.0, 4.0]]);
    assert!(d.field("absent").is_empty());
}

#[test]
fn field_names_survive_a_new_field_being_added() {
    // Names, not positions. If fields were addressed by index, adding one would renumber
    // the others and every stored reference would point at the wrong embedding — silently,
    // because a vector of the right length is always plausible.
    let mut d = Document::new("d1", vec![1.0, 2.0]);
    let before = d.field(DEFAULT_FIELD).to_vec();
    d.vectors
        .insert("aaa_sorts_first".to_owned(), VectorField::dense(vec![9.0]));
    assert_eq!(
        d.field(DEFAULT_FIELD).to_vec(),
        before,
        "adding a field that sorts before it changed what {DEFAULT_FIELD} means"
    );
}

#[test]
fn a_field_may_hold_many_vectors_per_document() {
    // D-28: multi-vector is a document-level concept. A late-interaction field holds one
    // vector per token, and a layout that keeps the first is a layout that cannot be
    // extended without a rewrite.
    let d = Document {
        id: "d1".to_owned(),
        vectors: BTreeMap::from([(
            "late".to_owned(),
            VectorField::Dense(vec![vec![1.0, 2.0], vec![3.0, 4.0], vec![5.0, 6.0]]),
        )]),
        attrs: BTreeMap::new(),
    };
    assert_eq!(d.field("late").len(), 3, "only the first vector survived");
    assert_eq!(d.field("late")[2], vec![5.0, 6.0]);
}

#[test]
fn a_sparse_field_is_representable() {
    // ⚠️ The shape must exist in v1 even though the retriever is M5a. A sparse vector is
    // `(dimension, impact)` pairs; as a dense array over a 30,000-term vocabulary it would
    // cost 150x the bytes — 12 TB against 0.08 TB at 100M documents — so a model that can
    // only hold dense arrays moves the trap rather than closing it.
    let d = Document {
        id: "d1".to_owned(),
        vectors: BTreeMap::from([(
            "body_sparse".to_owned(),
            VectorField::Sparse(vec![(7, Impact::new(0.5)), (2_999, Impact::new(1.25))]),
        )]),
        attrs: BTreeMap::new(),
    };
    let Some(VectorField::Sparse(postings)) = d.vectors.get("body_sparse") else {
        panic!("a sparse field did not round-trip as sparse");
    };
    assert_eq!(postings.len(), 2);
    assert_eq!(postings[1].0, 2_999);
    assert_eq!(postings[1].1.get(), 1.25);
    // A sparse field has no dense reading, and must not pretend to.
    assert!(d.field("body_sparse").is_empty());
}

#[test]
fn an_impact_round_trips_through_its_api_not_its_field() {
    // ⚠️ `Impact`'s representation is PRIVATE, so D-72's configurable encoding — u8, f16 or
    // varint — can be chosen in M5a without touching `Document`. That is enforced by the
    // compiler rather than by this test: `Impact(0.5)` does not compile outside the crate.
    // What this test pins is that the API is the only way in and out, so changing the
    // storage cannot change what callers see.
    let i = Impact::new(0.5);
    assert_eq!(i.get(), 0.5);
    assert_eq!(Impact::new(0.0).get(), 0.0);
}

#[test]
fn the_single_vector_constructor_still_means_one_named_field() {
    // The bridge for every caller written against the old shape. It is the one-field case
    // spelled conveniently, not a second model alongside the first.
    let d = Document::new("d1", vec![1.0, 2.0, 3.0]);
    assert_eq!(d.vectors.len(), 1);
    assert_eq!(d.vector(), [1.0, 2.0, 3.0]);
    assert_eq!(d.field(DEFAULT_FIELD), [vec![1.0, 2.0, 3.0]]);
}

#[test]
fn a_document_with_no_vectors_is_legal() {
    // Attribute-only rows exist, and asking one for its vector must be empty rather than a
    // panic or a zero-filled array that scores as a real vector.
    let d = Document {
        id: "d1".to_owned(),
        vectors: BTreeMap::new(),
        attrs: BTreeMap::new(),
    };
    assert!(d.vector().is_empty());
    assert!(d.field(DEFAULT_FIELD).is_empty());
}

#[tokio::test]
async fn a_writer_refuses_only_what_it_still_cannot_store() {
    // ⚠️ This test began much wider, and the history is the point. M3b.1 made the MODEL
    // expressible ahead of the FORMAT, and in between a document with a field named
    // anything but `DEFAULT_FIELD` round-tripped to *nothing* — discarded, silently,
    // because the writer read one field by name and any other yielded an empty slice. The
    // refusal turned that into a loud failure; M3b.3 then made those documents storable, so
    // the refusal narrowed to what is genuinely still unsupported.
    use pstore_blob::{BlobStore, Key, MemoryStore};
    use pstore_format::{Segment, SegmentWriter};

    let mut w = SegmentWriter::new(8);
    w.push(Document {
        id: "d".to_owned(),
        vectors: BTreeMap::from([(
            "s".to_owned(),
            VectorField::Sparse(vec![(1, Impact::new(0.5))]),
        )]),
        attrs: BTreeMap::new(),
    });
    let err = w.try_finish().unwrap_err();
    assert!(
        format!("{err}").contains("M5a"),
        "a sparse field was refused without naming what would support it: {err}"
    );

    // Named and plural fields are stored now, not refused.
    let s = MemoryStore::new();
    let key = Key::new("ok");
    let mut w = SegmentWriter::new(8);
    w.push(Document {
        id: "d0".to_owned(),
        vectors: BTreeMap::from([
            ("other".to_owned(), VectorField::dense(vec![1.0])),
            (
                "late".to_owned(),
                VectorField::Dense(vec![vec![2.0], vec![3.0]]),
            ),
        ]),
        attrs: BTreeMap::new(),
    });
    s.put(&key, w.try_finish().unwrap()).await.unwrap();
    let seg = Segment::open(&s, &key).await.unwrap();
    let out = seg.scan(&s, &key, None).await.unwrap();
    assert_eq!(out[0].field("other"), [vec![1.0]]);
    assert_eq!(out[0].field("late").len(), 2);
}
