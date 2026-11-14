//! Columnar blocks (M231): a block's ids and attributes as one column each, typed and
//! encoded, in a `VERSION_COLUMNAR` segment.
//!
//! ```text
//! u8 layout (1) | rows | len, id column | attributes
//!   per attribute, in name order: len, name | len, body      (counts and lengths: varints)
//! body: u8 presence (0 every row, 1 bitmap) [bitmap] | u8 kind | the present values
//! ```
//!
//! ⚠️ **Lossless, and checked.** Every codec here is decoded exactly: an integer column is
//! frame-of-reference over the values or over their deltas, a float column is decimal only
//! when every value comes back bit for bit, and a string column's dictionary and LZ4 are
//! byte-exact. A value no typed kind holds -- an array, bytes, a name holding two types --
//! goes into a tagged column of M9a's value codec, so nothing a row block holds is out of
//! reach.
//!
//! ⚠️ **A body's length comes first**, so a projection skips a column it does not read in
//! one step: what row blocks could not do (M119 still walked every value of every row).

use crate::codec::{Dec, Enc};
use crate::{Document, FormatError, Projection, Value};
use std::collections::{BTreeMap, HashMap};

/// The block layout byte: the only one there is.
const LAYOUT: u8 = 1;

const KIND_INT: u8 = 0;
const KIND_STR: u8 = 1;
const KIND_FLOAT: u8 = 2;
const KIND_BOOL: u8 = 3;
const KIND_DATETIME: u8 = 5;
const KIND_UUID: u8 = 6;
const KIND_UINT: u8 = 8;
const KIND_TAGGED: u8 = 0xFE;

const INT_FOR: u8 = 1;
const INT_DELTA: u8 = 2;
const BYTES_PLAIN: u8 = 0;
const BYTES_DICT: u8 = 1;
const FLOAT_RAW: u8 = 0;
const FLOAT_DECIMAL: u8 = 1;
/// The largest decimal exponent a float column uses: `10^15` and below are exact in `f64`.
const MAX_EXPONENT: u32 = 15;

fn corrupt(what: &'static str) -> FormatError {
    FormatError::Corrupt(what)
}

/// An `i64` as a `u64` that orders the same: the sign bit flipped.
fn ordered(v: i64) -> u64 {
    (v as u64) ^ (1 << 63)
}

fn unordered(v: u64) -> i64 {
    (v ^ (1 << 63)) as i64
}

/// A LEB128 varint: small counts, lengths and frames cost a byte, not four or eight.
fn put_var(e: &mut Enc, mut v: u64) {
    while v >= 0x80 {
        #[allow(clippy::cast_possible_truncation, reason = "the low seven bits")]
        e.u8((v as u8) | 0x80);
        v >>= 7;
    }
    #[allow(clippy::cast_possible_truncation, reason = "below 0x80")]
    e.u8(v as u8);
}

fn get_var(d: &mut Dec<'_>) -> Result<u64, FormatError> {
    let mut v: u64 = 0;
    for shift in (0..64).step_by(7) {
        let b = d.u8()?;
        let low = u64::from(b & 0x7f);
        if shift == 63 && low > 1 {
            return Err(corrupt("a varint past 64 bits"));
        }
        v |= low << shift;
        if b & 0x80 == 0 {
            return Ok(v);
        }
    }
    Err(corrupt("a varint past 64 bits"))
}

fn get_len(d: &mut Dec<'_>) -> Result<usize, FormatError> {
    usize::try_from(get_var(d)?).map_err(|_| corrupt("a length past the address space"))
}

/// A length, then that many bytes.
fn put_bytes(e: &mut Enc, b: &[u8]) {
    put_var(e, b.len() as u64);
    e.raw(b);
}

fn get_bytes<'a>(d: &mut Dec<'a>) -> Result<&'a [u8], FormatError> {
    let n = get_len(d)?;
    d.raw(n)
}

/// Euclid's, in at most 128 steps: two 64-bit numbers take at most 93 (consecutive Fibonacci
/// numbers are the worst case), and a bound keeps a broken step from looping forever.
fn gcd(mut a: u64, mut b: u64) -> u64 {
    for _ in 0..128 {
        if b == 0 {
            break;
        }
        (a, b) = (b, a % b);
    }
    a
}

fn width_of(max: u64) -> u8 {
    #[allow(clippy::cast_possible_truncation, reason = "at most 64")]
    let w = (64 - max.leading_zeros()) as u8;
    w
}

/// `values`, each `width` bits, least significant first.
fn pack(e: &mut Enc, values: impl Iterator<Item = u64>, width: u8) {
    if width == 0 {
        return;
    }
    let mut acc: u128 = 0;
    let mut bits: u32 = 0;
    for v in values {
        acc |= u128::from(v) << bits;
        bits += u32::from(width);
        while bits >= 8 {
            #[allow(clippy::cast_possible_truncation, reason = "the low byte")]
            e.u8(acc as u8);
            acc >>= 8;
            bits -= 8;
        }
    }
    if bits > 0 {
        #[allow(clippy::cast_possible_truncation, reason = "the low byte")]
        e.u8(acc as u8);
    }
}

/// [`pack`]'s inverse: `n` values of `width` bits.
fn unpack(d: &mut Dec<'_>, n: usize, width: u8) -> Result<Vec<u64>, FormatError> {
    if width > 64 {
        return Err(corrupt("an integer column wider than 64 bits"));
    }
    if width == 0 {
        return Ok(vec![0; n]);
    }
    let len = n
        .checked_mul(usize::from(width))
        .map(|b| b.div_ceil(8))
        .ok_or_else(|| corrupt("an integer column too long"))?;
    let raw = d.raw(len)?;
    let mask: u128 = (1u128 << width) - 1;
    let mut out = Vec::with_capacity(n);
    let mut acc: u128 = 0;
    let mut bits: u32 = 0;
    let mut at = 0usize;
    for _ in 0..n {
        while bits < u32::from(width) {
            acc |= u128::from(*raw.get(at).ok_or(FormatError::Truncated)?) << bits;
            at += 1;
            bits += 8;
        }
        #[allow(
            clippy::cast_possible_truncation,
            reason = "masked to `width` <= 64 bits"
        )]
        out.push((acc & mask) as u64);
        acc >>= width;
        bits -= u32::from(width);
    }
    Ok(out)
}

/// Frame of reference over `values`: the minimum, the GCD of the offsets, a width, packed.
fn encode_for(e: &mut Enc, values: &[u64]) {
    let min = values.iter().copied().min().unwrap_or(0);
    let g = values.iter().fold(0, |g, v| gcd(g, v - min)).max(1);
    let max = values.iter().map(|v| (v - min) / g).max().unwrap_or(0);
    let width = width_of(max);
    put_var(e, min);
    put_var(e, g);
    e.u8(width);
    pack(e, values.iter().map(|v| (v - min) / g), width);
}

fn decode_for(d: &mut Dec<'_>, n: usize) -> Result<Vec<u64>, FormatError> {
    let min = get_var(d)?;
    let g = get_var(d)?;
    if g == 0 {
        return Err(corrupt("an integer column with a zero divisor"));
    }
    let width = d.u8()?;
    unpack(d, n, width)?
        .into_iter()
        .map(|v| {
            v.checked_mul(g)
                .and_then(|x| x.checked_add(min))
                .ok_or_else(|| corrupt("an integer column past 64 bits"))
        })
        .collect()
}

/// An integer column: frame-of-reference over the values, or over their successive (wrapping)
/// deltas when that is strictly smaller -- a tie keeps frame-of-reference, which decodes
/// without a running sum.
pub(crate) fn encode_ints(e: &mut Enc, values: &[u64]) {
    let mut plain = Enc::default();
    plain.u8(INT_FOR);
    encode_for(&mut plain, values);
    {
        let deltas: Vec<u64> = values
            .windows(2)
            .filter_map(|w| match *w {
                [a, b] => Some(ordered(b.wrapping_sub(a) as i64)),
                _ => None,
            })
            .collect();
        let mut delta = Enc::default();
        delta.u8(INT_DELTA);
        put_var(&mut delta, values.first().copied().unwrap_or(0));
        encode_for(&mut delta, &deltas);
        if delta.len() < plain.len() {
            e.raw(&delta.0);
            return;
        }
    }
    e.raw(&plain.0);
}

/// [`encode_ints`]'s inverse, `n` values.
pub(crate) fn decode_ints(d: &mut Dec<'_>, n: usize) -> Result<Vec<u64>, FormatError> {
    match d.u8()? {
        INT_FOR => decode_for(d, n),
        INT_DELTA => {
            let first = get_var(d)?;
            let deltas = decode_for(d, n.saturating_sub(1))?;
            let mut out = Vec::with_capacity(n);
            if n > 0 {
                out.push(first);
            }
            let mut at = first;
            for dl in deltas {
                at = at.wrapping_add(unordered(dl) as u64);
                out.push(at);
            }
            Ok(out)
        }
        _ => Err(corrupt("unknown integer codec")),
    }
}

/// LZ4 when it saves an eighth: a flag, then the raw length and the compressed bytes, or the
/// raw bytes.
fn encode_blob(e: &mut Enc, raw: &[u8]) {
    let packed = lz4_flex::block::compress(raw);
    if packed.len() < raw.len() - raw.len() / 8 {
        e.u8(1);
        put_var(e, raw.len() as u64);
        put_bytes(e, &packed);
    } else {
        e.u8(0);
        put_bytes(e, raw);
    }
}

fn decode_blob<'a>(d: &mut Dec<'a>) -> Result<std::borrow::Cow<'a, [u8]>, FormatError> {
    match d.u8()? {
        0 => Ok(std::borrow::Cow::Borrowed(get_bytes(d)?)),
        1 => {
            let len = get_len(d)?;
            let packed = get_bytes(d)?;
            // Bounded by what LZ4 can expand a block to, so a corrupt length cannot reserve
            // gigabytes before failing.
            if len > packed.len().saturating_mul(255).saturating_add(16) {
                return Err(corrupt("an LZ4 column longer than its bytes can hold"));
            }
            lz4_flex::block::decompress(packed, len)
                .map(std::borrow::Cow::Owned)
                .map_err(|_| corrupt("an LZ4 column that does not decompress"))
        }
        _ => Err(corrupt("unknown byte column compression")),
    }
}

/// A byte column: plain (lengths, then bytes) or a dictionary (entries, then ordinals),
/// whichever is smaller.
pub(crate) fn encode_bytes(e: &mut Enc, items: &[&[u8]]) {
    let plain = {
        let mut p = Enc::default();
        p.u8(BYTES_PLAIN);
        let lens: Vec<u64> = items.iter().map(|b| b.len() as u64).collect();
        encode_ints(&mut p, &lens);
        encode_blob(&mut p, &items.concat());
        p
    };
    let mut index: HashMap<&[u8], u64> = HashMap::new();
    let mut entries: Vec<&[u8]> = Vec::new();
    let ordinals: Vec<u64> = items
        .iter()
        .map(|b| {
            *index.entry(b).or_insert_with(|| {
                entries.push(b);
                entries.len() as u64 - 1
            })
        })
        .collect();
    if entries.len() < items.len() {
        let mut dict = Enc::default();
        dict.u8(BYTES_DICT);
        put_var(&mut dict, entries.len() as u64);
        let lens: Vec<u64> = entries.iter().map(|b| b.len() as u64).collect();
        encode_ints(&mut dict, &lens);
        encode_blob(&mut dict, &entries.concat());
        encode_ints(&mut dict, &ordinals);
        if dict.len() < plain.len() {
            e.raw(&dict.0);
            return;
        }
    }
    e.raw(&plain.0);
}

/// The ranges of `bytes` that `lens` cut it into, refusing lengths that do not sum to it.
fn cut(lens: &[u64], total: usize) -> Result<Vec<std::ops::Range<usize>>, FormatError> {
    let mut at = 0usize;
    let mut out = Vec::with_capacity(lens.len());
    for l in lens {
        let end = usize::try_from(*l)
            .ok()
            .and_then(|l| at.checked_add(l))
            .filter(|e| *e <= total)
            .ok_or_else(|| corrupt("a byte column's lengths exceed its bytes"))?;
        out.push(at..end);
        at = end;
    }
    if at != total {
        return Err(corrupt("a byte column's lengths do not cover its bytes"));
    }
    Ok(out)
}

/// A decoded byte column: the bytes, and each item's range in them.
pub(crate) struct Bytes<'a> {
    data: std::borrow::Cow<'a, [u8]>,
    ranges: Vec<std::ops::Range<usize>>,
    /// Per item, its entry; the identity for a plain column.
    ordinals: Option<Vec<usize>>,
}

impl Bytes<'_> {
    fn get(&self, i: usize) -> Option<&[u8]> {
        let r = match &self.ordinals {
            Some(o) => self.ranges.get(*o.get(i)?)?,
            None => self.ranges.get(i)?,
        };
        self.data.get(r.clone())
    }
}

/// [`encode_bytes`]'s inverse, `n` items.
pub(crate) fn decode_bytes<'a>(d: &mut Dec<'a>, n: usize) -> Result<Bytes<'a>, FormatError> {
    match d.u8()? {
        BYTES_PLAIN => {
            let lens = decode_ints(d, n)?;
            let data = decode_blob(d)?;
            let ranges = cut(&lens, data.len())?;
            Ok(Bytes {
                data,
                ranges,
                ordinals: None,
            })
        }
        BYTES_DICT => {
            let entries = get_len(d)?;
            if entries > n {
                return Err(corrupt("a dictionary larger than its column"));
            }
            let lens = decode_ints(d, entries)?;
            let data = decode_blob(d)?;
            let ranges = cut(&lens, data.len())?;
            let ordinals = decode_ints(d, n)?
                .into_iter()
                .map(|o| {
                    usize::try_from(o)
                        .ok()
                        .filter(|o| *o < entries)
                        .ok_or_else(|| corrupt("a dictionary ordinal out of range"))
                })
                .collect::<Result<Vec<_>, _>>()?;
            Ok(Bytes {
                data,
                ranges,
                ordinals: Some(ordinals),
            })
        }
        _ => Err(corrupt("unknown byte column codec")),
    }
}

fn bitmap(e: &mut Enc, bits: impl Iterator<Item = bool>) {
    pack(e, bits.map(u64::from), 1);
}

fn unbitmap(d: &mut Dec<'_>, n: usize) -> Result<Vec<bool>, FormatError> {
    Ok(unpack(d, n, 1)?.into_iter().map(|b| b == 1).collect())
}

/// The kind a column of these present values is written as.
fn kind_of(values: &[&Value]) -> u8 {
    let kind = |v: &Value| match v {
        Value::Int(_) => KIND_INT,
        Value::Str(_) => KIND_STR,
        Value::Float(_) => KIND_FLOAT,
        Value::Bool(_) => KIND_BOOL,
        Value::DateTime(_) => KIND_DATETIME,
        Value::Uuid(_) => KIND_UUID,
        Value::Uint(_) => KIND_UINT,
        Value::Array(_) | Value::Bytes(_) | Value::Null => KIND_TAGGED,
    };
    let first = values.first().map_or(KIND_TAGGED, |v| kind(v));
    if values.iter().all(|v| kind(v) == first) {
        first
    } else {
        KIND_TAGGED
    }
}

/// The decimal exponent that writes every float as an integer exactly, if one does.
fn decimal_exponent(fs: &[f64]) -> Option<u32> {
    (0..=MAX_EXPONENT).find(|e| {
        let scale = 10f64.powi(*e as i32);
        fs.iter().all(|f| {
            let i = (f * scale).round();
            i.abs() < 9.0e15 && ((i as i64) as f64 / scale).to_bits() == f.to_bits()
        })
    })
}

fn encode_values(e: &mut Enc, kind: u8, values: &[&Value]) {
    match kind {
        KIND_INT | KIND_DATETIME => {
            let ints: Vec<u64> = values
                .iter()
                .map(|v| match v {
                    Value::Int(n) | Value::DateTime(n) => ordered(*n),
                    _ => 0,
                })
                .collect();
            encode_ints(e, &ints);
        }
        KIND_UINT => {
            let ints: Vec<u64> = values
                .iter()
                .map(|v| if let Value::Uint(n) = v { *n } else { 0 })
                .collect();
            encode_ints(e, &ints);
        }
        KIND_FLOAT => {
            let fs: Vec<f64> = values
                .iter()
                .map(|v| if let Value::Float(f) = v { *f } else { 0.0 })
                .collect();
            if let Some(x) = decimal_exponent(&fs) {
                let scale = 10f64.powi(x as i32);
                e.u8(FLOAT_DECIMAL);
                #[allow(clippy::cast_possible_truncation, reason = "at most 15")]
                e.u8(x as u8);
                let ints: Vec<u64> = fs
                    .iter()
                    .map(|f| ordered((f * scale).round() as i64))
                    .collect();
                encode_ints(e, &ints);
            } else {
                e.u8(FLOAT_RAW);
                for f in fs {
                    e.f64(f);
                }
            }
        }
        KIND_BOOL => bitmap(e, values.iter().map(|v| matches!(v, Value::Bool(true)))),
        KIND_STR => {
            let items: Vec<&[u8]> = values
                .iter()
                .map(|v| {
                    if let Value::Str(s) = v {
                        s.as_bytes()
                    } else {
                        &[]
                    }
                })
                .collect();
            encode_bytes(e, &items);
        }
        KIND_UUID => {
            for v in values {
                if let Value::Uuid(u) = v {
                    e.raw(u);
                }
            }
        }
        _ => {
            for v in values {
                crate::docs::encode_value(e, v);
            }
        }
    }
}

/// A column's present values, decoded: each built on demand, so an unpicked row costs no
/// allocation.
enum Column<'a> {
    Ints(Vec<u64>, u8),
    Floats(Vec<f64>),
    Bools(Vec<bool>),
    Strs(Bytes<'a>),
    Uuids(&'a [u8]),
    Tagged(Vec<Value>),
}

impl Column<'_> {
    fn value(&self, i: usize) -> Result<Value, FormatError> {
        let gone = || corrupt("a column shorter than its presence");
        Ok(match self {
            Self::Ints(v, kind) => {
                let n = *v.get(i).ok_or_else(gone)?;
                match *kind {
                    KIND_UINT => Value::Uint(n),
                    KIND_DATETIME => Value::DateTime(unordered(n)),
                    _ => Value::Int(unordered(n)),
                }
            }
            Self::Floats(v) => Value::Float(*v.get(i).ok_or_else(gone)?),
            Self::Bools(v) => Value::Bool(*v.get(i).ok_or_else(gone)?),
            Self::Strs(b) => Value::Str(
                std::str::from_utf8(b.get(i).ok_or_else(gone)?)
                    .map_err(|_| corrupt("a string column that is not UTF-8"))?
                    .to_owned(),
            ),
            Self::Uuids(raw) => {
                let s = raw.get(i * 16..i * 16 + 16).ok_or_else(gone)?;
                Value::Uuid(s.try_into().map_err(|_| gone())?)
            }
            Self::Tagged(v) => v.get(i).ok_or_else(gone)?.clone(),
        })
    }
}

fn decode_values<'a>(d: &mut Dec<'a>, kind: u8, m: usize) -> Result<Column<'a>, FormatError> {
    Ok(match kind {
        KIND_INT | KIND_DATETIME | KIND_UINT => Column::Ints(decode_ints(d, m)?, kind),
        KIND_FLOAT => match d.u8()? {
            FLOAT_RAW => Column::Floats((0..m).map(|_| d.f64()).collect::<Result<_, _>>()?),
            FLOAT_DECIMAL => {
                let x = d.u8()?;
                if u32::from(x) > MAX_EXPONENT {
                    return Err(corrupt("a decimal exponent out of range"));
                }
                let scale = 10f64.powi(i32::from(x));
                Column::Floats(
                    decode_ints(d, m)?
                        .into_iter()
                        .map(|i| unordered(i) as f64 / scale)
                        .collect(),
                )
            }
            _ => return Err(corrupt("unknown float codec")),
        },
        KIND_BOOL => Column::Bools(unbitmap(d, m)?),
        KIND_STR => Column::Strs(decode_bytes(d, m)?),
        KIND_UUID => Column::Uuids(d.raw(m.checked_mul(16).ok_or(FormatError::Truncated)?)?),
        KIND_TAGGED => Column::Tagged(
            (0..m)
                .map(|_| crate::docs::decode_value(d, true))
                .collect::<Result<_, _>>()?,
        ),
        _ => return Err(corrupt("unknown column kind")),
    })
}

/// A block of rows, columnar.
#[must_use]
pub fn encode_block(docs: &[Document]) -> Vec<u8> {
    let mut e = Enc::default();
    e.u8(LAYOUT);
    put_var(&mut e, docs.len() as u64);
    let mut ids = Enc::default();
    let id_bytes: Vec<&[u8]> = docs.iter().map(|d| d.id.as_bytes()).collect();
    encode_bytes(&mut ids, &id_bytes);
    put_bytes(&mut e, &ids.0);
    let mut names: BTreeMap<&str, ()> = BTreeMap::new();
    for d in docs {
        for k in d.attrs.keys() {
            names.insert(k, ());
        }
    }
    put_var(&mut e, names.len() as u64);
    for name in names.keys() {
        let present: Vec<Option<&Value>> = docs.iter().map(|d| d.attrs.get(*name)).collect();
        let values: Vec<&Value> = present.iter().flatten().copied().collect();
        let mut body = Enc::default();
        if values.len() == docs.len() {
            body.u8(0);
        } else {
            body.u8(1);
            bitmap(&mut body, present.iter().map(Option::is_some));
        }
        let kind = kind_of(&values);
        body.u8(kind);
        encode_values(&mut body, kind, &values);
        put_bytes(&mut e, name.as_bytes());
        put_bytes(&mut e, &body.0);
    }
    e.0
}

/// The rows of a columnar block `pick` names (every row for `None`), each built as far as
/// `keep` asks, with their positions.
///
/// # Errors
/// `Corrupt` or `Truncated` for any malformed column, a trailing byte, or a layout byte this
/// version does not know.
pub fn decode_block(
    buf: &[u8],
    keep: &Projection<'_>,
    pick: Option<&dyn Fn(usize) -> bool>,
) -> Result<Vec<(usize, Document)>, FormatError> {
    let mut d = Dec::new(buf);
    if d.u8()? != LAYOUT {
        return Err(corrupt("unknown block layout"));
    }
    let n = get_len(&mut d)?;
    let picked = |i: usize| pick.is_none_or(|p| p(i));
    let mut rows: Vec<(usize, Document)> = Vec::new();
    let id_body = get_bytes(&mut d)?;
    if !keep.id {
        // ⚠️ Not decoded when not asked (a mask reads no id): the column's length bounds it,
        // and a row's place is its position.
        rows.extend((0..n).filter(|i| picked(*i)).map(|i| {
            (
                i,
                Document {
                    id: String::new(),
                    vectors: BTreeMap::new(),
                    attrs: BTreeMap::new(),
                },
            )
        }));
    } else {
        let mut ids = Dec::new(id_body);
        // A count past what the block could hold fails here, before anything is reserved.
        let col = decode_bytes(&mut ids, n)?;
        if !ids.at_end() {
            return Err(corrupt("trailing bytes in the id column"));
        }
        for i in (0..n).filter(|i| picked(*i)) {
            let id = std::str::from_utf8(col.get(i).ok_or(FormatError::Truncated)?)
                .map_err(|_| corrupt("an id that is not UTF-8"))?
                .to_owned();
            rows.push((
                i,
                Document {
                    id,
                    vectors: BTreeMap::new(),
                    attrs: BTreeMap::new(),
                },
            ));
        }
    }
    let attrs = get_len(&mut d)?;
    let mut last: Option<&str> = None;
    for _ in 0..attrs {
        let name = std::str::from_utf8(get_bytes(&mut d)?)
            .map_err(|_| corrupt("an attribute name that is not UTF-8"))?;
        if last.is_some_and(|l| l >= name) {
            return Err(corrupt("attribute columns out of order"));
        }
        last = Some(name);
        let body = get_bytes(&mut d)?;
        if !(keep.attrs)(name) {
            continue;
        }
        let mut b = Dec::new(body);
        let present: Option<Vec<bool>> = match b.u8()? {
            0 => None,
            1 => Some(unbitmap(&mut b, n)?),
            _ => return Err(corrupt("unknown presence codec")),
        };
        let m = present
            .as_ref()
            .map_or(n, |p| p.iter().filter(|x| **x).count());
        let kind = b.u8()?;
        let col = decode_values(&mut b, kind, m)?;
        if !b.at_end() {
            return Err(corrupt("trailing bytes in a column"));
        }
        // Each row's place among the present values.
        let slot: Vec<Option<usize>> = match &present {
            None => (0..n).map(Some).collect(),
            Some(p) => {
                let mut k = 0usize;
                p.iter()
                    .map(|x| {
                        x.then(|| {
                            k += 1;
                            k - 1
                        })
                    })
                    .collect()
            }
        };
        for (i, doc) in &mut rows {
            if let Some(Some(s)) = slot.get(*i) {
                doc.attrs.insert(name.to_owned(), col.value(*s)?);
            }
        }
    }
    if !d.at_end() {
        return Err(corrupt("trailing bytes in a block"));
    }
    Ok(rows)
}

/// A columnar block decoded **late** (M233): each row built as far as `first` asks, offered to
/// `admit` with its position, and only the admitted rows built further, as far as `then`
/// asks. A column `first` decoded is not decoded again; a column only `then` names has no
/// value built for a row `admit` refused.
///
/// # Errors
/// As [`decode_block`], for either pass.
pub fn decode_block_late(
    buf: &[u8],
    first: &Projection<'_>,
    admit: &mut dyn FnMut(usize, &Document) -> bool,
    then: &Projection<'_>,
) -> Result<Vec<(usize, Document)>, FormatError> {
    let mut rows = decode_block(buf, first, None)?;
    rows.retain(|(i, d)| admit(*i, d));
    let rest = |name: &str| (then.attrs)(name) && !(first.attrs)(name);
    let more = Projection {
        id: then.id && !first.id,
        attrs: &rest,
    };
    let wanted: Vec<usize> = rows.iter().map(|(i, _)| *i).collect();
    let pick = |i: usize| wanted.binary_search(&i).is_ok();
    let later = decode_block(buf, &more, Some(&pick))?;
    for ((_, doc), (_, add)) in rows.iter_mut().zip(later) {
        if more.id {
            doc.id = add.id;
        }
        doc.attrs.extend(add.attrs);
    }
    Ok(rows)
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    clippy::panic,
    reason = "assertions in tests are the reporting mechanism"
)]
mod tests {
    use super::*;

    fn ints(values: &[u64]) -> Vec<u8> {
        let mut e = Enc::default();
        encode_ints(&mut e, values);
        e.0
    }

    /// Each malformed codec header is refused by name: a varint past 64 bits, a zero divisor,
    /// an LZ4 length its bytes cannot hold, a dictionary larger than its column, an unknown
    /// float codec, and bytes left in the id column.
    #[test]
    fn an_unadmitted_row_is_never_built() {
        let row = |id: &str, s: &str, k: i64| {
            let mut d = Document::new(id, vec![]);
            d.attrs.insert("k".to_owned(), Value::Int(k));
            d.attrs.insert("s".to_owned(), Value::Str(s.to_owned()));
            d
        };
        let mut buf = encode_block(&[row("a", "ab", 1), row("b", "cd", 2)]);
        // Row 1's string is made bytes that are not UTF-8.
        let at = buf.windows(2).position(|w| w == b"cd").unwrap();
        buf[at] = 0xff;
        buf[at + 1] = 0xfe;
        let k = |n: &str| n == "k";
        let first = Projection {
            id: false,
            attrs: &k,
        };
        let late = |want: i64| {
            decode_block_late(
                &buf,
                &first,
                &mut |_, d: &Document| d.attrs.get("k") == Some(&Value::Int(want)),
                &Projection::ALL,
            )
        };
        let got = late(1).unwrap();
        assert_eq!(got.len(), 1);
        let (i, d) = &got[0];
        assert_eq!((*i, d.id.as_str()), (0, "a"));
        assert_eq!(d.attrs, row("a", "ab", 1).attrs);
        assert!(late(2).is_err());
        assert!(decode_block(&buf, &Projection::ALL, None).is_err());
        // What the first pass decoded is kept when the second does not ask for it.
        let s = |n: &str| n == "s";
        let got = decode_block_late(
            &encode_block(&[row("a", "ab", 1), row("b", "cd", 2)]),
            &first,
            &mut |i, _| i == 1,
            &Projection {
                id: true,
                attrs: &s,
            },
        )
        .unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0, 1);
        assert_eq!(got[0].1.id, "b");
        assert_eq!(got[0].1.attrs, row("b", "cd", 2).attrs);
    }

    #[test]
    fn a_late_row_holds_only_what_either_pass_asked_for() {
        let mut d = Document::new("a", vec![]);
        for (n, v) in [("k", 1), ("s", 2), ("x", 3)] {
            d.attrs.insert(n.to_owned(), Value::Int(v));
        }
        let buf = encode_block(&[d]);
        let k = |n: &str| n == "k";
        let s = |n: &str| n == "s";
        let got = decode_block_late(
            &buf,
            &Projection {
                id: false,
                attrs: &k,
            },
            &mut |_, _| true,
            &Projection {
                id: false,
                attrs: &s,
            },
        )
        .unwrap();
        let names: Vec<&str> = got[0].1.attrs.keys().map(String::as_str).collect();
        assert_eq!(names, ["k", "s"]);
        assert_eq!(got[0].1.id, "", "no id was asked for");
    }

    #[test]
    fn malformed_codec_headers_are_refused() {
        let refused = |r: Result<(), FormatError>, what: &str| match r {
            Err(FormatError::Corrupt(m)) => assert!(m.contains(what), "{m}"),
            other => panic!("{what}: {other:?}"),
        };
        let eleven = [0xffu8; 11];
        refused(get_var(&mut Dec::new(&eleven)).map(|_| ()), "past 64 bits");
        // Frame of reference: min 0, divisor 0.
        refused(
            decode_ints(&mut Dec::new(&[INT_FOR, 0, 0, 0]), 1).map(|_| ()),
            "zero divisor",
        );
        // LZ4, a raw length of 10,000 from one compressed byte.
        let mut e = Enc::default();
        e.u8(1);
        put_var(&mut e, 10_000);
        put_bytes(&mut e, &[0]);
        refused(decode_blob(&mut Dec::new(&e.0)).map(|_| ()), "longer than");
        // A dictionary of 3 entries for 2 rows.
        let mut e = Enc::default();
        e.u8(BYTES_DICT);
        put_var(&mut e, 3);
        refused(
            decode_bytes(&mut Dec::new(&e.0), 2).map(|_| ()),
            "dictionary larger",
        );
        refused(
            decode_values(&mut Dec::new(&[7]), KIND_FLOAT, 1).map(|_| ()),
            "unknown float codec",
        );
        // A block whose id column carries a byte past its one id.
        let good = encode_block(&[Document::new("a", vec![])]);
        let mut d = Dec::new(&good);
        assert_eq!(d.u8().unwrap(), LAYOUT);
        let n = get_len(&mut d).unwrap();
        let ids = get_bytes(&mut d).unwrap().to_vec();
        let mut e = Enc::default();
        e.u8(LAYOUT);
        put_var(&mut e, n as u64);
        let head = e.len();
        let rest = good[head + {
            let mut b = Enc::default();
            put_bytes(&mut b, &ids);
            b.len()
        }..]
            .to_vec();
        let mut longer = ids.clone();
        longer.push(0);
        put_bytes(&mut e, &longer);
        e.raw(&rest);
        refused(
            decode_block(&e.0, &Projection::ALL, None).map(|_| ()),
            "trailing bytes in the id column",
        );
    }

    #[test]
    fn a_varint_s_tenth_byte_holds_one_bit() {
        let var = |last: u8, low: u8| {
            let mut b = vec![low | 0x80; 9];
            b.push(last);
            get_var(&mut Dec::new(&b))
        };
        assert_eq!(var(0, 0).unwrap(), 0, "a padded zero");
        assert_eq!(var(1, 0x7f).unwrap(), u64::MAX);
        assert!(var(2, 0).is_err());
    }

    #[test]
    fn euclid_finishes_on_its_worst_case() {
        // F(93) and F(92): the largest consecutive Fibonacci numbers in 64 bits.
        let (mut a, mut b) = (0u64, 1u64);
        for _ in 0..92 {
            (a, b) = (b, a + b);
        }
        assert_eq!(gcd(b, a), 1);
        assert_eq!(gcd(12_000, 18_000), 6_000);
        assert_eq!(gcd(1_000, 0), 1_000);
    }

    #[test]
    fn a_common_divisor_is_factored_out() {
        // 256 multiples of 1,000 in no order: eight bits a value with the divisor (or nine a
        // delta), 18 without.
        let values: Vec<u64> = (0..256u64).map(|i| 7 + (i * 97 % 256) * 1_000).collect();
        let buf = ints(&values);
        assert!(buf.len() < 270, "{}", buf.len());
        assert_eq!(decode_ints(&mut Dec::new(&buf), 256).unwrap(), values);
    }

    #[test]
    fn a_short_column_keeps_frame_of_reference() {
        // Deltas cost their first value and a frame of their own: on two values, about nine
        // bytes more than frame-of-reference.
        let buf = ints(&[0, 1 << 40]);
        assert_eq!((buf[0], buf.len()), (INT_FOR, 10));
    }

    #[test]
    fn a_delta_column_of_no_values_is_empty() {
        let mut e = Enc::default();
        e.u8(INT_DELTA);
        put_var(&mut e, 5);
        encode_for(&mut e, &[]);
        assert_eq!(
            decode_ints(&mut Dec::new(&e.0), 0).unwrap(),
            Vec::<u64>::new()
        );
    }

    #[test]
    fn bytes_that_save_less_than_an_eighth_are_stored_raw() {
        let mut x = 0x2545_f491_u64;
        let mut noise = |n: usize| -> Vec<u8> {
            (0..n)
                .map(|_| {
                    x ^= x << 13;
                    x ^= x >> 7;
                    x ^= x << 17;
                    x as u8
                })
                .collect()
        };
        let random = noise(1_000);
        // 920 random bytes and 80 zeros: LZ4 saves something, but under an eighth.
        let mut some = noise(920);
        some.extend([0u8; 80]);
        let packed = lz4_flex::block::compress(&some).len();
        assert!(
            packed < some.len() && packed >= some.len() - some.len() / 8,
            "{packed}"
        );
        for raw in [random, some] {
            let mut e = Enc::default();
            encode_blob(&mut e, &raw);
            assert_eq!(e.0[0], 0, "stored raw");
            assert_eq!(&*decode_blob(&mut Dec::new(&e.0)).unwrap(), &raw[..]);
        }
    }

    #[test]
    fn a_column_of_repeats_takes_the_dictionary() {
        let items: Vec<&[u8]> = (0..100)
            .map(|i| {
                if i % 2 == 0 {
                    &b"alpha"[..]
                } else {
                    &b"beta"[..]
                }
            })
            .collect();
        let mut e = Enc::default();
        encode_bytes(&mut e, &items);
        assert_eq!(e.0[0], BYTES_DICT);
        let col = decode_bytes(&mut Dec::new(&e.0), items.len()).unwrap();
        for (i, b) in items.iter().enumerate() {
            assert_eq!(col.get(i).unwrap(), *b);
        }
    }

    #[test]
    fn floats_with_few_decimals_are_written_as_integers() {
        let column = |fs: &[f64]| {
            let values: Vec<Value> = fs.iter().map(|f| Value::Float(*f)).collect();
            let refs: Vec<&Value> = values.iter().collect();
            let mut e = Enc::default();
            encode_values(&mut e, KIND_FLOAT, &refs);
            let col = decode_values(&mut Dec::new(&e.0), KIND_FLOAT, fs.len()).unwrap();
            for (i, v) in values.iter().enumerate() {
                assert_eq!(&col.value(i).unwrap(), v, "{fs:?}");
            }
            e.0
        };
        let prices: Vec<f64> = (0..200).map(|i| f64::from(i) * 0.25 - 10.0).collect();
        let buf = column(&prices);
        assert_eq!(&buf[..2], &[FLOAT_DECIMAL, 2]);
        assert!(buf.len() < 220, "{}", buf.len());
        assert_eq!(&column(&[1.5, -2.5, 3.0])[..2], &[FLOAT_DECIMAL, 1]);
        assert_eq!(&column(&[4.0, 1e15])[..2], &[FLOAT_DECIMAL, 0]);
        assert_eq!(column(&[0.1 + 0.2, 1e300])[0], FLOAT_RAW);
    }

    #[test]
    fn every_row_of_a_uuid_column_keeps_its_own() {
        let docs: Vec<Document> = (0..20u8)
            .map(|i| {
                let mut d = Document::new(format!("d{i}"), vec![]);
                d.attrs.insert("u".to_owned(), Value::Uuid([i; 16]));
                d
            })
            .collect();
        let got = decode_block(&encode_block(&docs), &Projection::ALL, None).unwrap();
        for ((_, g), d) in got.iter().zip(&docs) {
            assert_eq!(g.attrs, d.attrs);
        }
    }

    #[test]
    fn every_width_round_trips() {
        for width in 0..=64u32 {
            let top = if width == 64 {
                u64::MAX
            } else {
                (1u64 << width).saturating_sub(1)
            };
            let values = vec![0, top, top / 3, 1.min(top), top];
            let buf = ints(&values);
            assert_eq!(
                decode_ints(&mut Dec::new(&buf), values.len()).unwrap(),
                values
            );
        }
    }

    #[test]
    fn ascending_values_take_the_delta_codec() {
        // Timestamps a second apart: frame-of-reference needs 29 bits a value, deltas none.
        let values: Vec<u64> = (0..500)
            .map(|i| ordered(1_700_000_000_000_000 + i * 1_000_000))
            .collect();
        let buf = ints(&values);
        assert_eq!(buf[0], INT_DELTA);
        assert!(buf.len() < 40, "{}", buf.len());
        // A shuffled column keeps frame-of-reference, which deltas would only widen.
        let mut x = 0x9e37_79b9_u64;
        let mixed: Vec<u64> = (0..500)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x % 1_000_000
            })
            .collect();
        assert_eq!(ints(&mixed)[0], INT_FOR);
    }

    #[test]
    fn redundant_bytes_are_compressed_and_random_ones_are_not() {
        let long: Vec<String> = (0..50)
            .map(|i| format!("{i:03}{}", "abc".repeat(30)))
            .collect();
        let items: Vec<&[u8]> = long.iter().map(|s| s.as_bytes()).collect();
        let mut e = Enc::default();
        encode_bytes(&mut e, &items);
        let raw: usize = items.iter().map(|b| b.len()).sum();
        assert!(e.len() * 4 < raw, "{} of {raw}", e.len());
        let col = decode_bytes(&mut Dec::new(&e.0), items.len()).unwrap();
        for (i, b) in items.iter().enumerate() {
            assert_eq!(col.get(i).unwrap(), *b);
        }
    }

    /// A dictionary column of `entries` one-byte entries, with the given ordinals.
    fn dict(entries: u64, ordinals: &[u64]) -> Vec<u8> {
        let mut e = Enc::default();
        e.u8(BYTES_DICT);
        put_var(&mut e, entries);
        encode_ints(&mut e, &vec![1; entries as usize]);
        e.u8(0);
        put_bytes(&mut e, &vec![b'a'; entries as usize]);
        encode_ints(&mut e, ordinals);
        e.0
    }

    #[test]
    fn a_dictionary_ordinal_past_its_entries_is_refused() {
        assert!(decode_bytes(&mut Dec::new(&dict(3, &[0, 1, 2, 2])), 4).is_ok());
        assert!(
            decode_bytes(&mut Dec::new(&dict(3, &[2, 1, 0])), 3).is_ok(),
            "n entries of n"
        );
        assert!(decode_bytes(&mut Dec::new(&dict(3, &[0, 1, 3, 2])), 4).is_err());
    }

    #[test]
    fn lengths_must_cover_the_bytes_exactly() {
        let col = |lens: &[u64], bytes: &[u8]| {
            let mut e = Enc::default();
            e.u8(BYTES_PLAIN);
            encode_ints(&mut e, lens);
            e.u8(0);
            put_bytes(&mut e, bytes);
            e.0
        };
        assert!(decode_bytes(&mut Dec::new(&col(&[1, 2], b"abc")), 2).is_ok());
        assert!(decode_bytes(&mut Dec::new(&col(&[1, 1], b"abc")), 2).is_err());
        assert!(decode_bytes(&mut Dec::new(&col(&[2, 2], b"abc")), 2).is_err());
    }

    #[test]
    fn a_decimal_exponent_past_fifteen_is_refused() {
        let col = |x: u8| {
            let mut e = Enc::default();
            e.u8(FLOAT_DECIMAL);
            e.u8(x);
            encode_ints(&mut e, &[ordered(5)]);
            e.0
        };
        assert!(decode_values(&mut Dec::new(&col(15)), KIND_FLOAT, 1).is_ok());
        assert!(decode_values(&mut Dec::new(&col(16)), KIND_FLOAT, 1).is_err());
    }

    #[test]
    fn attribute_columns_out_of_order_are_refused() {
        let mut d = Document::new("a", Vec::new());
        d.attrs.insert("x".into(), Value::Int(1));
        d.attrs.insert("y".into(), Value::Int(2));
        let buf = encode_block(&[d]);
        assert!(decode_block(&buf, &Projection::ALL, None).is_ok());
        // The two names are one byte each: swap them, and the columns are out of order.
        let at_x = buf.iter().rposition(|b| *b == b'x').unwrap();
        let at_y = buf.iter().rposition(|b| *b == b'y').unwrap();
        let mut bad = buf.clone();
        bad.swap(at_x, at_y);
        assert!(decode_block(&bad, &Projection::ALL, None).is_err());
    }
}
