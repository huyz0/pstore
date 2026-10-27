//! The wire shapes — `api-design.md`'s request and response bodies, minus what M7c defers.
//!
//! ⚠️ `vector` is a JSON array of `f32`. D-36 says vectors must always be sendable as raw
//! binary and that JSON encoding of a 768-dim vector is ~10 KB against 3 KB raw, which is a
//! top-three CPU cost. Accepted for v1 by the same document that names it; the field keeps
//! its name so a base64 variant is an addition rather than a move.

use serde::{Deserialize, Serialize};

/// What a request cost the tenant, in blob requests and bytes.
///
/// ⚠️ **This request's own**, as a difference of the tenant's monotone counters — see
/// `Api::spend`. Exact for one request in flight, cross-attributed under concurrency.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Cost {
    /// Read-class requests.
    pub blob_reads: u64,
    /// Write-class requests. A PUT costs 12.5 GETs, which is why this is reported separately.
    pub blob_writes: u64,
    /// LISTs. **Always zero**, and reported so that a regression is visible to the caller
    /// rather than only to a test.
    pub blob_lists: u64,
    /// Bytes read.
    pub bytes_read: u64,
    /// Bytes written.
    pub bytes_written: u64,
}

/// How durable a write must be before it is acknowledged.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Durability {
    /// Buffered in this process, visible to its queries, **not yet in the blob store**.
    #[default]
    Batched,
    /// Written as one bundle object before the response. `RA = 1 W` for the batch.
    Durable,
    /// Durable, then answered only once a HEAD commit has folded the whole request, with
    /// what the fold decided (M129). Refused unless the process enables it.
    Applied,
}

/// One document, as a client sends it.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DocumentIn {
    /// The client's identifier.
    pub id: String,
    /// The dense vector: a JSON array, or base64 of little-endian `f32`s (M9d).
    #[serde(deserialize_with = "vector")]
    pub vector: Vec<f32>,
    /// Named fields of several vectors each (M61): `{"late": [vector, …]}`, each vector
    /// spelled as `vector` is. Searched by a query's `multi` leg.
    #[serde(default, deserialize_with = "named_vectors")]
    pub vectors: std::collections::BTreeMap<String, Vec<Vec<f32>>>,
    /// Free-text attribute, indexed for BM25 when the engine's text field names it.
    #[serde(default)]
    pub text: Option<String>,
    /// Typed attributes: a JSON integer (`i64`) or string each (M9a), a float or bool
    /// (M9h.1), an array of those (M9h.2), or a datetime the write's `schema` declares
    /// (M9h.3). Anything else is refused at the door rather than coerced — see `lib.rs`'s
    /// `scalar`.
    #[serde(default)]
    pub attributes: std::collections::BTreeMap<String, serde_json::Value>,
}

/// `PUT /v1/indexes/{id}/documents`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WriteRequest {
    /// Defaults to `batched`. ⚠️ An unknown value is **refused**, never downgraded.
    #[serde(default)]
    pub durability: Durability,
    /// The batch. Batch-first: the cheap thing is the natural thing. Each is an **upsert**
    /// (M9c): a later write of an id replaces the earlier one, and a later duplicate in this
    /// batch wins.
    #[serde(default)]
    pub documents: Vec<DocumentIn>,
    /// Ids to delete, applied **after** `documents` (M9c). Either list may be empty, not both.
    #[serde(default)]
    pub deletes: Vec<String>,
    /// How the index compares vectors (M9d): `cosine_distance`, `euclidean_squared`, or
    /// `dot_product`, the default and every index's metric before M9d. Refused if unknown.
    #[serde(default)]
    pub distance_metric: Option<String>,
    /// Which of this batch's attributes are datetimes (M9h.3): `{"<attr>": "datetime"}`.
    /// **Per request**: a value carries its type, and nothing is recorded for the index, so a
    /// write that omits it stores plain strings.
    #[serde(default)]
    pub schema: Option<serde_json::Value>,
    /// Patches (M13): `[{"id", "attributes": {..}}]`, each setting the attributes it names --
    /// `null` removes one -- on the id's current version at the fold. Taken raw, so a patch
    /// carrying a vector is refused rather than ignored.
    #[serde(default)]
    pub patch_rows: Vec<serde_json::Value>,
    /// The same patches in columns (M13): `{"id": [..], "<attr>": [..]}`, one value per id.
    #[serde(default)]
    pub patch_columns: Option<serde_json::Value>,
    /// A filter each of `documents` applies only if the id's current version admits (M13).
    #[serde(default)]
    pub upsert_condition: Option<serde_json::Value>,
    /// A filter each patch applies only if the id's current version admits (M13).
    #[serde(default)]
    pub patch_condition: Option<serde_json::Value>,
    /// A filter each of `deletes` applies only if the id's current version admits (M13).
    #[serde(default)]
    pub delete_condition: Option<serde_json::Value>,
    /// Deletes every row whose current version at the fold admits this filter (M13.2), after
    /// the request's other operations.
    #[serde(default)]
    pub delete_by_filter: Option<serde_json::Value>,
    /// `{"filters": .., "attributes": {..}}`: patches every row whose current version at the
    /// fold admits the filter (M13.2), before `delete_by_filter`.
    #[serde(default)]
    pub patch_by_filter: Option<serde_json::Value>,
    /// `delete_by_filter` may apply to the bound's rows and say `rows_remaining`, rather than
    /// be refused past the bound (M137). Needs `durability: "applied"`.
    #[serde(default)]
    pub delete_by_filter_allow_partial: bool,
    /// The same for `patch_by_filter` (M137), counting only rows it would change.
    #[serde(default)]
    pub patch_by_filter_allow_partial: bool,
    /// Makes this index a branch of the one named (M16): its folded state at the commit, sharing
    /// every segment. **Alone** in its request.
    #[serde(default)]
    pub branch_from_namespace: Option<String>,
    /// The same operation as `branch_from_namespace` (M16): segments are immutable, so a
    /// physical copy would buy nothing a branch does not.
    #[serde(default)]
    pub copy_from_namespace: Option<String>,
    /// An applied write's ask for the ids its operations changed (M130). Refused beside any
    /// other durability, beside a by-filter operation, or past 10,000 distinct ids.
    #[serde(default)]
    pub affected_ids: bool,
    /// An applied write's idempotency key (M132): of the attempts carrying one, at most one
    /// applies. Issued by `POST /v1/indexes/{index}/idempotency-keys`.
    #[serde(default)]
    pub idempotency_key: Option<String>,
}

/// A vector as the wire carries it (M9d): an array, or base64 of little-endian `f32`s.
enum VectorIn {
    Array(Vec<f32>),
    Base64(String),
}

/// What `#[serde(untagged)]` refused anything else with, kept word for word.
const NOT_A_VECTOR: &str = "data did not match any variant of untagged enum VectorIn";

/// ⚠️ **By hand, not `#[serde(untagged)]`** (M119): an untagged enum buffers its input into
/// serde's `Content` -- about 32 bytes an element -- and then tries each variant over the
/// buffer, so every vector of every write was parsed twice. This reads a sequence straight
/// into `f32`s, each cast from the number as the derived `Vec<f32>` cast it, and a string as
/// base64; anything else is refused with the derive's own message.
impl<'de> Deserialize<'de> for VectorIn {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct Visit;
        impl<'de> serde::de::Visitor<'de> for Visit {
            type Value = VectorIn;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("an array of numbers or a base64 string")
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> Result<VectorIn, A::Error> {
                let mut v = Vec::with_capacity(seq.size_hint().unwrap_or(0).min(1 << 16));
                loop {
                    match seq.next_element::<f32>() {
                        Ok(Some(x)) => v.push(x),
                        Ok(None) => return Ok(VectorIn::Array(v)),
                        Err(_) => return Err(serde::de::Error::custom(NOT_A_VECTOR)),
                    }
                }
            }
            fn visit_str<E: serde::de::Error>(self, s: &str) -> Result<VectorIn, E> {
                Ok(VectorIn::Base64(s.to_owned()))
            }
            fn visit_string<E: serde::de::Error>(self, s: String) -> Result<VectorIn, E> {
                Ok(VectorIn::Base64(s))
            }
            fn visit_bool<E: serde::de::Error>(self, _: bool) -> Result<VectorIn, E> {
                Err(E::custom(NOT_A_VECTOR))
            }
            fn visit_i64<E: serde::de::Error>(self, _: i64) -> Result<VectorIn, E> {
                Err(E::custom(NOT_A_VECTOR))
            }
            fn visit_u64<E: serde::de::Error>(self, _: u64) -> Result<VectorIn, E> {
                Err(E::custom(NOT_A_VECTOR))
            }
            fn visit_f64<E: serde::de::Error>(self, _: f64) -> Result<VectorIn, E> {
                Err(E::custom(NOT_A_VECTOR))
            }
            fn visit_unit<E: serde::de::Error>(self) -> Result<VectorIn, E> {
                Err(E::custom(NOT_A_VECTOR))
            }
            fn visit_none<E: serde::de::Error>(self) -> Result<VectorIn, E> {
                Err(E::custom(NOT_A_VECTOR))
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(self, _: A) -> Result<VectorIn, A::Error> {
                Err(serde::de::Error::custom(NOT_A_VECTOR))
            }
        }
        d.deserialize_any(Visit)
    }
}

/// A vector from either spelling. ⚠️ **Refused, never repaired**: a length that is not whole
/// `f32`s is not truncated, and a non-finite component -- which base64 can carry and JSON
/// cannot -- is not stored.
fn decode_vector<E: serde::de::Error>(raw: VectorIn) -> Result<Vec<f32>, E> {
    use base64::Engine as _;
    let v = match raw {
        VectorIn::Array(v) => v,
        VectorIn::Base64(s) => {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(s.as_bytes())
                .map_err(|e| E::custom(format!("a vector that is not base64: {e}")))?;
            if bytes.len() % 4 != 0 {
                return Err(E::custom(format!(
                    "a base64 vector of {} bytes, which is not a whole number of f32s",
                    bytes.len()
                )));
            }
            bytes
                .chunks_exact(4)
                .map(|c| {
                    <[u8; 4]>::try_from(c)
                        .map(f32::from_le_bytes)
                        .map_err(|e| E::custom(e.to_string()))
                })
                .collect::<Result<_, _>>()?
        }
    };
    if v.iter().any(|x| !x.is_finite()) {
        return Err(E::custom("a vector with a non-finite component"));
    }
    Ok(v)
}

fn vector<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<f32>, D::Error> {
    decode_vector(VectorIn::deserialize(d)?)
}

fn many_vectors<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<Vec<f32>>, D::Error> {
    Vec::<VectorIn>::deserialize(d)?
        .into_iter()
        .map(decode_vector)
        .collect()
}

type Named = std::collections::BTreeMap<String, Vec<VectorIn>>;

fn named_vectors<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<std::collections::BTreeMap<String, Vec<Vec<f32>>>, D::Error> {
    Named::deserialize(d)?
        .into_iter()
        .map(|(name, vs)| {
            vs.into_iter()
                .map(decode_vector)
                .collect::<Result<_, _>>()
                .map(|vs| (name, vs))
        })
        .collect()
}

/// A query's `multi` leg (M61): MaxSim of `vectors` over a named field's.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MultiIn {
    /// The named field to search.
    pub field: String,
    /// The query's vectors, each spelled as `vector` is.
    #[serde(deserialize_with = "many_vectors")]
    pub vectors: Vec<Vec<f32>>,
    /// `"exact"` (the default) or `"fde"`: candidates from the fixed-dimensional encoding
    /// (M67).
    #[serde(default)]
    pub mode: Option<serde_json::Value>,
    /// With `mode: "fde"`, `"exact"` re-scores the candidates by MaxSim (M67).
    #[serde(default)]
    pub rerank: Option<serde_json::Value>,
    /// The candidates a rerank re-scores a segment (M67).
    #[serde(default)]
    pub rerank_window: Option<serde_json::Value>,
    /// With `mode: "fde"`, the clustered lists read a segment that has them (M72).
    #[serde(default)]
    pub probes: Option<serde_json::Value>,
}

fn some_vector<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Vec<f32>>, D::Error> {
    Option::<VectorIn>::deserialize(d)?
        .map(decode_vector)
        .transpose()
}

/// The answer to a write.
#[derive(Debug, Clone, Serialize)]
pub struct WriteResponse {
    /// This engine's last **committed** epoch. ⚠️ Not a read of HEAD: a write costs the
    /// requests the cost model says it costs, and asking the store what epoch it is on would
    /// add one to every batch.
    pub epoch: u64,
    /// Documents accepted.
    pub documents_written: usize,
    /// Ids a delete was **requested** for -- not ids found, which would need a read (M9c).
    pub documents_deleted: usize,
    /// Ids a patch was **requested** for (M13), as `documents_deleted` counts.
    pub documents_patched: usize,
    /// Whether the batch is in the blob store or only in this process.
    pub durable: bool,
    /// What it cost.
    pub cost: Cost,
    /// The session token, as the `x-pstore-session` header carries it (M11.1).
    pub session: String,
    /// `"applied"` for an applied write (M129); absent otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completion: Option<&'static str>,
    /// An applied write's operation id, 32 hex digits (M129).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operation: Option<String>,
    /// What the folds decided for an applied write (M129), or `null` when that can no
    /// longer be read -- never invented counts.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub applied: Option<Option<AppliedOut>>,
    /// `true` beside `applied: null` (M129).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outcome_unavailable: Option<bool>,
    /// `true` when another attempt of the write's idempotency key applied, and `applied` is
    /// that attempt's (M132).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub replayed: Option<bool>,
}

/// A fresh idempotency key (M132).
#[derive(Debug, Serialize, Deserialize)]
pub struct IdempotencyKeyResponse {
    /// `<e0>-<32 hex>`: the epoch it was issued at, and 128 random bits.
    pub idempotency_key: String,
}

/// An applied write's outcome (M129), in rows; `skipped` and `rejected` in operations.
#[derive(Debug, Clone, Serialize)]
pub struct AppliedOut {
    /// Rows an upsert wrote.
    pub upserted: u64,
    /// Rows a delete removed: a version existed.
    pub deleted: u64,
    /// Rows a patch changed.
    pub patched: u64,
    /// Operations that changed nothing.
    pub skipped: u64,
    /// Operations a reject, width, replica or drop pass discarded, and by-filter operations
    /// past their row bound (M136).
    pub rejected: u64,
    /// An `allow_partial` by-filter operation left rows it would change (M137): send the
    /// write again, with a new idempotency key if it carries one, to resume. Absent when
    /// none did.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rows_remaining: Option<bool>,
    /// The distinct ids each kind changed, sorted, when the write asked (M130).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ids: Option<AffectedIds>,
}

/// An applied write's affected ids (M130): distinct and sorted, so a list can be shorter than
/// its count, and an id changed by two kinds is in both.
#[derive(Debug, Clone, Serialize)]
pub struct AffectedIds {
    /// Ids an upsert wrote.
    pub upserted: Vec<String>,
    /// Ids a delete removed.
    pub deleted: Vec<String>,
    /// Ids a patch changed.
    pub patched: Vec<String>,
}

/// `POST /v1/indexes/{id}/query`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QueryRequest {
    /// The dense leg's query vector, as a document's is written (M9d).
    #[serde(default, deserialize_with = "some_vector")]
    pub vector: Option<Vec<f32>>,
    /// The dense leg's field. Defaults to the engine's default vector field.
    #[serde(default)]
    pub field: Option<String>,
    /// A window of the ranking diversified by maximal marginal relevance (M64).
    #[serde(default)]
    pub diversify: Option<serde_json::Value>,
    /// An aggregation beside a ranked query, over its filter (M65).
    #[serde(default)]
    pub facets: Option<serde_json::Value>,
    /// The query's terms marked in each result's text (M63).
    #[serde(default)]
    pub highlight: Option<serde_json::Value>,
    /// Attribute terms that rescore a window of the ranking's candidates (M62).
    #[serde(default)]
    pub boost: Option<serde_json::Value>,
    /// The `multi` leg (M61): MaxSim over a named field of several vectors a document.
    #[serde(default)]
    pub multi: Option<MultiIn>,
    /// The BM25 leg's query text -- or several, each its own leg (M9g).
    #[serde(default)]
    pub text: Option<TextIn>,
    /// How the legs combine (M9g.1): `{"rrf": {"k", "weights"}}`, the default being `k = 60`
    /// and every weight 1. Parsed by `lib.rs`'s `fusion`.
    #[serde(default)]
    pub fusion: Option<serde_json::Value>,
    /// Answer as the index stood at this epoch. ⚠️ Bounded by what GC has reaped: below the
    /// horizon the request is **refused** rather than answered with a short index.
    #[serde(default)]
    pub as_of: Option<u64>,
    /// How many results. **Zero is refused**: a query that can return nothing is a request
    /// nobody meant to make.
    #[serde(default = "default_top_k")]
    pub top_k: usize,
    /// Which attributes each result carries: all, none, or those named. Omitted, with no
    /// `exclude_attributes`, a row carries no `attributes` key at all.
    #[serde(default)]
    pub include_attributes: Option<IncludeAttributes>,
    /// Names removed from what `include_attributes` selects; alone, "all except these".
    #[serde(default)]
    pub exclude_attributes: Option<Vec<String>>,
    /// turbopuffer's filter arrays, parsed by `lib.rs`'s `predicate` (M9b).
    #[serde(default)]
    pub filters: Option<serde_json::Value>,
    /// Every row scored at full precision -- turbopuffer's `kNN` (M9d). Reads every vector of
    /// the index, at the default depth.
    #[serde(default)]
    pub exact: bool,
    /// `[attr, "asc" | "desc"]`: every admitted document in that order, instead of a relevance
    /// ranking (M9e). `attr` may be `id`, which is how an index is exported, a page at a time.
    /// Or a list of 1 to 8 such pairs (M140), ordered key by key, then by id; `id` may only
    /// be the last.
    #[serde(default)]
    pub rank_by: Option<serde_json::Value>,
    /// `{label: ["Count" | "Sum", attr]}`: counts and sums instead of rows (M12). Parsed by
    /// `lib.rs`'s `aggregation`.
    #[serde(default)]
    pub aggregate_by: Option<serde_json::Value>,
    /// Attributes whose values group an aggregation (M12). Refused without `aggregate_by`.
    #[serde(default)]
    pub group_by: Option<serde_json::Value>,
    /// Documents of a `rank_by` order to skip (M9e). Refused without `rank_by`.
    #[serde(default)]
    pub offset: Option<usize>,
    /// `"eventual"` (the default, and `null`), `"strong"` (M9i.2), `"session"` (M11.1) or
    /// `"bounded"` (M11.2). Anything else is refused.
    #[serde(default)]
    pub consistency: Option<serde_json::Value>,
    /// With `consistency: "bounded"` only: how old a HEAD may be served (M11.2). Taken raw,
    /// so a string or a fraction is refused rather than coerced.
    #[serde(default)]
    pub max_staleness_ms: Option<serde_json::Value>,
}

/// `include_attributes`: `false`, `true`, or a list of names — turbopuffer's spelling.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub enum IncludeAttributes {
    /// `true` for every attribute, `false` for none.
    All(bool),
    /// Only these, where a row has them.
    Named(Vec<String>),
}

fn default_top_k() -> usize {
    10
}

/// One ranked result.
#[derive(Debug, Clone, Serialize)]
pub struct ResultRow {
    /// The document's id.
    pub id: String,
    /// Its fused score. Absent for a `rank_by` order (M9e), which has none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub score: Option<f32>,
    /// Its distance under the index's metric, when the query's dense leg scored it (M9d).
    /// Without `exact` it is the ranking rung's estimate.
    #[serde(rename = "$dist", skip_serializing_if = "Option::is_none")]
    pub dist: Option<f32>,
    /// Its attributes, **only when the query asked** — absent otherwise, so a query that
    /// did not ask gets exactly the response it always did.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attributes: Option<std::collections::BTreeMap<String, serde_json::Value>>,
    /// The highlighted text, when the query asked for `highlight` and the attribute is a
    /// string (M63). The query's terms are wrapped in `pre` and `post`, and nothing is escaped.
    #[serde(rename = "$highlight", skip_serializing_if = "Option::is_none")]
    pub highlight: Option<String>,
}

/// What the answer cost and how fresh it is.
#[derive(Debug, Clone, Serialize)]
pub struct QueryMeta {
    /// The epoch the answer was computed against: the HEAD it read, or its `as_of` (M10).
    /// Rows from this process's unfolded writes are newer, and counted in `unfolded_hits`.
    pub epoch: u64,
    /// Hits served from the freshness layer rather than from a segment.
    ///
    /// ⚠️ The honest half of `api-design.md` principle 3: a caller can see that an answer
    /// came from rows this process has not folded, and therefore that another process would
    /// not have returned it.
    pub unfolded_hits: usize,
    /// What it cost.
    pub cost: Cost,
    /// The level served: `eventual` or `strong` (M9i.2).
    pub consistency: &'static str,
    /// The session token, as the `x-pstore-session` header carries it (M11.1).
    pub session: String,
    /// How old the HEAD served was: zero unless a `bounded` read took it from the cache
    /// (M11.2). Design rule 10: every response carries the epoch and staleness it was served at.
    pub staleness_ms: u64,
    /// The epoch the facets were read at (M65): `epoch`, unless a fold landed between the
    /// two concurrent reads.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub facets_epoch: Option<u64>,
}

/// A query's text: one string, or several, each a BM25 leg (M9g).
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum TextIn {
    /// One leg.
    One(String),
    /// A leg each.
    Many(Vec<String>),
}

impl TextIn {
    /// The legs' queries, in request order.
    #[must_use]
    pub fn queries(&self) -> Vec<&str> {
        match self {
            Self::One(q) => vec![q.as_str()],
            Self::Many(qs) => qs.iter().map(String::as_str).collect(),
        }
    }
}

/// The answer to `{"queries": [...]}` (M9g): each query's results in request order, and one
/// cost -- the spend of queries that ran concurrently is not attributable to one of them.
#[derive(Debug, Clone, Serialize)]
pub struct MultiQueryResponse {
    /// Each query's ranking, in request order.
    pub results: Vec<Vec<ResultRow>>,
    /// Freshness per query, and the request's cost.
    pub meta: MultiQueryMeta,
}

/// [`MultiQueryResponse`]'s meta.
#[derive(Debug, Clone, Serialize)]
pub struct MultiQueryMeta {
    /// The epoch each query was answered at.
    pub epochs: Vec<u64>,
    /// Each query's hits from unfolded rows.
    pub unfolded_hits: Vec<usize>,
    /// What the whole request cost.
    pub cost: Cost,
    /// Each query's level served (M9i.2).
    pub consistencies: Vec<&'static str>,
    /// The session token, merged over every query (M11.1).
    pub session: String,
    /// Each query's `staleness_ms` (M11.2).
    pub staleness_ms: Vec<u64>,
}

/// The answer to a query.
#[derive(Debug, Clone, Serialize)]
pub struct QueryResponse {
    /// The ranking. Empty for an aggregation (M12).
    pub results: Vec<ResultRow>,
    /// An aggregation without `group_by`: each label's total (M12).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub aggregations: Option<serde_json::Map<String, serde_json::Value>>,
    /// An aggregation with `group_by`: each group's key and totals, ascending by key (M12).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub aggregation_groups: Option<Vec<serde_json::Map<String, serde_json::Value>>>,
    /// A ranked query's facets (M65): M12's `aggregations` object, or its groups, over the
    /// query's filter -- the whole filtered set, not the ranked window.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub facets: Option<serde_json::Value>,
    /// Cost and freshness.
    pub meta: QueryMeta,
}

/// What an index's rows must look like. Inferred by its first fold, immutable afterwards.
#[derive(Debug, Clone, Serialize)]
pub struct Schema {
    /// Components in every vector, as a client writes them.
    pub dims: u32,
    /// How vectors are compared (M9d).
    pub distance_metric: &'static str,
    /// The attribute the text index is built over, or empty if the index carries no text.
    pub text_field: String,
    /// The analyzer and BM25 parameters its text is indexed and scored with (M14).
    pub full_text_search: serde_json::Value,
    /// The attributes whose trigrams are sketched (M15.2), sorted.
    pub regex: Vec<String>,
    /// Each named dense field's width, as a client writes it (M98); absent when none is known.
    #[serde(skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub vectors: std::collections::BTreeMap<String, u32>,
}

/// One index, as `GET /v1/indexes/{id}` reports it.
#[derive(Debug, Clone, Serialize)]
pub struct IndexSummary {
    /// Its name.
    pub index: String,
    /// Segments HEAD names for it.
    pub segments: u64,
    /// Rows in those segments. ⚠️ Folded rows only; `unfolded` is the other half.
    pub documents: u64,
    /// HEAD's epoch.
    pub epoch: u64,
    /// Whether this process holds unfolded rows for it.
    pub unfolded: bool,
    /// What its rows must look like. `null` until its first fold records it.
    pub schema: Option<Schema>,
    /// ⚠️ Rows that were **acknowledged and then discarded** at a fold because they
    /// contradicted the schema. Reported because a discard nobody can see is indistinguishable
    /// from a bug — and because the alternative to discarding was stopping the tenant.
    pub rejected_rows: u64,
    /// Of those, the rows still set aside (M25): `GET …/quarantine` exports them, and
    /// `DELETE …/quarantine` discards them.
    pub quarantined_rows: u64,
    /// The epoch of the last commit that rewrote its segments or their delete vectors, or
    /// `null` before its first fold (M9f).
    pub updated_epoch: Option<u64>,
    /// `documents`, under turbopuffer's name (M9f).
    pub approx_row_count: u64,
    /// What the summary cost.
    pub cost: Cost,
}

/// `GET /v1/indexes?prefix=&cursor=&page_size=` (M9f).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ListParams {
    /// Only names beginning with this.
    #[serde(default)]
    pub prefix: Option<String>,
    /// Only names strictly after this.
    #[serde(default)]
    pub cursor: Option<String>,
    /// At most this many, 1 to 1000; 100 when absent. A string, so a bad value is this API's
    /// refusal rather than the extractor's.
    #[serde(default)]
    pub page_size: Option<String>,
}

/// `GET /v1/indexes`.
#[derive(Debug, Clone, Serialize)]
pub struct IndexList {
    /// Every index HEAD names, **unioned with this process's unfolded ones**, in name order:
    /// one page of them (M9f).
    pub indexes: Vec<String>,
    /// The cursor for the next page -- the last name on this one -- or `null` on the last.
    pub next_cursor: Option<String>,
    /// What the enumeration cost. Zero LISTs, which is the claim worth reporting.
    pub cost: Cost,
}

/// `POST /v1/admin/gc`.
#[derive(Debug, Clone, Serialize)]
pub struct GcResponse {
    /// The epoch the reap committed.
    pub epoch: u64,
    /// Objects deleted.
    pub reaped: usize,
    /// ⚠️ The oldest epoch `as_of` can still answer. Reported because a caller that has been
    /// time-travelling needs to learn where its history now ends, and the only alternative to
    /// telling it is letting it find out by being refused.
    pub reaped_before: u64,
    /// What it cost.
    pub cost: Cost,
}

/// `POST /v1/admin/fold`.
#[derive(Debug, Clone, Serialize)]
pub struct FoldResponse {
    /// The epoch the fold committed.
    pub epoch: u64,
    /// What it cost.
    pub cost: Cost,
}

/// `POST /v1/indexes/{index}/warm` (M21).
#[derive(Debug, Clone, Serialize)]
pub struct WarmResponse {
    /// The segments whose metadata is now cached.
    pub segments: usize,
    /// The sidecars fetched beside their footers: delete vectors, centroid tables and
    /// dictionaries.
    pub fetched: usize,
    /// What it cost, as a query reports it.
    pub meta: WarmMeta,
}

/// [`WarmResponse`]'s `meta`.
#[derive(Debug, Clone, Serialize)]
pub struct WarmMeta {
    /// The blob requests the warm spent, billed to the tenant.
    pub cost: Cost,
}

/// The body of every refusal.
#[derive(Debug, Clone, Serialize)]
pub struct ErrorBody {
    /// The refusal.
    pub error: ErrorDetail,
}

impl ErrorBody {
    pub(crate) fn new(code: &'static str, message: String, retryable: bool) -> Self {
        Self {
            error: ErrorDetail {
                code,
                message,
                retryable,
            },
        }
    }
}

/// A stable code, a human message, and whether retrying may help.
#[derive(Debug, Clone, Serialize)]
pub struct ErrorDetail {
    /// Stable across releases; the field a client switches on.
    pub code: &'static str,
    /// For a human reading a log.
    pub message: String,
    /// ⚠️ Derived from the status, so no two rows of the table can disagree.
    pub retryable: bool,
}

/// A duty this server does **not** perform.
///
/// ⚠️ **The list is a constant in code, not a paragraph in a document**, because a document
/// cannot be compared against anything. `scripts/byoc.sh` asserts that the ids served at
/// `/v1/admin/duties` are exactly the ids documented under `docs/deploy.md`'s
/// `## Unscheduled duties` — so a duty added here fails the gate until the deployment
/// document says what an operator must do instead. A grep for four fixed phrases would have
/// tested only that the commit which wrote the document also wrote the grep.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Duty {
    /// A stable lower-case token. ⚠️ Lower-case and hyphens only: it is extracted from
    /// `deploy.md` with a character class, and an id outside it would make the gate match
    /// nothing and pass.
    pub id: &'static str,
    /// What an operator must do instead, in one line.
    pub instead: &'static str,
}

/// What `/v1/admin/duties` answers.
#[derive(Debug, Clone, Serialize)]
pub struct Duties {
    /// Every duty, in the order the constant declares them.
    pub duties: &'static [Duty],
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    reason = "assertions in tests are the reporting mechanism"
)]
mod tests {
    use super::*;

    /// The derive `VectorIn` replaced (M119), verbatim, under its own name: the name is in
    /// its message.
    mod derived {
        #[derive(serde::Deserialize)]
        #[serde(untagged)]
        pub(super) enum VectorIn {
            Array(Vec<f32>),
            Base64(String),
        }
    }
    use derived::VectorIn as Derived;

    fn by_hand(v: &serde_json::Value) -> Result<String, String> {
        match serde_json::from_value::<VectorIn>(v.clone()) {
            Ok(VectorIn::Array(a)) => Ok(format!(
                "array {:?}",
                a.iter().map(|x| x.to_bits()).collect::<Vec<_>>()
            )),
            Ok(VectorIn::Base64(s)) => Ok(format!("base64 {s}")),
            Err(e) => Err(e.to_string()),
        }
    }

    fn derived(v: &serde_json::Value) -> Result<String, String> {
        match serde_json::from_value::<Derived>(v.clone()) {
            Ok(Derived::Array(a)) => Ok(format!(
                "array {:?}",
                a.iter().map(|x| x.to_bits()).collect::<Vec<_>>()
            )),
            Ok(Derived::Base64(s)) => Ok(format!("base64 {s}")),
            Err(e) => Err(e.to_string()),
        }
    }

    #[test]
    fn the_hand_written_vector_is_the_derived_one() {
        for raw in [
            "[]",
            "[1, 2.5, -3]",
            "[0.1, 1e30, 1e-45, 123456789012]",
            "[18446744073709551615, -9223372036854775808]",
            "\"AACAPw==\"",
            "\"\"",
            "[1, \"a\"]",
            "[null]",
            "[[1]]",
            "[true]",
            "{}",
            "{\"a\": 1}",
            "true",
            "1",
            "-1",
            "1.5",
            "null",
        ] {
            let v: serde_json::Value = serde_json::from_str(raw).unwrap();
            assert_eq!(by_hand(&v), derived(&v), "{raw}");
        }
    }
}
