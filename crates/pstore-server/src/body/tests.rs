#![allow(
    clippy::unwrap_used,
    reason = "parser conformance fixtures report failure through assertions"
)]
use super::*;

#[test]
fn wide_objects_and_heading_scalar_shapes_preserve_tree_semantics() {
    let fields = (0..40)
        .map(|n| format!("\"key{n}\":{n}"))
        .collect::<Vec<_>>()
        .join(",");
    let wide = format!("{{{fields}}}");
    assert_eq!(plain(wide.as_bytes()).unwrap().keys.unwrap().len(), 40);
    for repeated in ["key0", "key31", "key39"] {
        let duplicate = format!("{{{fields},\"{repeated}\":999}}");
        assert!(plain(duplicate.as_bytes()).is_none());
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&duplicate)
                .unwrap()
                .get(repeated),
            Some(&serde_json::json!(999))
        );
    }
    for value in [
        "true",
        "false",
        "null",
        "-1",
        "1.5",
        "42",
        "\"scan\"",
        "[]",
        "{}",
        "[1,2]",
        "{\"nested\":1}",
    ] {
        let body = format!("{{\"protocol\":{value},\"phase\":{value}}}");
        let tree: serde_json::Value = serde_json::from_str(&body).unwrap();
        let checked = plain(body.as_bytes()).unwrap();
        assert_eq!(
            checked.heading.protocol,
            tree.get("protocol").and_then(serde_json::Value::as_u64)
        );
        assert_eq!(
            checked.heading.phase.as_deref(),
            tree.get("phase").and_then(serde_json::Value::as_str)
        );
    }
    for body in [
        br#"{"protocol":{"x":1,"x":2}}"#.as_slice(),
        br#"{"phase":[{"x":1,"x":2}]}"#,
        br#"{"protocol":1e999}"#,
        br#"{"phase":1e999}"#,
    ] {
        assert!(plain(body).is_none());
    }
}
