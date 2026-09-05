//! Document encoding, shared by segments and WAL bundles.
//!
//! One codec, used in both places: a bundle and the segment it folds into cannot disagree
//! about what a document is, because there is only one implementation to disagree with.

use crate::codec::{Dec, Enc};
use crate::{Document, FormatError, Value};
use std::collections::BTreeMap;

/// Encodes documents as a self-delimiting run.
#[must_use]
pub fn encode_docs(docs: &[Document]) -> Vec<u8> {
    let mut e = Enc::default();
    e.u32(docs.len() as u32);
    for d in docs {
        e.bytes(d.id.as_bytes());
        e.u32(d.vector.len() as u32);
        for f in &d.vector {
            e.f32(*f);
        }
        e.u32(d.attrs.len() as u32);
        for (k, v) in &d.attrs {
            e.bytes(k.as_bytes());
            match v {
                Value::Int(n) => {
                    e.u8(0);
                    e.i64(*n);
                }
                Value::Str(s) => {
                    e.u8(1);
                    e.bytes(s.as_bytes());
                }
            }
        }
    }
    e.0
}

/// Decodes a run written by [`encode_docs`], refusing anything malformed.
pub fn decode_docs(buf: &[u8]) -> Result<Vec<Document>, FormatError> {
    let mut d = Dec::new(buf);
    let n = d.u32()? as usize;
    let mut out = Vec::with_capacity(n.min(1 << 20));
    for _ in 0..n {
        let id = d.string()?;
        let dims = d.u32()? as usize;
        let mut vector = Vec::with_capacity(dims.min(1 << 16));
        for _ in 0..dims {
            vector.push(d.f32()?);
        }
        let an = d.u32()? as usize;
        let mut attrs = BTreeMap::new();
        for _ in 0..an {
            let k = d.string()?;
            let v = match d.u8()? {
                0 => Value::Int(d.i64()?),
                1 => Value::Str(d.string()?),
                _ => return Err(FormatError::Corrupt("unknown value tag")),
            };
            attrs.insert(k, v);
        }
        out.push(Document { id, vector, attrs });
    }
    Ok(out)
}
