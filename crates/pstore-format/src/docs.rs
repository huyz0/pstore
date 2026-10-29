//! Document encoding for WAL bundles and segment blocks.
//!
//! ⚠️ **Two record shapes, one attribute codec.** A bundle carries whole documents,
//! vectors included, because it is the durable record of a write before anything has been
//! folded. A segment block carries id and attributes only, because segments keep vectors in
//! their own section so an approximate query never fetches them.
//!
//! They were briefly one function — correct while the shapes were identical, and wrong the
//! moment vectors moved out. What must not drift is the *attribute* encoding, so that is
//! the part that stayed shared.

use crate::codec::{Dec, Enc};
use crate::{Document, FormatError, Value};
use std::collections::BTreeMap;

/// Encodes one document's attributes.
///
/// ⚠️ Factored out because the two record shapes below diverged in M3 and the value tags
/// are the part that must not drift. They were briefly one function; that was correct while
/// the shapes were identical and stopped being correct the moment vectors left the blocks.
fn encode_attrs(e: &mut Enc, attrs: &BTreeMap<String, Value>) {
    e.u32(attrs.len() as u32);
    for (k, v) in attrs {
        e.bytes(k.as_bytes());
        encode_value(e, v);
    }
}

fn encode_value(e: &mut Enc, v: &Value) {
    match v {
        Value::Int(n) => {
            e.u8(0);
            e.i64(*n);
        }
        Value::Str(s) => {
            e.u8(1);
            e.bytes(s.as_bytes());
        }
        // M9h.1. A reader from before it refuses these tags as unknown, never misreads.
        Value::Float(f) => {
            e.u8(2);
            e.f64(*f);
        }
        Value::Bool(b) => {
            e.u8(3);
            e.u8(u8::from(*b));
        }
        // M9h.3: microseconds since the epoch, UTC.
        Value::DateTime(t) => {
            e.u8(5);
            e.i64(*t);
        }
        // M171: 16 bytes, and a length then the bytes.
        Value::Uuid(u) => {
            e.u8(6);
            e.raw(u);
        }
        Value::Bytes(b) => {
            e.u8(7);
            e.bytes(b);
        }
        // M172: 8 bytes, little-endian.
        Value::Uint(n) => {
            e.u8(8);
            e.u64(*n);
        }
        // M175: a null array element, the tag alone.
        Value::Null => e.u8(9),
        // M9h.2: a count, then each element with its own tag.
        Value::Array(items) => {
            e.u8(4);
            e.u32(items.len() as u32);
            for i in items {
                encode_value(e, i);
            }
        }
    }
}

fn decode_attrs(d: &mut Dec<'_>) -> Result<BTreeMap<String, Value>, FormatError> {
    decode_attrs_where(d, &every)
}

/// [`decode_attrs`], keeping only the names `keep` admits. A value it drops is still read
/// through, and refused for exactly what [`decode_value`] refuses.
fn decode_attrs_where(
    d: &mut Dec<'_>,
    keep: &dyn Fn(&str) -> bool,
) -> Result<BTreeMap<String, Value>, FormatError> {
    let n = d.u32()? as usize;
    let mut attrs = BTreeMap::new();
    for _ in 0..n {
        let k = d.str()?;
        if keep(k) {
            attrs.insert(k.to_owned(), decode_value(d, true)?);
        } else {
            skip_value(d, true)?;
        }
    }
    Ok(attrs)
}

/// [`decode_value`]'s checks, in its order, building nothing.
fn skip_value(d: &mut Dec<'_>, array: bool) -> Result<(), FormatError> {
    match d.u8()? {
        0 | 5 => {
            d.i64()?;
        }
        1 => {
            d.str()?;
        }
        2 => {
            d.f64()?;
        }
        8 => {
            d.u64()?;
        }
        6 => {
            d.uuid()?;
        }
        7 => {
            d.bytes()?;
        }
        3 => match d.u8()? {
            0 | 1 => {}
            _ => return Err(FormatError::Corrupt("a bool that is neither 0 nor 1")),
        },
        4 if array => {
            let n = d.u32()? as usize;
            for _ in 0..n {
                skip_value(d, false)?;
            }
        }
        4 => return Err(FormatError::Corrupt("an array inside an array")),
        9 if !array => {}
        9 => return Err(FormatError::Corrupt("a null outside an array")),
        _ => return Err(FormatError::Corrupt("unknown value tag")),
    }
    Ok(())
}

/// One tagged value; an array only where `array` allows it, so never nested.
fn decode_value(d: &mut Dec<'_>, array: bool) -> Result<Value, FormatError> {
    Ok(match d.u8()? {
        0 => Value::Int(d.i64()?),
        1 => Value::Str(d.string()?),
        2 => Value::Float(d.f64()?),
        3 => match d.u8()? {
            0 => Value::Bool(false),
            1 => Value::Bool(true),
            _ => return Err(FormatError::Corrupt("a bool that is neither 0 nor 1")),
        },
        4 if array => {
            let n = d.u32()? as usize;
            // Grown as elements decode, never reserved from the count: a corrupt count then
            // fails at the bytes it lacks instead of reserving gigabytes (M6a's remedy).
            let mut items = Vec::new();
            for _ in 0..n {
                items.push(decode_value(d, false)?);
            }
            Value::Array(items)
        }
        4 => return Err(FormatError::Corrupt("an array inside an array")),
        5 => Value::DateTime(d.i64()?),
        6 => Value::Uuid(d.uuid()?),
        7 => Value::Bytes(d.bytes()?.to_vec()),
        8 => Value::Uint(d.u64()?),
        // M175: only an array element is null; at the top level an attribute is absent.
        9 if !array => Value::Null,
        9 => return Err(FormatError::Corrupt("a null outside an array")),
        // A tag from a future version. Refused, not guessed: a mistyped attribute is
        // one a filter later reads as the wrong thing.
        _ => return Err(FormatError::Corrupt("unknown value tag")),
    })
}

/// The run encoding's own version, written ahead of the count.
///
/// ⚠️ Added by M5a.2, which widened a run from one dense vector to every named field. A run
/// written before it starts with a `u32` count, whose first two bytes cannot be 2 for any
/// plausible batch — so an old bundle is **refused**, not misdecoded into documents whose
/// fields are read out of the wrong bytes. Bundles are replayed into a segment immediately,
/// so refusing one is a replay failure rather than durable loss.
pub(crate) const DOCS_VERSION: u16 = 2;

/// Encodes documents **with every named field**, for the WAL bundle.
///
/// ⚠️ Unlike a segment block, a bundle is the durable record of a write before anything has
/// been folded, so it must carry everything needed to reconstruct the document. Until M5a.2
/// it carried **one dense vector**: a document written with a second dense field, or with a
/// sparse one, was acknowledged and then replayed as a document that never had it. That is
/// the same silent-loss shape `check_storable` was created to stop, one layer down, and the
/// comment that used to sit here called it "M5a's problem" rather than a bug.
#[must_use]
pub fn encode_docs(docs: &[Document]) -> Vec<u8> {
    let mut e = Enc::default();
    e.u16(DOCS_VERSION);
    e.u32(docs.len() as u32);
    for d in docs {
        e.bytes(d.id.as_bytes());
        e.u32(d.vectors.len() as u32);
        for (name, field) in &d.vectors {
            e.bytes(name.as_bytes());
            match field {
                crate::VectorField::Dense(vs) => {
                    e.u8(0);
                    e.u32(vs.len() as u32);
                    for v in vs {
                        e.u32(v.len() as u32);
                        for x in v {
                            e.f32(*x);
                        }
                    }
                }
                crate::VectorField::Sparse(pairs) => {
                    e.u8(1);
                    e.u32(pairs.len() as u32);
                    for (dim, impact) in pairs {
                        e.u32(*dim);
                        e.f32(impact.get());
                    }
                }
            }
        }
        encode_attrs(&mut e, &d.attrs);
    }
    e.0
}

/// Decodes a run written by [`encode_docs`], refusing anything malformed.
pub fn decode_docs(buf: &[u8]) -> Result<Vec<Document>, FormatError> {
    let mut d = Dec::new(buf);
    let version = d.u16()?;
    if version != DOCS_VERSION {
        return Err(FormatError::UnsupportedVersion(version));
    }
    let n = d.u32()? as usize;
    let mut out = Vec::with_capacity(n.min(1 << 20));
    for _ in 0..n {
        let id = d.string()?;
        let fields = d.u32()? as usize;
        let mut vectors = BTreeMap::new();
        for _ in 0..fields.min(1 << 12) {
            // Borrowed, then shared: the default field allocates nothing (M120).
            let name: crate::Field = d.str()?.into();
            let kind = d.u8()?;
            let count = d.u32()? as usize;
            let field = if kind == 0 {
                let mut vs = Vec::with_capacity(count.min(1 << 12));
                for _ in 0..count {
                    let dims = d.u32()? as usize;
                    let mut v = Vec::with_capacity(dims.min(1 << 16));
                    for _ in 0..dims {
                        v.push(d.f32()?);
                    }
                    vs.push(v);
                }
                crate::VectorField::Dense(vs)
            } else {
                let mut pairs = Vec::with_capacity(count.min(1 << 20));
                for _ in 0..count {
                    let dim = d.u32()?;
                    pairs.push((dim, crate::Impact::new(d.f32()?)));
                }
                crate::VectorField::Sparse(pairs)
            };
            vectors.insert(name, field);
        }
        let attrs = decode_attrs(&mut d)?;
        out.push(Document { id, vectors, attrs });
    }
    Ok(out)
}

/// Encodes segment-block rows: id and attributes, **no vector**.
#[must_use]
pub fn encode_rows(docs: &[Document]) -> Vec<u8> {
    let mut e = Enc::default();
    e.u32(docs.len() as u32);
    for d in docs {
        e.bytes(d.id.as_bytes());
        encode_attrs(&mut e, &d.attrs);
    }
    e.0
}

/// Decodes segment-block rows. Vectors come back empty and are filled from the
/// [`crate::Section::Vectors`] section by the caller that wants them.
pub fn decode_rows(buf: &[u8]) -> Result<Vec<Document>, FormatError> {
    decode_rows_where(buf, &Projection::ALL)
}

/// What [`decode_rows_where`] builds of each row (M119): the rest is read through, checked,
/// and dropped.
pub struct Projection<'a> {
    /// Whether the id is kept; dropped, it comes back empty.
    pub id: bool,
    /// The attributes kept.
    pub attrs: &'a (dyn Fn(&str) -> bool + Sync),
}

impl Projection<'_> {
    /// Everything: [`decode_rows`].
    pub const ALL: Projection<'static> = Projection {
        id: true,
        attrs: &every,
    };
}

fn every(_: &str) -> bool {
    true
}

/// [`decode_rows`], building only what `keep` names of each row, and refusing exactly what
/// it refuses.
///
/// ⚠️ For a filter's mask (M119): it reads a name or two of each row of every block it cannot
/// prune, and building every row whole -- its id, and every string attribute, the text field
/// among them -- was most of a filtered query's time.
pub fn decode_rows_where(buf: &[u8], keep: &Projection<'_>) -> Result<Vec<Document>, FormatError> {
    let mut d = Dec::new(buf);
    let n = d.u32()? as usize;
    let mut out = Vec::with_capacity(n.min(1 << 20));
    for _ in 0..n {
        let id = if keep.id {
            d.string()?
        } else {
            d.str()?;
            String::new()
        };
        let attrs = decode_attrs_where(&mut d, keep.attrs)?;
        out.push(Document {
            id,
            vectors: BTreeMap::new(),
            attrs,
        });
    }
    Ok(out)
}

/// The rows of a block `pick` names, with their positions, each built whole (M121); the
/// rest are read through, checked as [`decode_rows`] checks them, and dropped.
///
/// ⚠️ For `rows_at`, which resolves a query's hits: a block holds as many rows as its index
/// budget forces, thousands in a large segment, and building every one -- its id and every
/// attribute -- to return the one or two asked was most of the work.
pub fn decode_rows_picked(
    buf: &[u8],
    pick: &dyn Fn(usize) -> bool,
) -> Result<Vec<(usize, Document)>, FormatError> {
    let mut d = Dec::new(buf);
    let n = d.u32()? as usize;
    let mut out = Vec::new();
    for i in 0..n {
        if pick(i) {
            let id = d.string()?;
            let attrs = decode_attrs_where(&mut d, &every)?;
            out.push((
                i,
                Document {
                    id,
                    vectors: BTreeMap::new(),
                    attrs,
                },
            ));
        } else {
            d.str()?;
            decode_attrs_where(&mut d, &|_| false)?;
        }
    }
    Ok(out)
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]
mod tests {
    use super::*;

    fn rows() -> Vec<Document> {
        let mut a = Document::new("a", Vec::new());
        a.attrs.insert("n".to_owned(), Value::Int(3));
        a.attrs
            .insert("text".to_owned(), Value::Str("some words".to_owned()));
        a.attrs.insert(
            "tags".to_owned(),
            Value::Array(vec![Value::Bool(true), Value::Float(1.5)]),
        );
        let mut b = Document::new("b", Vec::new());
        b.attrs.insert("n".to_owned(), Value::Int(4));
        b.attrs.insert("when".to_owned(), Value::DateTime(7));
        // M171: both new tags, scalar and in an array.
        b.attrs.insert("u".to_owned(), Value::Uuid([0x5a; 16]));
        // M172: a uint64 at its maximum, scalar and in an array.
        b.attrs.insert("big".to_owned(), Value::Uint(u64::MAX));
        // M175: null elements, first, between and last.
        b.attrs.insert(
            "nulls".to_owned(),
            Value::Array(vec![Value::Null, Value::Int(1), Value::Null]),
        );
        b.attrs.insert(
            "bigs".to_owned(),
            Value::Array(vec![Value::Uint(0), Value::Uint(1 << 63)]),
        );
        b.attrs.insert(
            "y".to_owned(),
            Value::Array(vec![
                Value::Bytes(vec![0, 1, 255]),
                Value::Bytes(Vec::new()),
            ]),
        );
        vec![a, b]
    }

    #[test]
    fn a_projection_keeps_exactly_what_it_names() {
        let buf = encode_rows(&rows());
        let only_n = |name: &str| name == "n";
        let got = decode_rows_where(
            &buf,
            &Projection {
                id: false,
                attrs: &only_n,
            },
        )
        .unwrap();
        let want: Vec<(String, Vec<(String, Value)>)> = vec![
            (String::new(), vec![("n".to_owned(), Value::Int(3))]),
            (String::new(), vec![("n".to_owned(), Value::Int(4))]),
        ];
        let got: Vec<(String, Vec<(String, Value)>)> = got
            .into_iter()
            .map(|d| (d.id, d.attrs.into_iter().collect()))
            .collect();
        assert_eq!(got, want);
        // Everything is `decode_rows`.
        assert_eq!(
            decode_rows_where(&buf, &Projection::ALL).unwrap(),
            decode_rows(&buf).unwrap()
        );
    }

    /// ⚠️ A dropped value is still checked (M119): every corruption of every byte that
    /// `decode_rows` refuses, a projection keeping nothing refuses too, with the same error.
    #[test]
    fn a_projection_refuses_what_the_full_decode_refuses() {
        let buf = encode_rows(&rows());
        let nothing = |_: &str| false;
        let none = Projection {
            id: false,
            attrs: &nothing,
        };
        let mut refused = 0;
        for at in 0..buf.len() {
            for byte in [0x00, 0x01, 0x04, 0x06, 0x7f, 0xff] {
                let mut bad = buf.clone();
                bad[at] = byte;
                let full = decode_rows(&bad).map(|_| ());
                let projected = decode_rows_where(&bad, &none).map(|_| ());
                assert_eq!(
                    projected.as_ref().err().map(ToString::to_string),
                    full.as_ref().err().map(ToString::to_string),
                    "byte {at} set to {byte:#x}"
                );
                refused += usize::from(full.is_err());
            }
            // And a truncation at every length.
            let cut = &buf[..at];
            assert_eq!(
                decode_rows_where(cut, &none).err().map(|e| e.to_string()),
                decode_rows(cut).err().map(|e| e.to_string()),
                "cut at {at}"
            );
        }
        assert!(refused > 10, "the corruptions refused nothing: {refused}");
    }

    /// M171: every tag round-trips, and a UUID or bytes cut short is refused, never padded.
    /// M172: and a uint64.
    #[test]
    fn every_value_tag_round_trips_and_a_short_one_is_refused() {
        let docs = rows();
        let back = decode_rows(&encode_rows(&docs)).unwrap();
        let attrs = |ds: &[Document]| ds.iter().map(|d| d.attrs.clone()).collect::<Vec<_>>();
        assert_eq!(attrs(&back), attrs(&docs));
        // M175: and through the bundle's encoding, which shares the value tags.
        assert_eq!(
            attrs(&decode_docs(&encode_docs(&docs)).unwrap()),
            attrs(&docs)
        );
        for v in [
            Value::Uuid([7; 16]),
            Value::Bytes(vec![1, 2, 3]),
            Value::Uint(u64::MAX - 1),
        ] {
            let mut d = Document::new("x", Vec::new());
            d.attrs.insert("v".to_owned(), v.clone());
            let buf = encode_rows(std::slice::from_ref(&d));
            assert_eq!(decode_rows(&buf).unwrap()[0].attrs["v"], v);
            // The value is the buffer's last bytes, so a shorter buffer cuts into it.
            for cut in 1..=3 {
                let short = &buf[..buf.len() - cut];
                assert!(decode_rows(short).is_err(), "{v:?} cut by {cut} was read");
            }
        }
    }

    /// M175: a null is an array's element only; at the top level each decoder refuses it,
    /// whether it builds the value or reads past it.
    #[test]
    fn a_null_outside_an_array_is_refused() {
        let mut d = Document::new("x", Vec::new());
        d.attrs.insert("v".to_owned(), Value::Null);
        d.attrs.insert("w".to_owned(), Value::Int(1));
        let buf = encode_rows(std::slice::from_ref(&d));
        assert!(decode_rows(&buf).is_err());
        let only_w = |name: &str| name == "w";
        let p = Projection {
            id: false,
            attrs: &only_w,
        };
        assert!(decode_rows_where(&buf, &p).is_err());
    }

    #[test]
    fn rows_at_builds_only_the_rows_asked() {
        // M121: the rows `rows_at` asks are built, the rest read through -- the same rows the
        // full decode gives, and the same refusal at every truncation.
        let mut docs = Vec::new();
        for i in 0..40u32 {
            let mut d = Document::new(format!("r{i}"), Vec::new());
            d.attrs.insert("n".to_owned(), Value::Int(i64::from(i)));
            if i % 3 == 0 {
                d.attrs
                    .insert("text".to_owned(), Value::Str("x".repeat(i as usize)));
            }
            if i % 4 == 1 {
                d.attrs.insert(
                    "tags".to_owned(),
                    Value::Array(vec![Value::Bool(true), Value::Float(f64::from(i))]),
                );
            }
            docs.push(d);
        }
        let buf = encode_rows(&docs);
        let full = decode_rows(&buf).unwrap();
        let mut state: u64 = 7;
        for _ in 0..50 {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1);
            let mask = state;
            let pick = |i: usize| mask >> (i % 64) & 1 == 1;
            let want: Vec<(usize, Document)> = full
                .iter()
                .cloned()
                .enumerate()
                .filter(|(i, _)| pick(*i))
                .collect();
            assert_eq!(decode_rows_picked(&buf, &pick).unwrap(), want);
        }
        let none = |_: usize| false;
        for cut in 0..buf.len() {
            let (a, b) = (
                decode_rows(&buf[..cut]),
                decode_rows_picked(&buf[..cut], &none),
            );
            assert_eq!(a.err(), b.err(), "cut at {cut}");
        }
    }
}
