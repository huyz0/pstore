//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! Several text fields in one segment (M151): a carried field's slice is its one-field build.

use pstore_format::text::{self, Analyzer, Dictionaries};
use pstore_format::{Document, Value};

fn analyzers() -> [Analyzer; 4] {
    [
        Analyzer::default(),
        Analyzer {
            stemming: true,
            remove_stopwords: true,
            ..Analyzer::default()
        },
        Analyzer {
            case_sensitive: true,
            ..Analyzer::default()
        },
        Analyzer {
            ascii_folding: true,
            ..Analyzer::default()
        },
    ]
}

/// Rows over `a`, `b`, `c` and `e`, each a string, an array, absent or empty; `e` never has text.
fn rows() -> Vec<Document> {
    let pool = [
        "the Quick fox",
        "",
        "Café-au-lait runs",
        "running dogs",
        "fox fox Fox",
        "a b c",
    ];
    let s = |x: &str| Value::Str(x.to_owned());
    (0..48)
        .map(|i| {
            let mut d = Document::new(format!("d{i:05}"), vec![1.0, 0.0]);
            let pick = |k: usize| pool[(i * k + k) % pool.len()];
            match i % 4 {
                0 => {}
                1 => {
                    d.attrs.insert("a".to_owned(), s(pick(1)));
                }
                2 => {
                    d.attrs
                        .insert("a".to_owned(), Value::Array(vec![s(pick(1)), s(pick(5))]));
                }
                _ => {
                    d.attrs.insert("a".to_owned(), s(""));
                }
            }
            if i % 3 != 0 {
                d.attrs.insert("b".to_owned(), s(pick(7)));
            }
            if i % 5 == 1 {
                d.attrs.insert(
                    "c".to_owned(),
                    Value::Array(vec![s(pick(3)), Value::Int(5)]),
                );
            }
            match i % 3 {
                0 => {
                    d.attrs.insert("e".to_owned(), s(""));
                }
                1 => {
                    d.attrs.insert("e".to_owned(), Value::Array(vec![s("")]));
                }
                _ => {}
            }
            d
        })
        .collect()
}

#[test]
fn one_field_builds_as_it_always_has() {
    // M151 criterion 1, the format's half: no container, no change.
    let docs = rows();
    for a in &analyzers() {
        let one = text::build_fields(&docs, &[("a", a)]).expect("built");
        let old = text::build_with(&docs, "a", a);
        assert_eq!(one.names, ["a"]);
        assert_eq!(one.postings, old.postings, "{a:?}");
        assert_eq!(one.dictionary, old.dictionary, "{a:?}");
        assert_eq!(one.fieldnorms, old.fieldnorms, "{a:?}");
    }
}

#[test]
fn each_carried_field_is_its_own_one_field_build() {
    // M151 criterion 2.
    let docs = rows();
    let [x, y, z, w] = analyzers();
    let asked = [("a", &x), ("b", &y), ("e", &w), ("c", &z)];
    let built = text::build_fields(&docs, &asked).expect("built");
    assert_eq!(
        built.names,
        ["a", "b", "c"],
        "a field with no text was carried"
    );
    let raw = bytes::Bytes::from(built.dictionary.clone());
    let dicts = Dictionaries::decode(&raw, 3).expect("the container decodes");
    assert_eq!(dicts.len(), 3);
    let n = docs.len();
    assert_eq!(built.fieldnorms.len(), 3 * n);
    let mut end = 0;
    for (i, (f, a)) in [("a", &x), ("b", &y), ("c", &z)].into_iter().enumerate() {
        let one = text::build_with(&docs, f, a);
        assert!(!one.postings.is_empty(), "{f} has no postings to compare");
        assert_eq!(dicts.dictionary(i).unwrap().as_ref(), one.dictionary, "{f}");
        let span = dicts.postings(i).unwrap();
        assert_eq!(span.start, end, "{f}: spans are back to back");
        end = span.end;
        assert_eq!(
            &built.postings[span.start as usize..span.end as usize],
            one.postings,
            "{f}"
        );
        assert_eq!(&built.fieldnorms[i * n..(i + 1) * n], one.fieldnorms, "{f}");
        assert_eq!(dicts.term_dict(i).unwrap().doc_count() as usize, n, "{f}");
    }
    assert_eq!(end as usize, built.postings.len());
}

#[test]
fn a_dictionary_is_a_slice_of_the_buffer_not_a_copy() {
    // M151: M122's cache keys a decoded dictionary by its buffer.
    let docs = rows();
    let a = Analyzer::default();
    let built = text::build_fields(&docs, &[("a", &a), ("b", &a)]).expect("built");
    let raw = bytes::Bytes::from(built.dictionary);
    let dicts = Dictionaries::decode(&raw, 2).expect("decodes");
    let inside = raw.as_ptr_range();
    for i in 0..2 {
        let d = dicts.dictionary(i).unwrap();
        assert!(inside.contains(&d.as_ptr()), "field {i} was copied");
    }
    let kept = Dictionaries::decode_shared(&raw, 2).expect("decodes");
    let again = Dictionaries::decode_shared(&raw, 2).expect("decodes");
    assert!(std::sync::Arc::ptr_eq(&kept, &again), "decoded twice");
}

#[test]
fn what_a_field_list_cannot_mean_is_refused() {
    let docs = rows();
    let a = Analyzer::default();
    assert!(text::build_fields(&docs, &[("a", &a), ("a", &a)]).is_none());
    assert!(text::build_fields(&docs, &[]).is_none());
    // Nothing carried: no postings, no norms, no sidecar.
    let none = text::build_fields(&docs, &[("e", &a), ("zz", &a)]).expect("built");
    assert!(none.names.is_empty());
    assert!(none.postings.is_empty() && none.dictionary.is_empty() && none.fieldnorms.is_empty());
}

#[test]
fn text_with_no_token_is_not_carried() {
    // M151 (code review): a stopword alone writes no postings, so a one-field segment of it
    // carries no text, and neither does a field of several.
    let mut docs = rows();
    for d in &mut docs {
        d.attrs.insert("s".to_owned(), Value::Str("The".to_owned()));
    }
    let a = Analyzer::default();
    let stop = Analyzer {
        remove_stopwords: true,
        ..Analyzer::default()
    };
    assert!(text::build_with(&docs, "s", &stop).postings.is_empty());
    let built = text::build_fields(&docs, &[("a", &a), ("s", &stop)]).expect("built");
    assert_eq!(built.names, ["a"], "a field of no token was carried");
    assert_eq!(
        built.dictionary,
        text::build_with(&docs, "a", &a).dictionary
    );
}

#[test]
fn a_malformed_container_decodes_as_nothing() {
    // M151 criterion 4, the sidecar's half.
    let docs = rows();
    let a = Analyzer::default();
    let built = text::build_fields(&docs, &[("a", &a), ("b", &a), ("c", &a)]).expect("built");
    let good = built.dictionary.clone();
    let dec = |v: &[u8], n: usize| Dictionaries::decode(&bytes::Bytes::copy_from_slice(v), n);
    assert!(dec(&good, 3).is_some());
    // The count is the segment's, and a container is never one field nor one field a container.
    assert!(dec(&good, 2).is_none());
    assert!(dec(&good, 4).is_none());
    assert!(dec(&good, 1).is_none());
    let one = text::build_with(&docs, "a", &a).dictionary;
    assert!(dec(&one, 1).is_some());
    assert!(dec(&one, 2).is_none());
    assert!(dec(&one, 0).is_none());
    // A header that miscounts a container otherwise whole for the segment's count.
    let pair = text::build_fields(&docs, &[("a", &a), ("b", &a)]).expect("built");
    assert!(dec(&pair.dictionary, 2).is_some());
    let mut miscounted = pair.dictionary.clone();
    miscounted[10] = 3;
    assert!(
        dec(&miscounted, 2).is_none(),
        "a header N that is not the segment's"
    );
    // Every truncation, and a trailing byte.
    for cut in 0..good.len() {
        assert!(dec(&good[..cut], 3).is_none(), "cut at {cut}");
    }
    let mut long = good.clone();
    long.push(0);
    assert!(dec(&long, 3).is_none());
    // The directory: entry i is dict_len(4) start(8) len(8), after magic(8) version(2) n(2).
    let entry = |i: usize| 12 + i * 20;
    let put = |v: &mut Vec<u8>, at: usize, x: u64| v[at..at + 8].copy_from_slice(&x.to_le_bytes());
    let get = |v: &[u8], at: usize| u64::from_le_bytes(v[at..at + 8].try_into().unwrap());
    // A gap: the last field starts one byte late. (A middle one would also overlap the next.)
    let mut gap = good.clone();
    let s2 = get(&gap, entry(2) + 4);
    put(&mut gap, entry(2) + 4, s2 + 1);
    assert!(dec(&gap, 3).is_none(), "a gap");
    let s1 = get(&good, entry(1) + 4);
    // An overlap: field 1 starts one byte early.
    let mut over = good.clone();
    put(&mut over, entry(1) + 4, s1 - 1);
    assert!(dec(&over, 3).is_none(), "an overlap");
    // Field 0 not at 0.
    let mut late = good.clone();
    put(&mut late, entry(0) + 4, 1);
    assert!(dec(&late, 3).is_none(), "a first span not at 0");
    // A dictionary that does not decode: field 2's magic.
    let mut bad = good.clone();
    let dict_len = |v: &[u8], i: usize| {
        u32::from_le_bytes(v[entry(i)..entry(i) + 4].try_into().unwrap()) as usize
    };
    let at2 = entry(3) + dict_len(&good, 0) + dict_len(&good, 1);
    bad[at2] ^= 0xff;
    assert!(dec(&bad, 3).is_none(), "a bad dictionary");
    // A bad magic or version.
    let mut magic = good.clone();
    magic[0] ^= 0xff;
    assert!(dec(&magic, 3).is_none());
    let mut version = good;
    version[8] = 2;
    assert!(dec(&version, 3).is_none());
}
