//! Facets: an aggregation beside a ranked query, over the same filter (M65).
//!
//! ⚠️ **Facets count the filtered set, not what the ranking matched.** The aggregation is the
//! M12 query with the ranked query's `filters`, `as_of` and consistency, so it is built as
//! one and planned by the same code: every limit and refusal is M12's, with its message
//! prefixed `facets:`.

use crate::types::QueryRequest;
use crate::{ApiError, Plan, parse_query, plan};

/// Groups a facet holds when it names no `top_k`.
const DEFAULT_TOP_K: u64 = 10;

/// The aggregation `req.facets` asks for, planned, or `None` for a query without facets.
///
/// # Errors
/// A `400` naming `facets` for every refusal.
pub(crate) fn derive(req: &QueryRequest) -> Result<Option<(QueryRequest, Plan)>, ApiError> {
    let Some(raw) = &req.facets else {
        return Ok(None);
    };
    let refuse = |why: String| ApiError::bad_request(format!("facets: {why}"));
    let o = raw
        .as_object()
        .ok_or_else(|| refuse("is {\"aggregate_by\", \"group_by\", \"top_k\"}".to_owned()))?;
    if let Some(k) = o
        .keys()
        .find(|k| !matches!(k.as_str(), "aggregate_by" | "group_by" | "top_k"))
    {
        return Err(refuse(format!(
            "{k:?} is not a facets field; aggregate_by, group_by and top_k are"
        )));
    }
    if req.rank_by.is_some() || req.aggregate_by.is_some() {
        return Err(refuse(
            "beside a relevance ranking only: it cannot be combined with rank_by or \
             aggregate_by"
                .to_owned(),
        ));
    }
    if req.vector.is_none() && req.multi.is_none() && req.text.is_none() && req.sparse.is_none() {
        return Err(refuse(
            "needs a ranking leg (vector, multi, text or sparse); an aggregation alone is \
             aggregate_by"
                .to_owned(),
        ));
    }
    let Some(aggregate_by) = o.get("aggregate_by") else {
        return Err(refuse("needs aggregate_by".to_owned()));
    };
    let top_k = match o.get("top_k") {
        None => DEFAULT_TOP_K,
        Some(v) => v
            .as_u64()
            .ok_or_else(|| refuse("top_k is a whole number".to_owned()))?,
    };
    let mut body = serde_json::Map::new();
    body.insert("aggregate_by".to_owned(), aggregate_by.clone());
    body.insert("top_k".to_owned(), top_k.into());
    if let Some(g) = o.get("group_by") {
        body.insert("group_by".to_owned(), g.clone());
    }
    if let Some(f) = &req.filters {
        body.insert("filters".to_owned(), f.clone());
    }
    if let Some(e) = req.as_of {
        body.insert("as_of".to_owned(), e.into());
    }
    if let Some(c) = &req.consistency {
        body.insert("consistency".to_owned(), c.clone());
    }
    if let Some(m) = &req.max_staleness_ms {
        body.insert("max_staleness_ms".to_owned(), m.clone());
    }
    let prefixed = |mut e: ApiError| {
        e.message = format!("facets: {}", e.message);
        e
    };
    let facet_req = parse_query(serde_json::Value::Object(body)).map_err(prefixed)?;
    let facet_plan = plan(&facet_req).map_err(prefixed)?;
    Ok(Some((facet_req, facet_plan)))
}

/// A facet's answer as the response carries it: `{"aggregations": …}` or
/// `{"aggregation_groups": […]}`, M12's shapes.
pub(crate) fn render(
    aggregations: Option<serde_json::Map<String, serde_json::Value>>,
    groups: Option<Vec<serde_json::Map<String, serde_json::Value>>>,
) -> serde_json::Value {
    match (aggregations, groups) {
        (Some(a), _) => serde_json::Value::Object(a),
        (None, Some(g)) => serde_json::json!(g),
        (None, None) => serde_json::Value::Null,
    }
}
