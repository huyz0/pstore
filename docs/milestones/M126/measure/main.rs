#![forbid(unsafe_code)]
#![allow(dead_code)]

#[path = "../../../crates/pstore-server/src/types.rs"]
mod after;
#[path = "../baseline/types.rs"]
mod before;
#[path = "../baseline/body.rs"]
mod before_body;
#[path = "../../../crates/pstore-server/src/body.rs"]
mod body;

use stats_alloc::{INSTRUMENTED_SYSTEM, Region, StatsAlloc};
use std::alloc::System;
use std::hint::black_box;
use std::time::Instant;

#[global_allocator]
static GLOBAL: &StatsAlloc<System> = &INSTRUMENTED_SYSTEM;

// Exactly the shared typed-then-tree parsing path, excluding API error formatting and
// engine/planning work. The baseline type source was copied before the code change.
fn parse_before<T: serde::de::DeserializeOwned>(raw: &[u8]) -> bool {
    if before_body::typed::<T>(raw).is_some() {
        return true;
    }
    serde_json::from_slice::<serde_json::Value>(raw)
        .ok()
        .is_some_and(|v| serde_json::from_value::<T>(v).is_ok())
}

fn parse_after<T: serde::de::DeserializeOwned>(raw: &[u8]) -> bool {
    if body::typed::<T>(raw).is_some() {
        return true;
    }
    serde_json::from_slice::<serde_json::Value>(raw)
        .ok()
        .is_some_and(|v| serde_json::from_value::<T>(v).is_ok())
}

fn measure(label: &str, raw: &[u8], n: usize, f: fn(&[u8]) -> bool, expect: bool, sample: usize) {
    for _ in 0..1000 {
        assert_eq!(f(black_box(raw)), expect);
    }
    let start = Instant::now();
    let region = Region::new(GLOBAL);
    let mut accepted = 0usize;
    for _ in 0..n {
        accepted += usize::from(black_box(f(black_box(raw))));
    }
    let stats = region.change();
    let elapsed = start.elapsed().as_nanos();
    assert_eq!(accepted, if expect { n } else { 0 });
    println!(
        "{sample},{label},{},{n},{:.2},{:.2},{:.2},{:.2},{accepted}",
        raw.len(),
        elapsed as f64 / n as f64,
        stats.allocations as f64 / n as f64,
        stats.reallocations as f64 / n as f64,
        stats.bytes_allocated as f64 / n as f64
    );
}

fn main() {
    let vector = (0..768).map(|i| i as f32 / 768.0).collect::<Vec<_>>();
    let write = serde_json::to_vec(&serde_json::json!({"documents":[{"id":"a","vector":vector,"attributes":{"n":1,"text":"hello world"}}]})).unwrap();
    let query = serde_json::to_vec(&serde_json::json!({"vector":vector,"top_k":10})).unwrap();
    let write_bad = serde_json::to_vec(
        &serde_json::json!({"documents":[{"id":"a","vector":vector}],"return_affected_ids":true}),
    )
    .unwrap();
    let doc_bad = serde_json::to_vec(
        &serde_json::json!({"documents":[{"id":"a","vector":vector,"attrbutes":{"n":1}}]}),
    )
    .unwrap();
    let query_bad =
        serde_json::to_vec(&serde_json::json!({"vector":vector,"compute_attributes":{}})).unwrap();
    let fallback = br#"{"documents":[{"id":"a","vector":[0,1],"attributes":{"n":1,"n":2}}]}"#;
    println!(
        "sample,case,bytes,iterations,ns_per_parse,allocations,reallocations,allocated_bytes,accepted"
    );
    for sample in 0..5 {
        for (name, raw, n, old, new) in [
            (
                "write",
                write.as_slice(),
                10_000,
                parse_before::<before::WriteRequest> as fn(&[u8]) -> bool,
                parse_after::<after::WriteRequest> as fn(&[u8]) -> bool,
            ),
            (
                "query",
                query.as_slice(),
                10_000,
                parse_before::<before::QueryRequest>,
                parse_after::<after::QueryRequest>,
            ),
            (
                "fallback",
                fallback.as_slice(),
                50_000,
                parse_before::<before::WriteRequest>,
                parse_after::<after::WriteRequest>,
            ),
            (
                "write_unknown",
                write_bad.as_slice(),
                10_000,
                parse_before::<before::WriteRequest>,
                parse_after::<after::WriteRequest>,
            ),
            (
                "document_unknown",
                doc_bad.as_slice(),
                10_000,
                parse_before::<before::WriteRequest>,
                parse_after::<after::WriteRequest>,
            ),
            (
                "query_unknown",
                query_bad.as_slice(),
                10_000,
                parse_before::<before::QueryRequest>,
                parse_after::<after::QueryRequest>,
            ),
        ] {
            let invalid = name.contains("unknown");
            if sample % 2 == 0 {
                measure(&format!("{name}_before"), raw, n, old, true, sample);
                measure(&format!("{name}_after"), raw, n, new, !invalid, sample);
            } else {
                measure(&format!("{name}_after"), raw, n, new, !invalid, sample);
                measure(&format!("{name}_before"), raw, n, old, true, sample);
            }
        }
    }
}
