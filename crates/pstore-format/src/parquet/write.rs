//! Writing a page of documents as a Parquet file (M235): the reverse of M70's footer reader.
//!
//! One row group, one data page a column chunk, `PLAIN` values, uncompressed, v1 pages: the
//! simplest file every reader reads. Each attribute is a column typed from the values the page
//! holds; a dense field holding one vector a row is a list of floats.

use crate::{Document, Value, VectorField};
use std::collections::BTreeSet;

/// Parquet's physical types.
const BOOLEAN: i32 = 0;
const INT64: i32 = 2;
const FLOAT: i32 = 4;
const DOUBLE: i32 = 5;
const BYTE_ARRAY: i32 = 6;
const FIXED_LEN_BYTE_ARRAY: i32 = 7;

/// Repetition.
const REQUIRED: i32 = 0;
const OPTIONAL: i32 = 1;
const REPEATED: i32 = 2;

/// Converted types, for readers that predate logical types.
const UTF8: i32 = 0;
const LIST: i32 = 3;
const TIMESTAMP_MICROS: i32 = 10;
const UINT_64: i32 = 14;
const JSON: i32 = 19;

/// Encodings.
const PLAIN: i32 = 0;
const RLE: i32 = 3;

/// How an attribute's column is written, chosen from every value the page holds under it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Every value an `Int`: `INT64`.
    Int,
    /// Every value a `Float`, or `Int`s and `Float`s: `DOUBLE`.
    Double,
    /// Every value a `Bool`: `BOOLEAN`.
    Bool,
    /// Every value a `Str`: `BYTE_ARRAY` `STRING`.
    Str,
    /// Every value a `DateTime`: `INT64` `TIMESTAMP(MICROS, UTC)`.
    Time,
    /// Every value a `Uint`: `INT64` `UINT_64`.
    Uint,
    /// Every value a `Uuid`: `FIXED_LEN_BYTE_ARRAY(16)` `UUID`.
    Uuid,
    /// Every value `Bytes`: `BYTE_ARRAY`.
    Bytes,
    /// Anything else -- arrays, nulls, mixed kinds -- as `BYTE_ARRAY` `JSON`.
    Json,
}

impl Kind {
    /// The kind of a column holding `values`.
    #[must_use]
    pub fn of<'a>(values: impl IntoIterator<Item = &'a Value>) -> Self {
        let mut kind: Option<Self> = None;
        for v in values {
            let k = match v {
                Value::Int(_) => Self::Int,
                Value::Float(_) => Self::Double,
                Value::Bool(_) => Self::Bool,
                Value::Str(_) => Self::Str,
                Value::DateTime(_) => Self::Time,
                Value::Uint(_) => Self::Uint,
                Value::Uuid(_) => Self::Uuid,
                Value::Bytes(_) => Self::Bytes,
                Value::Array(_) | Value::Null => return Self::Json,
            };
            kind = Some(match (kind, k) {
                (None, k) => k,
                (Some(a), b) if a == b => a,
                (Some(Self::Int | Self::Double), Self::Int | Self::Double) => Self::Double,
                _ => return Self::Json,
            });
        }
        kind.unwrap_or(Self::Json)
    }
}

/// A compact-protocol Thrift writer.
#[derive(Default)]
struct Thrift {
    out: Vec<u8>,
    last: Vec<i16>,
}

const T_TRUE: u8 = 1;
const T_FALSE: u8 = 2;
const T_I32: u8 = 5;
const T_I64: u8 = 6;
const T_BINARY: u8 = 8;
const T_LIST: u8 = 9;
const T_STRUCT: u8 = 12;

impl Thrift {
    fn varint(&mut self, mut v: u64) {
        while v >= 0x80 {
            self.out.push((v as u8) | 0x80);
            v >>= 7;
        }
        self.out.push(v as u8);
    }

    fn zigzag(&mut self, v: i64) {
        self.varint(((v << 1) ^ (v >> 63)) as u64);
    }

    fn field(&mut self, id: i16, ty: u8) {
        let last = self.last.last().copied().unwrap_or(0);
        let delta = id - last;
        if (1..=15).contains(&delta) {
            self.out.push(((delta as u8) << 4) | ty);
        } else {
            self.out.push(ty);
            self.zigzag(i64::from(id));
        }
        if let Some(l) = self.last.last_mut() {
            *l = id;
        }
    }

    fn begin(&mut self) {
        self.last.push(0);
    }

    fn end(&mut self) {
        self.out.push(0);
        self.last.pop();
    }

    fn i32(&mut self, id: i16, v: i32) {
        self.field(id, T_I32);
        self.zigzag(i64::from(v));
    }

    fn i64(&mut self, id: i16, v: i64) {
        self.field(id, T_I64);
        self.zigzag(v);
    }

    fn bool(&mut self, id: i16, v: bool) {
        self.field(id, if v { T_TRUE } else { T_FALSE });
    }

    fn bytes(&mut self, b: &[u8]) {
        self.varint(b.len() as u64);
        self.out.extend_from_slice(b);
    }

    fn string(&mut self, id: i16, s: &str) {
        self.field(id, T_BINARY);
        self.bytes(s.as_bytes());
    }

    fn list(&mut self, id: i16, ty: u8, n: usize) {
        self.field(id, T_LIST);
        if n < 15 {
            self.out.push(((n as u8) << 4) | ty);
        } else {
            self.out.push(0xf0 | ty);
            self.varint(n as u64);
        }
    }

    fn struct_(&mut self, id: i16) {
        self.field(id, T_STRUCT);
        self.begin();
    }
}

/// Writes a column's logical type into its schema element.
type Logical = Box<dyn Fn(&mut Thrift)>;

/// Levels as the RLE/bit-packing hybrid, runs only, behind their 4-byte length.
fn levels(out: &mut Vec<u8>, values: &[u8], max: u8) {
    let width = 8 - max.leading_zeros() as usize;
    let mut body = Vec::new();
    for run in values.chunk_by(|a, b| a == b) {
        let Some(&v) = run.first() else { continue };
        let mut header = (run.len() as u64) << 1;
        while header >= 0x80 {
            body.push((header as u8) | 0x80);
            header >>= 7;
        }
        body.push(header as u8);
        // A level is at most 8 bits wide: its run's value is one byte, or none at width 0.
        if width > 0 {
            body.push(v);
        }
    }
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(&body);
}

/// One column: its schema, and its page's levels and values.
struct Column {
    /// The leaf's path from the root.
    path: Vec<String>,
    physical: i32,
    rep: Vec<u8>,
    def: Vec<u8>,
    max_rep: u8,
    max_def: u8,
    values: Vec<u8>,
    /// Values written: non-null leaves.
    count: usize,
}

impl Column {
    fn page(&self) -> Vec<u8> {
        let mut data = Vec::new();
        if self.max_rep > 0 {
            levels(&mut data, &self.rep, self.max_rep);
        }
        if self.max_def > 0 {
            levels(&mut data, &self.def, self.max_def);
        }
        data.extend_from_slice(&self.values);
        data
    }

    /// Levels written: one a leaf slot, nulls and empty lists included.
    fn slots(&self) -> usize {
        if self.max_def > 0 {
            self.def.len()
        } else {
            self.count
        }
    }
}

/// A schema element: name, physical type (none for a group), repetition, children,
/// converted type and logical type, as Thrift.
struct Element<'a> {
    name: &'a str,
    physical: Option<i32>,
    length: Option<i32>,
    repetition: Option<i32>,
    children: Option<i32>,
    converted: Option<i32>,
    logical: Option<&'a dyn Fn(&mut Thrift)>,
}

fn element(t: &mut Thrift, e: &Element<'_>) {
    t.begin();
    if let Some(p) = e.physical {
        t.i32(1, p);
    }
    if let Some(l) = e.length {
        t.i32(2, l);
    }
    if let Some(r) = e.repetition {
        t.i32(3, r);
    }
    t.string(4, e.name);
    if let Some(c) = e.children {
        t.i32(5, c);
    }
    if let Some(c) = e.converted {
        t.i32(6, c);
    }
    if let Some(l) = e.logical {
        t.struct_(10);
        l(&mut *t);
        t.end();
    }
    t.end();
}

/// An empty struct as a union's member `id`.
fn unit(id: i16) -> impl Fn(&mut Thrift) {
    move |t: &mut Thrift| {
        t.struct_(id);
        t.end();
    }
}

/// `rows` as one Parquet file: the id, each attribute name `rows` hold, and each dense field
/// in `fields` that holds one vector in every row holding it, with `metadata` as the file's
/// key-value pairs. `json` writes a value a [`Kind::Json`] column holds. Returns the file and
/// the fields left out for holding several vectors in a row, or for sharing an attribute's name.
#[must_use]
pub fn write(
    rows: &[Document],
    fields: &[String],
    metadata: &[(String, String)],
    json: &dyn Fn(&Value) -> String,
) -> (Vec<u8>, Vec<String>) {
    let names: BTreeSet<&str> = rows
        .iter()
        .flat_map(|d| d.attrs.keys().map(String::as_str))
        .collect();
    let mut schema: Vec<Vec<u8>> = Vec::new();
    let mut columns: Vec<Column> = Vec::new();

    // The id: required, a string.
    let mut ids = Vec::new();
    for d in rows {
        ids.extend_from_slice(&(d.id.len() as u32).to_le_bytes());
        ids.extend_from_slice(d.id.as_bytes());
    }
    let mut t = Thrift::default();
    element(
        &mut t,
        &Element {
            name: "id",
            physical: Some(BYTE_ARRAY),
            length: None,
            repetition: Some(REQUIRED),
            children: None,
            converted: Some(UTF8),
            logical: Some(&unit(1)),
        },
    );
    schema.push(t.out);
    columns.push(Column {
        path: vec!["id".to_owned()],
        physical: BYTE_ARRAY,
        rep: Vec::new(),
        def: Vec::new(),
        max_rep: 0,
        max_def: 0,
        values: ids,
        count: rows.len(),
    });

    for &name in &names {
        let kind = Kind::of(rows.iter().filter_map(|d| d.attrs.get(name)));
        let mut def = Vec::with_capacity(rows.len());
        let mut values = Vec::new();
        let mut count = 0;
        let mut bools: Vec<bool> = Vec::new();
        for d in rows {
            let Some(v) = d.attrs.get(name) else {
                def.push(0);
                continue;
            };
            def.push(1);
            count += 1;
            match (kind, v) {
                (Kind::Int, Value::Int(n)) | (Kind::Time, Value::DateTime(n)) => {
                    values.extend_from_slice(&n.to_le_bytes());
                }
                (Kind::Uint, Value::Uint(n)) => values.extend_from_slice(&n.to_le_bytes()),
                (Kind::Double, Value::Float(f)) => values.extend_from_slice(&f.to_le_bytes()),
                #[allow(
                    clippy::cast_precision_loss,
                    reason = "stated: an int beside floats is a double, as the API sums them"
                )]
                (Kind::Double, Value::Int(n)) => {
                    values.extend_from_slice(&(*n as f64).to_le_bytes());
                }
                (Kind::Bool, Value::Bool(b)) => bools.push(*b),
                (Kind::Str, Value::Str(s)) => {
                    values.extend_from_slice(&(s.len() as u32).to_le_bytes());
                    values.extend_from_slice(s.as_bytes());
                }
                (Kind::Bytes, Value::Bytes(b)) => {
                    values.extend_from_slice(&(b.len() as u32).to_le_bytes());
                    values.extend_from_slice(b);
                }
                (Kind::Uuid, Value::Uuid(u)) => values.extend_from_slice(u),
                (_, v) => {
                    let s = json(v);
                    values.extend_from_slice(&(s.len() as u32).to_le_bytes());
                    values.extend_from_slice(s.as_bytes());
                }
            }
        }
        if kind == Kind::Bool {
            values = vec![0u8; bools.len().div_ceil(8)];
            for (i, b) in bools.iter().enumerate() {
                if let (true, Some(byte)) = (*b, values.get_mut(i / 8)) {
                    *byte |= 1 << (i % 8);
                }
            }
        }
        let (physical, length, converted, logical): (i32, Option<i32>, Option<i32>, Logical) =
            match kind {
                Kind::Int => (INT64, None, None, Box::new(|_: &mut Thrift| {})),
                Kind::Double => (DOUBLE, None, None, Box::new(|_: &mut Thrift| {})),
                Kind::Bool => (BOOLEAN, None, None, Box::new(|_: &mut Thrift| {})),
                Kind::Str => (BYTE_ARRAY, None, Some(UTF8), Box::new(unit(1))),
                Kind::Bytes => (BYTE_ARRAY, None, None, Box::new(|_: &mut Thrift| {})),
                Kind::Json => (BYTE_ARRAY, None, Some(JSON), Box::new(unit(12))),
                Kind::Uuid => (FIXED_LEN_BYTE_ARRAY, Some(16), None, Box::new(unit(14))),
                Kind::Time => (
                    INT64,
                    None,
                    Some(TIMESTAMP_MICROS),
                    Box::new(|t: &mut Thrift| {
                        t.struct_(8);
                        t.bool(1, true);
                        t.struct_(2);
                        unit(2)(t);
                        t.end();
                        t.end();
                    }),
                ),
                Kind::Uint => (
                    INT64,
                    None,
                    Some(UINT_64),
                    Box::new(|t: &mut Thrift| {
                        t.struct_(10);
                        t.field(1, 3);
                        t.out.push(64);
                        t.bool(2, false);
                        t.end();
                    }),
                ),
            };
        let has_logical = !matches!(kind, Kind::Int | Kind::Double | Kind::Bool | Kind::Bytes);
        let mut t = Thrift::default();
        element(
            &mut t,
            &Element {
                name,
                physical: Some(physical),
                length,
                repetition: Some(OPTIONAL),
                children: None,
                converted,
                logical: has_logical.then_some(&*logical),
            },
        );
        schema.push(t.out);
        columns.push(Column {
            path: vec![name.to_owned()],
            physical,
            rep: Vec::new(),
            def,
            max_rep: 0,
            max_def: 1,
            values,
            count,
        });
    }

    // Dense fields: `optional group f (LIST) { repeated group list { required float element } }`.
    let mut skipped = Vec::new();
    for field in fields {
        let dense = |d: &Document| match d.vectors.get(field.as_str()) {
            Some(VectorField::Dense(v)) => Some(v.clone()),
            _ => None,
        };
        // A field an attribute's name already holds is left out too: a column name is unique.
        if names.contains(field.as_str()) || rows.iter().filter_map(dense).any(|v| v.len() != 1) {
            skipped.push(field.clone());
            continue;
        }
        let (mut rep, mut def, mut values, mut count) = (Vec::new(), Vec::new(), Vec::new(), 0);
        for d in rows {
            match dense(d).and_then(|v| v.into_iter().next()) {
                None => {
                    rep.push(0);
                    def.push(0);
                }
                Some(v) if v.is_empty() => {
                    rep.push(0);
                    def.push(1);
                }
                Some(v) => {
                    for (i, x) in v.iter().enumerate() {
                        rep.push(u8::from(i > 0));
                        def.push(2);
                        values.extend_from_slice(&x.to_le_bytes());
                        count += 1;
                    }
                }
            }
        }
        for e in [
            Element {
                name: field,
                physical: None,
                length: None,
                repetition: Some(OPTIONAL),
                children: Some(1),
                converted: Some(LIST),
                logical: Some(&unit(3)),
            },
            Element {
                name: "list",
                physical: None,
                length: None,
                repetition: Some(REPEATED),
                children: Some(1),
                converted: None,
                logical: None,
            },
            Element {
                name: "element",
                physical: Some(FLOAT),
                length: None,
                repetition: Some(REQUIRED),
                children: None,
                converted: None,
                logical: None,
            },
        ] {
            let mut t = Thrift::default();
            element(&mut t, &e);
            schema.push(t.out);
        }
        columns.push(Column {
            path: vec![field.clone(), "list".to_owned(), "element".to_owned()],
            physical: FLOAT,
            rep,
            def,
            max_rep: 1,
            max_def: 2,
            values,
            count,
        });
    }

    // The column chunks, each one page behind its header.
    let mut file = b"PAR1".to_vec();
    let mut chunks: Vec<(i64, i64, i64)> = Vec::new();
    for c in &columns {
        let data = c.page();
        let mut h = Thrift::default();
        h.begin();
        h.i32(1, 0);
        h.i32(2, data.len() as i32);
        h.i32(3, data.len() as i32);
        h.struct_(5);
        h.i32(1, c.slots() as i32);
        h.i32(2, PLAIN);
        h.i32(3, RLE);
        h.i32(4, RLE);
        h.end();
        h.end();
        let offset = file.len() as i64;
        file.extend_from_slice(&h.out);
        file.extend_from_slice(&data);
        chunks.push((offset, (h.out.len() + data.len()) as i64, c.slots() as i64));
    }

    // The footer.
    let mut m = Thrift::default();
    m.begin();
    m.i32(1, 1);
    m.list(2, T_STRUCT, schema.len() + 1);
    let mut root = Thrift::default();
    element(
        &mut root,
        &Element {
            name: "schema",
            physical: None,
            length: None,
            repetition: None,
            children: Some(schema_tops(&schema, fields, &skipped) as i32),
            converted: None,
            logical: None,
        },
    );
    m.out.extend_from_slice(&root.out);
    for e in &schema {
        m.out.extend_from_slice(e);
    }
    m.i64(3, rows.len() as i64);
    m.list(4, T_STRUCT, 1);
    m.begin();
    m.list(1, T_STRUCT, columns.len());
    for (c, (offset, len, slots)) in columns.iter().zip(&chunks) {
        m.begin();
        m.i64(2, *offset);
        m.struct_(3);
        m.i32(1, c.physical);
        m.list(2, T_I32, 2);
        m.zigzag(i64::from(PLAIN));
        m.zigzag(i64::from(RLE));
        m.list(3, T_BINARY, c.path.len());
        for p in &c.path {
            m.bytes(p.as_bytes());
        }
        m.i32(4, 0);
        m.i64(5, *slots);
        m.i64(6, *len);
        m.i64(7, *len);
        m.i64(9, *offset);
        m.end();
        m.end();
    }
    m.i64(2, chunks.iter().map(|c| c.1).sum());
    m.i64(3, rows.len() as i64);
    m.end();
    if !metadata.is_empty() {
        m.list(5, T_STRUCT, metadata.len());
        for (k, v) in metadata {
            m.begin();
            m.string(1, k);
            m.string(2, v);
            m.end();
        }
    }
    m.string(6, concat!("pstore version ", env!("CARGO_PKG_VERSION")));
    m.end();
    file.extend_from_slice(&m.out);
    file.extend_from_slice(&(m.out.len() as u32).to_le_bytes());
    file.extend_from_slice(b"PAR1");
    (file, skipped)
}

/// The root's children: the id, every attribute, and every field written.
fn schema_tops(schema: &[Vec<u8>], fields: &[String], skipped: &[String]) -> usize {
    // Each written field is three elements, every other top-level column one.
    let written = fields.len() - skipped.len();
    schema.len() - 2 * written
}
