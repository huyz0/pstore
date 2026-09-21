//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! M7g.1 — what a deployment must choose, and what it is told it must do itself.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use pstore_blob::{Accounted, MemoryStore, Support};
use pstore_server::{Api, Backend, Config, ConfigError, Profile, UNSCHEDULED, s3_capabilities};
use pstore_types::LaneId;
use std::collections::BTreeMap;
use std::sync::Arc;
use tower::ServiceExt;

fn vars(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
    let map: BTreeMap<String, String> = pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect();
    move |k| map.get(k).cloned()
}

/// ⚠️ **M7g.1, and the default is the refusal.** An operator who has not run the conformance
/// suite has not measured this bucket, and a profile that has not been measured must not be
/// trusted with a write the caller is told is durable. The safe default is the one that
/// refuses, so an omission cannot become a silent promise.
#[test]
fn the_default_profile_is_unprobed_and_the_default_backend_is_memory() {
    let c = Config::from_vars(vars(&[("PSTORE_LANE", "3")])).unwrap();
    assert_eq!(c.backend, Backend::Memory);
    assert_eq!(c.profile, Profile::Unprobed);
    assert_eq!(c.lane, LaneId(3));
}

#[test]
fn a_backend_or_profile_it_does_not_know_is_refused_by_name() {
    // ⚠️ Never "fall back to the default": a typo in `PSTORE_PROFILE` would then be a
    // deployment that silently refuses every write, and a typo in `PSTORE_BACKEND` a
    // deployment that silently stores nothing durably.
    let e = Config::from_vars(vars(&[("PSTORE_LANE", "1"), ("PSTORE_BACKEND", "azure")]));
    assert_eq!(e, Err(ConfigError::Backend("azure".to_owned())));
    let e = Config::from_vars(vars(&[
        ("PSTORE_LANE", "1"),
        ("PSTORE_PROFILE", "probably"),
    ]));
    assert_eq!(e, Err(ConfigError::Profile("probably".to_owned())));
}

/// ⚠️ **The one required field the first draft defaulted.** An absent endpoint under
/// `PSTORE_BACKEND=s3` built a backend named `s3()` which a `conforming` profile then declared
/// able to fence — moving a configuration error out of the constructor that validates
/// configuration and into the first request. `deploy.md` lists the variable with no default;
/// this is the code agreeing with it.
#[test]
fn s3_without_an_endpoint_is_refused_at_startup() {
    let e = Config::from_vars(vars(&[("PSTORE_LANE", "1"), ("PSTORE_BACKEND", "s3")]));
    assert_eq!(e, Err(ConfigError::Endpoint));
    // ⚠️ And only for s3: the memory backend has no endpoint and must not be made to invent
    // one, or the default configuration stops working.
    assert!(Config::from_vars(vars(&[("PSTORE_LANE", "1")])).is_ok());
}

/// ⚠️ **Half a key pair is not a credential**, and the deployed case is neither half: on EC2
/// or EKS the provider chain supplies an instance profile or a service-account role. A first
/// draft baked `pstore` / `pstore-dev-secret` in unconditionally, which makes that deployment
/// impossible and ships a dev password at a real bucket when an operator forgets one.
#[test]
fn credentials_are_used_only_when_both_halves_are_given() {
    let none = Config::from_vars(vars(&[
        ("PSTORE_LANE", "1"),
        ("PSTORE_BACKEND", "s3"),
        ("PSTORE_S3_ENDPOINT", "http://rustfs:9000"),
    ]))
    .unwrap();
    assert_eq!(none.credentials, None, "an unset key must mean the chain");

    let half = Config::from_vars(vars(&[
        ("PSTORE_LANE", "1"),
        ("PSTORE_BACKEND", "s3"),
        ("PSTORE_S3_ENDPOINT", "http://rustfs:9000"),
        ("PSTORE_ACCESS_KEY", "who"),
    ]))
    .unwrap();
    assert_eq!(half.credentials, None);

    let both = Config::from_vars(vars(&[
        ("PSTORE_LANE", "1"),
        ("PSTORE_BACKEND", "s3"),
        ("PSTORE_S3_ENDPOINT", "http://rustfs:9000"),
        ("PSTORE_ACCESS_KEY", "who"),
        ("PSTORE_SECRET_KEY", "shh"),
    ]))
    .unwrap();
    assert_eq!(both.credentials, Some(("who".to_owned(), "shh".to_owned())));
}

#[test]
fn s3_is_a_backend_and_conforming_is_a_profile() {
    let c = Config::from_vars(vars(&[
        ("PSTORE_LANE", "1"),
        ("PSTORE_BACKEND", "s3"),
        ("PSTORE_PROFILE", "conforming"),
        ("PSTORE_S3_ENDPOINT", "http://rustfs:9000"),
        ("PSTORE_BUCKET", "pstore"),
    ]))
    .unwrap();
    assert_eq!(c.backend, Backend::S3);
    assert_eq!(c.profile, Profile::Conforming);
    assert_eq!(c.endpoint, "http://rustfs:9000");
    assert_eq!(c.bucket, "pstore");
}

/// ⚠️ **The refusal criterion 3 rests on, asserted here rather than only in a container.**
/// `unprobed` marks both fencing primitives `Divergent`, `admits_durable_writes` is false, and
/// `Api::new` refuses — which is what makes the image exit non-zero instead of serving.
#[test]
fn an_unprobed_s3_profile_cannot_fence_and_a_conforming_one_can() {
    let unprobed = s3_capabilities("http://rustfs:9000", Profile::Unprobed);
    assert!(!unprobed.admits_durable_writes());
    assert!(matches!(unprobed.cas, Support::Divergent(_)));
    assert!(matches!(unprobed.create_if_absent, Support::Divergent(_)));

    let conforming = s3_capabilities("http://rustfs:9000", Profile::Conforming);
    assert!(conforming.admits_durable_writes());
    // ⚠️ The endpoint is in the name, because "which bucket did this refuse about" is the
    // first question an operator asks and `s3` alone does not answer it.
    assert!(
        conforming.backend.contains("http://rustfs:9000"),
        "got {}",
        conforming.backend
    );
}

/// ⚠️ **M7g's criterion 7 in the one place a unit test can hold it.** `byoc.sh` compares this
/// endpoint against `docs/deploy.md`; what is asserted here is that the endpoint exists, costs
/// nothing, and reports every duty the constant names — so a duty added to the constant reaches
/// the gate that compares it against the document.
#[tokio::test]
async fn the_duties_endpoint_reports_every_unscheduled_duty() {
    let api = Api::new(Accounted::new(MemoryStore::new()), LaneId(1)).unwrap();
    let req = Request::builder()
        .uri("/v1/admin/duties")
        .body(Body::empty())
        .unwrap();
    let res = Arc::clone(&api).router().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = res.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let ids: Vec<&str> = json["duties"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["id"].as_str().unwrap())
        .collect();
    let expected: Vec<&str> = UNSCHEDULED.iter().map(|d| d.id).collect();
    assert_eq!(ids, expected);
    assert!(ids.contains(&"fold"), "the one that loses data quietly");
    assert!(ids.contains(&"reap"));
    // Every duty says what to do instead, or the document it feeds is a list of complaints.
    for d in json["duties"].as_array().unwrap() {
        assert!(!d["instead"].as_str().unwrap().is_empty());
    }
    // ⚠️ Zero store requests: this is a constant, and an admin endpoint that costs a GET is
    // an admin endpoint a scraper turns into a bill. Read back through `/metrics`, which is
    // the only public window onto the accounting and does not itself touch the store.
    let req = Request::builder()
        .uri("/metrics")
        .body(Body::empty())
        .unwrap();
    let res = Arc::clone(&api).router().oneshot(req).await.unwrap();
    let text =
        String::from_utf8(res.into_body().collect().await.unwrap().to_bytes().to_vec()).unwrap();
    for class in ["read", "write", "list", "delete"] {
        assert!(
            text.contains(&format!(
                "pstore_blob_requests_total{{class=\"{class}\"}} 0"
            )),
            "the duties endpoint spent a {class} request:\n{text}"
        );
    }
}

/// ⚠️ Ids must be usable as markdown heading tokens, because `byoc.sh` extracts them with
/// `grep -o '^### `[a-z-]\+`'`. An id with a space or a capital would make the gate match
/// nothing and pass vacuously — which is the failure mode of every grep-shaped check.
#[test]
fn every_duty_id_is_a_lower_case_token() {
    for d in UNSCHEDULED {
        assert!(
            !d.id.is_empty() && d.id.chars().all(|c| c.is_ascii_lowercase() || c == '-'),
            "{} is not extractable by the deploy.md gate",
            d.id
        );
    }
}
