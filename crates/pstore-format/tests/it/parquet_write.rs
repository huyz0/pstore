//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_precision_loss,
    reason = "assertions in tests are the reporting mechanism"
)]

//! M235: a page written by `parquet::write` reads back through the reference implementation,
//! the `parquet` crate: every column's type, and every value, as written.

use ::parquet::file::reader::{FileReader, SerializedFileReader};
use ::parquet::record::{Field, List};
use pstore_format::parquet::write::{Kind, write};
use pstore_format::{DEFAULT_FIELD, Document, Value, VectorField};
use std::collections::BTreeMap;

struct Rng(u64);

impl Rng {
    fn below(&mut self, n: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % n
    }
}

/// A test's stand-in for the API's JSON: what a JSON column must hold.
fn json(v: &Value) -> String {
    format!("{v:?}")
}

/// Rows over every kind: each attribute one kind, but for `mixed` (an int, a float or a
/// string), `nums` (ints and floats), `arr` (arrays) and absent values throughout.
fn rows(n: usize, seed: u64) -> Vec<Document> {
    let mut r = Rng(seed);
    (0..n)
        .map(|i| {
            let mut d = Document::new(format!("d{i:05}"), vec![i as f32, -0.5, 1e-3]);
            match r.below(10) {
                0 | 1 => {
                    d.vectors.remove(DEFAULT_FIELD);
                }
                2 => {
                    d.vectors
                        .insert(DEFAULT_FIELD.into(), VectorField::Dense(vec![Vec::new()]));
                }
                _ => {}
            }
            if r.below(3) == 0 {
                d.vectors.insert(
                    "named".to_owned().into(),
                    VectorField::Dense(vec![vec![i as f32 * 0.25; 4]]),
                );
            }
            let a = &mut d.attrs;
            let mut put = |name: &str, v: Value, r: &mut Rng| {
                if r.below(4) != 0 {
                    a.insert(name.to_owned(), v);
                }
            };
            put(
                "int",
                Value::Int([i64::MIN, i64::MAX, 0, -1, i as i64][r.below(5) as usize]),
                &mut r,
            );
            put(
                "float",
                Value::Float(
                    [-0.0, 0.1, f64::MAX, f64::MIN_POSITIVE, i as f64 / 3.0][r.below(5) as usize],
                ),
                &mut r,
            );
            put("bool", Value::Bool(r.below(2) == 0), &mut r);
            put(
                "str",
                Value::Str(["", "é", "a\"b", "line\nbreak"][r.below(4) as usize].to_owned()),
                &mut r,
            );
            put(
                "time",
                Value::DateTime(1_700_000_000_000_000 + i as i64 * 1_000_001),
                &mut r,
            );
            put(
                "uint",
                Value::Uint([u64::MAX, 1 << 63, 7][r.below(3) as usize]),
                &mut r,
            );
            put("uuid", Value::Uuid([i as u8; 16]), &mut r);
            put("bytes", Value::Bytes(vec![0, 255, i as u8]), &mut r);
            put(
                "mixed",
                [
                    Value::Int(i as i64),
                    Value::Float(0.5),
                    Value::Str("x".to_owned()),
                ][r.below(3) as usize]
                    .clone(),
                &mut r,
            );
            put(
                "nums",
                [Value::Int(i as i64), Value::Float(0.5)][r.below(2) as usize].clone(),
                &mut r,
            );
            put(
                "arr",
                Value::Array(vec![Value::Int(1), Value::Str("two".to_owned())]),
                &mut r,
            );
            d
        })
        .collect()
}

/// What the reader should give back for `v` in a column of `kind`.
fn want(kind: Kind, v: Option<&Value>) -> Field {
    let Some(v) = v else { return Field::Null };
    match (kind, v) {
        (Kind::Int, Value::Int(n)) => Field::Long(*n),
        (Kind::Double, Value::Float(f)) => Field::Double(*f),
        (Kind::Double, Value::Int(n)) => Field::Double(*n as f64),
        (Kind::Bool, Value::Bool(b)) => Field::Bool(*b),
        (Kind::Str, Value::Str(s)) => Field::Str(s.clone()),
        (Kind::Time, Value::DateTime(t)) => Field::TimestampMicros(*t),
        (Kind::Uint, Value::Uint(n)) => Field::ULong(*n),
        (Kind::Bytes, Value::Bytes(b)) => Field::Bytes(b.clone().into()),
        (Kind::Json, v) => Field::Str(json(v)),
        (k, v) => panic!("{v:?} in a {k:?} column"),
    }
}

fn floats(l: &List) -> Vec<f32> {
    l.elements()
        .iter()
        .map(|f| match f {
            Field::Float(x) => *x,
            other => panic!("{other:?} in a vector"),
        })
        .collect()
}

/// `f` on a thread, failed if it takes longer than a sound file ever does: a broken file can
/// send the reader into a loop, and a hang is a failure, not a test that never ends.
fn within(f: impl FnOnce() + Send + 'static) {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        f();
        let _ = tx.send(());
    });
    rx.recv_timeout(std::time::Duration::from_secs(30))
        .expect("the reader failed or hung on the file");
}

#[test]
fn a_page_reads_back_through_the_parquet_crate() {
    for (n, seed) in [(1, 1), (37, 2), (1_000, 3)] {
        within(move || reads_back(n, seed));
    }
}

fn reads_back(n: usize, seed: u64) {
    {
        let docs = rows(n, seed);
        let fields = vec![DEFAULT_FIELD.to_owned(), "named".to_owned()];
        let meta = vec![("pstore.index".to_owned(), "idx".to_owned())];
        let (file, skipped) = write(&docs, &fields, &meta, &json);
        assert!(skipped.is_empty());
        let reader = SerializedFileReader::new(bytes::Bytes::from(file)).unwrap();
        let md = reader.metadata();
        assert_eq!(md.file_metadata().num_rows(), n as i64);
        assert_eq!(md.num_row_groups(), 1);
        let kv: BTreeMap<String, Option<String>> = md
            .file_metadata()
            .key_value_metadata()
            .unwrap()
            .iter()
            .map(|k| (k.key.clone(), k.value.clone()))
            .collect();
        assert_eq!(kv["pstore.index"].as_deref(), Some("idx"));
        let names: Vec<&str> = {
            let mut v: Vec<&str> = docs
                .iter()
                .flat_map(|d| d.attrs.keys().map(String::as_str))
                .collect();
            v.sort_unstable();
            v.dedup();
            v
        };
        let kinds: BTreeMap<&str, Kind> = names
            .iter()
            .map(|&name| {
                (
                    name,
                    Kind::of(docs.iter().filter_map(|d| d.attrs.get(name))),
                )
            })
            .collect();
        // Each attribute's kind, stated rather than derived: past one row, every attribute
        // holds several values, so a kind kept only for a lone value is caught.
        if n >= 37 {
            let stated = [
                ("arr", Kind::Json),
                ("bool", Kind::Bool),
                ("bytes", Kind::Bytes),
                ("float", Kind::Double),
                ("int", Kind::Int),
                ("mixed", Kind::Json),
                ("nums", Kind::Double),
                ("str", Kind::Str),
                ("time", Kind::Time),
                ("uint", Kind::Uint),
                ("uuid", Kind::Uuid),
            ];
            assert_eq!(kinds.clone().into_iter().collect::<Vec<_>>(), stated);
        }
        let mut seen = 0;
        for (d, row) in docs.iter().zip(reader.get_row_iter(None).unwrap()) {
            let row = row.unwrap();
            let got: BTreeMap<&String, &Field> = row.get_column_iter().collect();
            assert_eq!(got.len(), 1 + names.len() + fields.len(), "{got:?}");
            assert_eq!(got[&"id".to_owned()], &Field::Str(d.id.clone()));
            for &name in &names {
                let f = got[&name.to_owned()];
                if kinds[name] == Kind::Uuid {
                    match (d.attrs.get(name), f) {
                        (None, Field::Null) => {}
                        (Some(Value::Uuid(u)), Field::Bytes(b)) => assert_eq!(b.data(), u),
                        (Some(Value::Uuid(u)), Field::Str(s)) => {
                            assert_eq!(*s, pstore_format::typed::format_uuid(u));
                        }
                        other => panic!("{name}: {other:?}"),
                    }
                    continue;
                }
                assert_eq!(
                    f,
                    &want(kinds[name], d.attrs.get(name)),
                    "{name} of {}",
                    d.id
                );
            }
            for field in &fields {
                let f = got[field];
                match (d.vectors.get(field.as_str()), f) {
                    (None, Field::Null) => {}
                    (Some(VectorField::Dense(v)), Field::ListInternal(l)) => {
                        assert_eq!(floats(l), v[0], "{field} of {}", d.id);
                    }
                    other => panic!("{field}: {other:?}"),
                }
            }
            seen += 1;
        }
        assert_eq!(seen, n);
    }
}

#[test]
fn a_field_of_several_vectors_a_row_is_left_out_by_name() {
    within(a_field_of_several_vectors_a_row_is_left_out_by_name_body);
}

fn a_field_of_several_vectors_a_row_is_left_out_by_name_body() {
    let mut docs = rows(5, 9);
    docs[2].vectors.insert(
        "multi".to_owned().into(),
        VectorField::Dense(vec![vec![1.0, 2.0], vec![3.0, 4.0]]),
    );
    let fields = vec![DEFAULT_FIELD.to_owned(), "multi".to_owned()];
    let (file, skipped) = write(&docs, &fields, &[], &json);
    assert_eq!(skipped, ["multi"]);
    let reader = SerializedFileReader::new(bytes::Bytes::from(file)).unwrap();
    let row = reader.get_row_iter(None).unwrap().next().unwrap().unwrap();
    assert!(row.get_column_iter().all(|(n, _)| n != "multi"));
    assert!(row.get_column_iter().any(|(n, _)| n == DEFAULT_FIELD));
}

#[test]
fn a_field_named_as_an_attribute_is_left_out_by_name() {
    within(a_field_named_as_an_attribute_is_left_out_by_name_body);
}

fn a_field_named_as_an_attribute_is_left_out_by_name_body() {
    let mut docs = rows(5, 11);
    docs[1]
        .attrs
        .insert(DEFAULT_FIELD.to_owned(), Value::Str("a string".to_owned()));
    let (file, skipped) = write(&docs, &[DEFAULT_FIELD.to_owned()], &[], &json);
    assert_eq!(skipped, [DEFAULT_FIELD]);
    let reader = SerializedFileReader::new(bytes::Bytes::from(file)).unwrap();
    let names: Vec<String> = reader
        .metadata()
        .file_metadata()
        .schema_descr()
        .columns()
        .iter()
        .map(|c| c.path().string())
        .collect();
    assert_eq!(
        names.iter().filter(|n| *n == DEFAULT_FIELD).count(),
        1,
        "{names:?}"
    );
}

#[test]
fn an_empty_page_is_a_file_of_no_rows() {
    within(an_empty_page_is_a_file_of_no_rows_body);
}

fn an_empty_page_is_a_file_of_no_rows_body() {
    let (file, _) = write(&[], &[DEFAULT_FIELD.to_owned()], &[], &json);
    let reader = SerializedFileReader::new(bytes::Bytes::from(file)).unwrap();
    assert_eq!(reader.metadata().file_metadata().num_rows(), 0);
    assert_eq!(reader.get_row_iter(None).unwrap().count(), 0);
}
