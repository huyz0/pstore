//! `meta.explain` (M165): the plan a ranked query ran, as `pstore-query` recorded it.

use pstore_query::explain::{Because, Explain};
use serde_json::{Map, Value, json};

/// Runs `fut`, collecting its plan when `on`; otherwise runs it as it always ran.
pub(crate) async fn collect<T, E>(
    on: bool,
    fut: impl std::future::Future<Output = Result<T, E>>,
) -> Result<(T, Option<Value>), E> {
    if !on {
        return Ok((fut.await?, None));
    }
    let (out, plan) = pstore_query::explain::collect(fut).await;
    Ok((out?, Some(render(&plan))))
}

/// The plan as a response carries it: counts and the request's own field names, nothing a
/// key, a segment or a peer would name.
fn render(plan: &Explain) -> Value {
    let legs: Vec<Value> = plan
        .legs
        .iter()
        .map(|l| {
            let mode = match (l.exhaustive, l.widened) {
                (Some(_), _) => "exhaustive",
                (None, true) => "widened",
                (None, false) => "limited",
            };
            let mut leg = Map::new();
            leg.insert("kind".into(), json!(l.kind));
            leg.insert("field".into(), json!(l.field));
            leg.insert("limit".into(), json!(l.limit));
            leg.insert("mode".into(), json!(mode));
            leg.insert("requested".into(), json!(l.requested));
            leg.insert("candidates".into(), json!(l.candidates));
            leg.insert("kept".into(), json!(l.kept));
            if let Some(because) = l.exhaustive {
                let why = match because {
                    Because::Filter => "filter",
                    Because::Sum => "sum",
                };
                leg.insert("exhaustive_because".into(), json!(why));
            }
            if l.kind == "dense" {
                leg.insert("approximate_segments".into(), json!(l.approximate_segments));
                leg.insert("exact_segments".into(), json!(l.exact_segments));
                leg.insert("probes".into(), json!(l.probes));
            }
            Value::Object(leg)
        })
        .collect();
    json!({
        "segments": plan.segments,
        "shadow": plan.shadow,
        "remote_segments": plan.remote_segments,
        "filter": plan.filter,
        "fusion": plan.fusion,
        "legs": legs,
    })
}
