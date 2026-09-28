//! A [`Predicate`] as a string, for a write's condition carried in a lane bundle (M13).
//!
//! ⚠️ **Why not the filter's JSON.** The JSON form is parsed by the server, and the fold that
//! evaluates a condition runs in the engine, which does not depend on it. This is a small
//! tagged binary form, hex-encoded so that it is a valid string attribute. A condition only
//! ever travels from this process's writer to a fold, so it has no version but its tags. A
//! tag it does not know decodes as `None`, never as a different predicate.

use crate::filter::{Op, Predicate};
use pstore_format::Value;

/// Deeper than any filter a client writes: bounds the decoder's recursion.
const MAX_DEPTH: usize = 64;

/// The predicate, as a string [`decode`] reads back.
#[must_use]
pub fn encode(p: &Predicate) -> String {
    let mut b = Vec::new();
    put_pred(&mut b, p);
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// The predicate [`encode`] wrote, or `None` for anything else.
#[must_use]
pub fn decode(s: &str) -> Option<Predicate> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    let bytes: Option<Vec<u8>> = (0..s.len())
        .step_by(2)
        .map(|i| s.get(i..i + 2).and_then(|h| u8::from_str_radix(h, 16).ok()))
        .collect();
    let bytes = bytes?;
    let mut r = Reader { b: &bytes, at: 0 };
    let p = r.pred(0)?;
    (r.at == bytes.len()).then_some(p)
}

fn put_str(b: &mut Vec<u8>, s: &str) {
    b.extend((s.len() as u32).to_le_bytes());
    b.extend(s.as_bytes());
}

fn put_value(b: &mut Vec<u8>, v: &Value) {
    match v {
        Value::Int(n) => {
            b.push(0);
            b.extend(n.to_le_bytes());
        }
        Value::Str(s) => {
            b.push(1);
            put_str(b, s);
        }
        Value::Float(f) => {
            b.push(2);
            b.extend(f.to_bits().to_le_bytes());
        }
        Value::Bool(x) => {
            b.push(3);
            b.push(u8::from(*x));
        }
        Value::DateTime(t) => {
            b.push(4);
            b.extend(t.to_le_bytes());
        }
        Value::Array(items) => {
            b.push(5);
            b.extend((items.len() as u32).to_le_bytes());
            for i in items {
                put_value(b, i);
            }
        }
    }
}

fn put_values(b: &mut Vec<u8>, vs: &[Value]) {
    b.extend((vs.len() as u32).to_le_bytes());
    for v in vs {
        put_value(b, v);
    }
}

fn op_code(op: Op) -> u8 {
    match op {
        Op::Eq => 0,
        Op::Lt => 1,
        Op::Lte => 2,
        Op::Gt => 3,
        Op::Gte => 4,
    }
}

fn put_pred(b: &mut Vec<u8>, p: &Predicate) {
    match p {
        Predicate::Cmp(a, op, v) => {
            b.push(0);
            put_str(b, a);
            b.push(op_code(*op));
            put_value(b, v);
        }
        Predicate::Absent(a) => {
            b.push(1);
            put_str(b, a);
        }
        Predicate::In(a, vs) => {
            b.push(2);
            put_str(b, a);
            put_values(b, vs);
        }
        Predicate::ContainsAny(a, vs) => {
            b.push(3);
            put_str(b, a);
            put_values(b, vs);
        }
        Predicate::And(ps) | Predicate::Or(ps) => {
            b.push(if matches!(p, Predicate::And(_)) { 4 } else { 5 });
            b.extend((ps.len() as u32).to_le_bytes());
            for q in ps {
                put_pred(b, q);
            }
        }
        Predicate::Not(q) => {
            b.push(6);
            put_pred(b, q);
        }
    }
}

struct Reader<'a> {
    b: &'a [u8],
    at: usize,
}

impl Reader<'_> {
    fn take(&mut self, n: usize) -> Option<&[u8]> {
        let s = self.b.get(self.at..self.at.checked_add(n)?)?;
        self.at += n;
        Some(s)
    }

    fn u8(&mut self) -> Option<u8> {
        self.take(1)?.first().copied()
    }

    fn u32(&mut self) -> Option<usize> {
        let s = self.take(4)?;
        Some(u32::from_le_bytes(s.try_into().ok()?) as usize)
    }

    fn u64(&mut self) -> Option<u64> {
        let s = self.take(8)?;
        Some(u64::from_le_bytes(s.try_into().ok()?))
    }

    fn string(&mut self) -> Option<String> {
        let n = self.u32()?;
        String::from_utf8(self.take(n)?.to_vec()).ok()
    }

    fn value(&mut self, array: bool) -> Option<Value> {
        Some(match self.u8()? {
            #[allow(clippy::cast_possible_wrap, reason = "the bits are an i64's")]
            0 => Value::Int(self.u64()? as i64),
            1 => Value::Str(self.string()?),
            2 => Value::Float(f64::from_bits(self.u64()?)),
            3 => Value::Bool(self.u8()? != 0),
            #[allow(clippy::cast_possible_wrap, reason = "the bits are an i64's")]
            4 => Value::DateTime(self.u64()? as i64),
            5 if array => {
                let n = self.u32()?;
                let mut items = Vec::with_capacity(n.min(1024));
                for _ in 0..n {
                    items.push(self.value(false)?);
                }
                Value::Array(items)
            }
            _ => return None,
        })
    }

    fn values(&mut self) -> Option<Vec<Value>> {
        let n = self.u32()?;
        let mut out = Vec::with_capacity(n.min(1024));
        for _ in 0..n {
            out.push(self.value(true)?);
        }
        Some(out)
    }

    fn pred(&mut self, depth: usize) -> Option<Predicate> {
        if depth > MAX_DEPTH {
            return None;
        }
        Some(match self.u8()? {
            0 => {
                let a = self.string()?;
                let op = match self.u8()? {
                    0 => Op::Eq,
                    1 => Op::Lt,
                    2 => Op::Lte,
                    3 => Op::Gt,
                    4 => Op::Gte,
                    _ => return None,
                };
                Predicate::Cmp(a, op, self.value(true)?)
            }
            1 => Predicate::Absent(self.string()?),
            2 => Predicate::In(self.string()?, self.values()?),
            3 => Predicate::ContainsAny(self.string()?, self.values()?),
            t @ (4 | 5) => {
                let n = self.u32()?;
                let mut ps = Vec::with_capacity(n.min(1024));
                for _ in 0..n {
                    ps.push(self.pred(depth + 1)?);
                }
                if t == 4 {
                    Predicate::And(ps)
                } else {
                    Predicate::Or(ps)
                }
            }
            6 => Predicate::Not(Box::new(self.pred(depth + 1)?)),
            _ => return None,
        })
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    reason = "assertions in tests are the reporting mechanism"
)]
mod tests {
    use super::*;

    fn every_shape() -> Predicate {
        let a = || "a".to_owned();
        Predicate::And(vec![
            Predicate::Cmp(a(), Op::Eq, Value::Int(-7)),
            Predicate::Cmp(a(), Op::Lt, Value::Float(2.5)),
            Predicate::Cmp(a(), Op::Lte, Value::Str("é".into())),
            Predicate::Cmp(a(), Op::Gt, Value::Bool(true)),
            Predicate::Cmp(a(), Op::Gte, Value::DateTime(i64::MIN)),
            Predicate::Absent(a()),
            Predicate::In(a(), vec![Value::Int(1), Value::Array(vec![Value::Int(2)])]),
            Predicate::ContainsAny(a(), vec![Value::Str(String::new())]),
            Predicate::Or(vec![]),
            Predicate::Not(Box::new(Predicate::And(vec![]))),
        ])
    }

    #[test]
    fn every_predicate_round_trips() {
        let p = every_shape();
        assert_eq!(decode(&encode(&p)), Some(p));
    }

    #[test]
    fn anything_else_is_none() {
        let good = encode(&every_shape());
        for bad in [
            String::new(),
            "0".to_owned(),
            "zz".to_owned(),
            format!("{good}00"),
            good[..good.len() - 2].to_owned(),
            "07".to_owned(),
            // A Cmp with an unknown operator.
            format!("00{}{}", "01000000", "6109"),
        ] {
            assert_eq!(decode(&bad), None, "{bad}");
        }
        // Nested past the bound.
        let mut deep = Predicate::And(vec![]);
        for _ in 0..=MAX_DEPTH {
            deep = Predicate::Not(Box::new(deep));
        }
        assert_eq!(decode(&encode(&deep)), None);
    }
}
