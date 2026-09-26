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
    let n = d.u32()? as usize;
    let mut attrs = BTreeMap::new();
    for _ in 0..n {
        let k = d.string()?;
        attrs.insert(k, decode_value(d, true)?);
    }
    Ok(attrs)
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
            let name = d.string()?;
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
    let mut d = Dec::new(buf);
    let n = d.u32()? as usize;
    let mut out = Vec::with_capacity(n.min(1 << 20));
    for _ in 0..n {
        let id = d.string()?;
        let attrs = decode_attrs(&mut d)?;
        out.push(Document {
            id,
            vectors: BTreeMap::new(),
            attrs,
        });
    }
    Ok(out)
}
