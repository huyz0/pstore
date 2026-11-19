//! M288: pstore's own Azure client against a fake Blob service over real HTTP, beside
//! `object_store`'s, and its token providers against fakes of their endpoints.
#![cfg(feature = "azure-native")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

use super::native_s3_creds::{Endpoint, Seen, unix_now};
use bytes::Bytes;
use pstore_blob::azure::sign;
use pstore_blob::azure::token::Provider;
use pstore_blob::azure::{AzureAuth, AzureBackend};
use pstore_blob::{BlobError, BlobStore, CasError, Key, Precondition};
use pstore_testkit::fake_azure::{ACCOUNT, CONTAINER, FakeAzure, Fault, KEY};
use std::sync::Arc;

fn caps() -> pstore_blob::Capabilities {
    pstore_blob::s3::S3Backend::unprobed("fake-azure")
}

fn native(fake: &FakeAzure) -> AzureBackend {
    AzureBackend::new(fake.config(), AzureAuth::Key(KEY.to_owned()), caps()).unwrap()
}

/// `object_store`'s Azure client on the same fake, its retries off, as the server's
/// conditional client is.
#[cfg(feature = "compat")]
fn object_store(fake: &FakeAzure) -> pstore_blob::ObjectStoreBackend {
    let az = object_store::azure::MicrosoftAzureBuilder::new()
        .with_account(pstore_testkit::fake_azure::ACCOUNT)
        .with_container_name(pstore_testkit::fake_azure::CONTAINER)
        .with_access_key(KEY)
        .with_endpoint(fake.endpoint())
        .with_allow_http(true)
        .with_retry(object_store::RetryConfig {
            max_retries: 0,
            ..Default::default()
        })
        .build()
        .unwrap();
    pstore_blob::ObjectStoreBackend::azure(Arc::new(az), caps())
}

fn count(fake: &FakeAzure, method: &str) -> usize {
    fake.requests()
        .iter()
        .filter(|(m, _)| m.as_str() == method)
        .count()
}

/// Criteria 1 and 2: the same probes and the same observed profile through both clients,
/// and every request of both, batch sub-requests included, verified by pstore's signer.
#[cfg(feature = "compat")]
#[tokio::test]
async fn native_azure_conformance() {
    let fake = FakeAzure::start().await.unwrap();
    let ours = pstore_testkit::conformance::run(&native(&fake), 1).await;
    let theirs = pstore_testkit::conformance::run(&object_store(&fake), 2).await;
    assert_eq!(ours.probes, theirs.probes);
    assert_eq!(ours.observed, theirs.observed);
    // Azure's one divergence, C-14's: no suffix reads. ETags are not content-derived, so
    // ABA is not live, as on Azurite.
    let names: Vec<&str> = ours.divergences().iter().map(|p| p.name).collect();
    assert_eq!(names, ["suffix_read"]);
    assert!(ours.observed.admits_durable_writes());
    assert_eq!(fake.bad_signatures(), 0);
    assert!(fake.requests().len() > 20);
}

/// Criterion 2: `object_store`'s SharedKey requests, every verb and a batch, pass the fake's
/// check with pstore's signer, and a wrong key fails it.
#[cfg(feature = "compat")]
#[tokio::test]
async fn shared_key_matches_object_store() {
    let fake = FakeAzure::start().await.unwrap();
    let theirs = object_store(&fake);
    let k = Key::new("dir/a b+c");
    theirs.put(&k, Bytes::from_static(b"0123")).await.unwrap();
    theirs
        .put_conditional(
            &Key::new("dir/new"),
            Bytes::from_static(b"x"),
            Precondition::NotExists,
        )
        .await
        .unwrap();
    assert_eq!(&theirs.get_range(&k, 1..3).await.unwrap()[..], b"12");
    assert_eq!(theirs.head(&k).await.unwrap(), 4);
    assert_eq!(
        theirs
            .list_unrestricted(&Key::new("dir"))
            .await
            .unwrap()
            .len(),
        2
    );
    theirs
        .delete_batch(&[k.clone(), Key::new("dir/new")])
        .await
        .unwrap();
    assert!(matches!(
        native(&fake).get(&k).await,
        Err(BlobError::NotFound(_))
    ));
    assert_eq!(fake.bad_signatures(), 0);
    assert_eq!(count(&fake, "POST"), 1);

    let wrong = AzureBackend::new(
        fake.config(),
        AzureAuth::Key("d3Jvbmcta2V5".to_owned()),
        caps(),
    )
    .unwrap();
    assert!(wrong.get(&k).await.is_err());
    assert_eq!(fake.bad_signatures(), 1);
}

/// Criterion 3: a batch delete is one request of at most 256 deletes; missing keys are not
/// failures; 257 are refused before any request.
#[tokio::test]
async fn a_batch_delete_is_one_request() {
    let fake = FakeAzure::start().await.unwrap();
    let s = native(&fake);
    let keys: Vec<Key> = (0..256).map(|i| Key::new(format!("t/k{i:03}"))).collect();
    for k in &keys[..200] {
        s.put(k, Bytes::from_static(b"v")).await.unwrap();
    }
    let before = fake.requests().len();
    s.delete_batch(&keys).await.unwrap();
    assert_eq!(fake.requests().len(), before + 1);
    assert_eq!(count(&fake, "POST"), 1);
    assert!(
        s.list_unrestricted(&Key::new("t"))
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(fake.bad_signatures(), 0);

    let too_many: Vec<Key> = (0..257).map(|i| Key::new(format!("t/k{i}"))).collect();
    let before = fake.requests().len();
    assert!(s.delete_batch(&too_many).await.is_err());
    assert_eq!(fake.requests().len(), before);

    // One key is a plain DELETE, still one request; none is no request.
    s.put(&keys[0], Bytes::from_static(b"v")).await.unwrap();
    let before = fake.requests().len();
    s.delete_batch(&keys[..1]).await.unwrap();
    s.delete_batch(&keys[..1]).await.unwrap();
    s.delete_batch(&[]).await.unwrap();
    assert_eq!(fake.requests().len(), before + 2);
    assert_eq!(count(&fake, "DELETE"), 2);
}

/// A batch the service refuses is an error naming it, never a silent success.
#[tokio::test]
async fn a_refused_batch_is_an_error() {
    let fake = FakeAzure::start().await.unwrap();
    let s = native(&fake);
    fake.fail_next(Fault {
        status: 403,
        code: "AuthorizationFailure",
        apply: false,
    });
    let err = s
        .delete_batch(&[Key::new("a"), Key::new("b")])
        .await
        .unwrap_err();
    assert!(err.to_string().contains("AuthorizationFailure"), "{err}");
}

#[tokio::test]
async fn conditional_writes_win_and_lose_by_status() {
    let fake = FakeAzure::start().await.unwrap();
    let s = native(&fake);
    let k = Key::new("head");
    let first = s
        .put_conditional(&k, Bytes::from_static(b"1"), Precondition::NotExists)
        .await
        .unwrap();
    // `If-None-Match: *` on a blob that exists: 409 `BlobAlreadyExists`, which is `Lost`.
    assert_eq!(
        s.put_conditional(&k, Bytes::from_static(b"2"), Precondition::NotExists)
            .await,
        Err(CasError::Lost)
    );
    let second = s
        .put_conditional(
            &k,
            Bytes::from_static(b"2"),
            Precondition::Match(first.tag.clone()),
        )
        .await
        .unwrap();
    assert_ne!(first.tag, second.tag);
    assert_eq!(
        s.put_conditional(&k, Bytes::from_static(b"3"), Precondition::Match(first.tag))
            .await,
        Err(CasError::Lost)
    );
    assert_eq!(
        s.get_with_tag(&k).await.unwrap(),
        (Bytes::from_static(b"2"), second.tag.clone())
    );
    assert_eq!(s.get_tag(&k).await.unwrap(), Some(second.tag.clone()));
    assert_eq!(s.get_tag(&Key::new("absent")).await.unwrap(), None);
    // Deleted since: Azurite answers 412, and a 404 (S3's answer, M287) is `Lost` too.
    s.delete_batch(std::slice::from_ref(&k)).await.unwrap();
    assert_eq!(
        s.put_conditional(&k, Bytes::new(), Precondition::Match(second.tag.clone()))
            .await,
        Err(CasError::Lost)
    );
    fake.fail_next(Fault {
        status: 404,
        code: "BlobNotFound",
        apply: false,
    });
    assert_eq!(
        s.put_conditional(&k, Bytes::new(), Precondition::Match(second.tag))
            .await,
        Err(CasError::Lost)
    );
    // Any other 409 is a conflict to retry, not a loss.
    fake.fail_next(Fault {
        status: 409,
        code: "LeaseIdMissing",
        apply: false,
    });
    assert_eq!(
        s.put_conditional(&k, Bytes::new(), Precondition::NotExists)
            .await,
        Err(CasError::Contended)
    );
    // And a 404 to a create is no answer about the blob.
    fake.fail_next(Fault {
        status: 404,
        code: "ContainerNotFound",
        apply: false,
    });
    let r = s
        .put_conditional(&k, Bytes::new(), Precondition::NotExists)
        .await;
    assert!(matches!(r, Err(CasError::Io(_))), "{r:?}");
    fake.fail_next(Fault {
        status: 500,
        code: "InternalError",
        apply: false,
    });
    assert!(matches!(
        s.put_conditional(&k, Bytes::new(), Precondition::NotExists).await,
        Err(CasError::Io(m)) if m.contains("InternalError")
    ));
}

/// M282's case on Azure: a conditional write that landed and then got a 503 is not repeated.
#[tokio::test]
async fn a_conditional_write_that_lands_and_then_gets_a_503_is_not_repeated() {
    let fake = FakeAzure::start().await.unwrap();
    let s = native(&fake);
    fake.fail_next(Fault {
        status: 503,
        code: "ServerBusy",
        apply: true,
    });
    let k = Key::new("bundle");
    let r = s
        .put_conditional(&k, Bytes::from_static(b"x"), Precondition::NotExists)
        .await;
    assert!(matches!(r, Err(CasError::Io(_))), "{r:?}");
    assert_eq!(count(&fake, "PUT"), 1);
    assert_eq!(&s.get(&k).await.unwrap()[..], b"x");
    // And one that did not land is not there: the fake applies only what it is told to.
    fake.fail_next(Fault {
        status: 503,
        code: "ServerBusy",
        apply: false,
    });
    let other = Key::new("other");
    let r = s.put(&other, Bytes::from_static(b"y")).await;
    assert!(r.is_err(), "{r:?}");
    assert!(matches!(s.get(&other).await, Err(BlobError::NotFound(_))));
}

#[tokio::test]
async fn throttling_is_slowdown_after_one_request() {
    let fake = FakeAzure::start().await.unwrap();
    let s = native(&fake);
    for (status, code) in [(503, "ServerBusy"), (500, "ServerBusy"), (429, "")] {
        fake.fail_next(Fault {
            status,
            code,
            apply: false,
        });
        assert_eq!(s.get(&Key::new("k")).await, Err(BlobError::SlowDown));
    }
    assert_eq!(count(&fake, "GET"), 3);
    fake.fail_next(Fault {
        status: 503,
        code: "ServerBusy",
        apply: false,
    });
    assert_eq!(
        s.put(&Key::new("k"), Bytes::new()).await.map(|_| ()),
        Err(BlobError::SlowDown)
    );
    assert_eq!(count(&fake, "PUT"), 1);
}

#[tokio::test]
async fn reads_answer_exactly_the_bytes_asked_for_or_an_error() {
    let fake = FakeAzure::start().await.unwrap();
    let s = native(&fake);
    let k = Key::new("seg");
    s.put(&k, Bytes::from_static(b"0123456789")).await.unwrap();
    assert_eq!(&s.get_range(&k, 2..5).await.unwrap()[..], b"234");
    assert_eq!(&s.get_range(&k, 9..10).await.unwrap()[..], b"9");
    assert!(s.get_range(&k, 4..4).await.unwrap().is_empty());
    assert_eq!(
        s.get_range(&k, 8..12).await,
        Err(BlobError::RangeOutOfBounds(8..12, 2))
    );
    assert_eq!(
        s.get_range(&k, 10..12).await,
        Err(BlobError::RangeOutOfBounds(10..12, 10))
    );
    #[allow(clippy::reversed_empty_ranges, reason = "the case under test")]
    let backwards = s.get_range(&k, 5..3).await;
    assert!(matches!(backwards, Err(BlobError::RangeOutOfBounds(..))));
    assert_eq!(s.head(&k).await.unwrap(), 10);
    assert_eq!(s.stat(&k).await.unwrap().0, 10);
    assert!(matches!(
        s.get(&Key::new("absent")).await,
        Err(BlobError::NotFound(_))
    ));
    assert!(matches!(
        s.head(&Key::new("absent")).await,
        Err(BlobError::NotFound(_))
    ));
    // C-14: refused before any request.
    let before = fake.requests().len();
    assert!(s.get_suffix(&k, 4).await.is_err());
    assert_eq!(fake.requests().len(), before);
    assert!(!s.capabilities().suffix_read);
    assert_eq!(s.capabilities().max_batch_delete, 256);
    assert!(!s.capabilities().delete_is_free);
}

#[tokio::test]
async fn a_listing_reads_every_page_and_only_its_level() {
    let fake = FakeAzure::start().await.unwrap();
    let s = native(&fake);
    for k in ["t/a", "t/b", "t/c", "t/sub/d", "u/e"] {
        s.put(&Key::new(k), Bytes::from_static(b"v")).await.unwrap();
    }
    fake.set_page(1);
    let before = fake.requests().len();
    let got = s.list_unrestricted(&Key::new("t")).await.unwrap();
    assert_eq!(got, [Key::new("t/a"), Key::new("t/b"), Key::new("t/c")]);
    assert!(fake.requests().len() - before >= 4);
    assert_eq!(fake.bad_signatures(), 0);
}

#[tokio::test]
async fn a_bearer_token_signs_instead_of_the_key() {
    let fake = FakeAzure::start().await.unwrap();
    fake.accept_bearer("the-token");
    let imds = Endpoint::start(Arc::new(|r: &Seen| {
        if r.header("metadata") == Some("true") {
            (
                200,
                format!(
                    r#"{{"access_token":"the-token","expires_on":"{}"}}"#,
                    unix_now() + 3600
                ),
            )
        } else {
            (400, String::new())
        }
    }))
    .await;
    let s = AzureBackend::new(
        fake.config(),
        AzureAuth::Token(Provider::Imds {
            endpoint: imds.url.clone(),
            client_id: None,
        }),
        caps(),
    )
    .unwrap();
    s.put(&Key::new("a"), Bytes::from_static(b"v"))
        .await
        .unwrap();
    s.put(&Key::new("b"), Bytes::from_static(b"v"))
        .await
        .unwrap();
    s.delete_batch(&[Key::new("a"), Key::new("b")])
        .await
        .unwrap();
    assert_eq!(fake.bad_signatures(), 0);
    assert!(matches!(
        s.get(&Key::new("a")).await,
        Err(BlobError::NotFound(_))
    ));
    assert_eq!(imds.seen().len(), 1, "one token, cached");
}

/// Criterion 5: each provider sends the request its documentation names.
#[tokio::test]
async fn managed_identity_token_from_imds() {
    let imds = Endpoint::start(Arc::new(|_: &Seen| {
        (
            200,
            r#"{"access_token":"t1","expires_on":"1795089600"}"#.to_owned(),
        )
    }))
    .await;
    let token = fetch(Provider::Imds {
        endpoint: imds.url.clone(),
        client_id: Some("uami".to_owned()),
    })
    .await;
    assert_eq!(token, ("t1".to_owned(), Some(1_795_089_600)));
    let seen = &imds.seen()[0];
    assert_eq!(seen.method, "GET");
    assert_eq!(
        seen.target,
        "/metadata/identity/oauth2/token?api-version=2019-08-01&resource=https%3A%2F%2Fstorage.azure.com&client_id=uami"
    );
    assert_eq!(seen.header("metadata"), Some("true"));
}

#[tokio::test]
async fn managed_identity_token_from_identity_endpoint() {
    let ep = Endpoint::start(Arc::new(|r: &Seen| {
        if r.header("x-identity-header") == Some("secret-header") {
            (
                200,
                r#"{"access_token":"t2","expires_on":1795089600}"#.to_owned(),
            )
        } else {
            (401, String::new())
        }
    }))
    .await;
    let provider = Provider::from_vars(|k| match k {
        "IDENTITY_ENDPOINT" => Some(format!("{}/msi/token", ep.url)),
        "IDENTITY_HEADER" => Some("secret-header".to_owned()),
        _ => None,
    });
    assert_eq!(
        fetch(provider).await,
        ("t2".to_owned(), Some(1_795_089_600))
    );
    assert_eq!(
        ep.seen()[0].target,
        "/msi/token?api-version=2019-08-01&resource=https%3A%2F%2Fstorage.azure.com"
    );
}

#[tokio::test]
async fn workload_identity_exchanges_the_federated_token() {
    let login = Endpoint::start(Arc::new(|_: &Seen| {
        (
            200,
            r#"{"token_type":"Bearer","expires_in":3599,"access_token":"t3"}"#.to_owned(),
        )
    }))
    .await;
    let file = std::env::temp_dir().join(format!("pstore-azure-fed-{}", std::process::id()));
    std::fs::write(&file, "federated-jwt\n").unwrap();
    let provider = Provider::from_vars(|k| match k {
        "AZURE_CLIENT_ID" => Some("cid".to_owned()),
        "AZURE_TENANT_ID" => Some("tid".to_owned()),
        "AZURE_FEDERATED_TOKEN_FILE" => Some(file.display().to_string()),
        "AZURE_AUTHORITY_HOST" => Some(login.url.clone()),
        _ => None,
    });
    let before = unix_now();
    let (token, expires) = fetch(provider).await;
    assert_eq!(token, "t3");
    assert!(expires.unwrap() >= before + 3599);
    let seen = &login.seen()[0];
    assert_eq!(
        (seen.method.as_str(), seen.target.as_str()),
        ("POST", "/tid/oauth2/v2.0/token")
    );
    assert_eq!(
        seen.header("content-type"),
        Some("application/x-www-form-urlencoded")
    );
    assert_eq!(
        seen.body,
        "client_id=cid&client_assertion_type=urn%3Aietf%3Aparams%3Aoauth%3Aclient-assertion-type%3Ajwt-bearer\
         &client_assertion=federated-jwt&scope=https%3A%2F%2Fstorage.azure.com%2F.default\
         &grant_type=client_credentials"
    );
    std::fs::remove_file(&file).unwrap();
}

#[tokio::test]
async fn client_secret_token() {
    let login = Endpoint::start(Arc::new(|r: &Seen| {
        if r.body.contains("client_secret=s%2F1") {
            (
                200,
                r#"{"expires_in":"3599","access_token":"t4"}"#.to_owned(),
            )
        } else {
            (401, r#"{"error":"invalid_client"}"#.to_owned())
        }
    }))
    .await;
    let provider = Provider::ClientSecret {
        token_url: format!("{}/tid/oauth2/v2.0/token", login.url),
        client_id: "cid".to_owned(),
        secret: "s/1".to_owned(),
    };
    assert_eq!(fetch(provider).await.0, "t4");
    assert_eq!(
        login.seen()[0].body,
        "client_id=cid&client_secret=s%2F1&scope=https%3A%2F%2Fstorage.azure.com%2F.default\
         &grant_type=client_credentials"
    );
    // A refusal is an error, never an anonymous request.
    let refused = Provider::ClientSecret {
        token_url: format!("{}/tid/oauth2/v2.0/token", login.url),
        client_id: "cid".to_owned(),
        secret: "wrong".to_owned(),
    };
    let fake = FakeAzure::start().await.unwrap();
    let s = AzureBackend::new(fake.config(), AzureAuth::Token(refused), caps()).unwrap();
    let err = s.get(&Key::new("k")).await.unwrap_err();
    assert!(err.to_string().contains("invalid_client"), "{err}");
    assert!(fake.requests().is_empty());
}

/// One fetch through the cache: the token and its expiry.
async fn fetch(provider: Provider) -> (String, Option<u64>) {
    use pstore_blob::s3::creds::Source as _;
    let http = pstore_blob::s3::http_client().unwrap();
    let e = provider.fetch(&http).await.unwrap();
    ((*e.credentials).clone(), e.expires)
}

/// A server that ignores `x-ms-range` answers the whole blob: the client cuts the range
/// out of it, or refuses one past its end.
#[tokio::test]
async fn a_server_that_ignores_ranges_still_answers_the_range() {
    let fake = FakeAzure::start().await.unwrap();
    let s = native(&fake);
    let k = Key::new("seg");
    s.put(&k, Bytes::from_static(b"0123456789")).await.unwrap();
    fake.ignore_ranges();
    assert_eq!(&s.get_range(&k, 2..5).await.unwrap()[..], b"234");
    assert_eq!(&s.get_range(&k, 0..10).await.unwrap()[..], b"0123456789");
    assert_eq!(
        s.get_range(&k, 8..11).await,
        Err(BlobError::RangeOutOfBounds(8..11, 10))
    );
}

/// One request to the fake, signed with its key: what pstore's client never sends.
async fn raw(
    fake: &FakeAzure,
    method: &str,
    query: &[(&str, &str)],
    extra: &[(&str, &str)],
    body: String,
) -> (u16, String) {
    use http_body_util::BodyExt;
    let path = format!("/{ACCOUNT}/{CONTAINER}");
    let date = pstore_blob::azure::sign::http_date(unix_now());
    let len = body.len().to_string();
    let mut headers = vec![
        ("x-ms-date", date.as_str()),
        ("x-ms-version", pstore_blob::azure::VERSION),
        ("content-length", len.as_str()),
    ];
    headers.extend_from_slice(extra);
    let key = sign::decode_key(KEY).unwrap();
    let auth = sign::authorization(
        ACCOUNT,
        &key,
        &sign::Request {
            method,
            path: &path,
            query,
            headers: &headers,
        },
    );
    let q = pstore_blob::s3::sign::canonical_query(query);
    let base = fake.endpoint();
    let mut req = http::Request::builder()
        .method(method)
        .uri(format!("{base}/{CONTAINER}?{q}"));
    for (k, v) in &headers {
        req = req.header(*k, *v);
    }
    let req = req
        .header("authorization", auth)
        .body(http_body_util::Full::new(Bytes::from(body)))
        .unwrap();
    let resp = pstore_blob::s3::http_client()
        .unwrap()
        .request(req)
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&body).into_owned())
}

fn batch_part(i: usize, path: &str, auth: &str) -> String {
    format!(
        "--b\r\nContent-Type: application/http\r\nContent-ID: {i}\r\n\r\n\
         DELETE {path} HTTP/1.1\r\nx-ms-date: Thu, 19 Nov 2026 00:00:00 GMT\r\n\
         x-ms-version: 2023-11-03\r\nauthorization: {auth}\r\ncontent-length: 0\r\n\r\n"
    )
}

/// The fake holds the service's batch bounds, and counts a sub-request it cannot verify.
#[tokio::test]
async fn the_fake_refuses_batches_the_service_refuses() {
    let fake = FakeAzure::start().await.unwrap();
    let ct = [("content-type", "multipart/mixed; boundary=b")];
    let q = [("comp", "batch"), ("restype", "container")];
    let (status, _) = raw(&fake, "POST", &q, &ct, "--b--\r\n".to_owned()).await;
    assert_eq!(status, 400);
    let path = format!("/{ACCOUNT}/{CONTAINER}/k");
    let mut many: String = (0..257)
        .map(|i| batch_part(i, &path, "SharedKey x:y"))
        .collect();
    many.push_str("--b--\r\n");
    let (status, _) = raw(&fake, "POST", &q, &ct, many).await;
    assert_eq!(status, 400);
    assert_eq!(fake.bad_signatures(), 0);
    let mut one = batch_part(0, &path, "SharedKey x:y");
    one.push_str("--b--\r\n");
    let (status, body) = raw(&fake, "POST", &q, &ct, one).await;
    assert_eq!(status, 202, "{body}");
    assert!(body.contains("403"), "{body}");
    assert_eq!(fake.bad_signatures(), 1);
}

/// The fake's listing: one level, its sub-levels as one `BlobPrefix` each, and a page
/// counting both, with the next page starting at the marker.
#[tokio::test]
async fn the_fake_lists_one_level_a_page_at_a_time() {
    let fake = FakeAzure::start().await.unwrap();
    let s = native(&fake);
    for k in ["t/a", "t/sub/d", "t/sub/e", "t/x", "t/z"] {
        s.put(&Key::new(k), Bytes::from_static(b"v")).await.unwrap();
    }
    fake.set_page(3);
    let q = [("comp", "list"), ("prefix", "t/"), ("restype", "container")];
    let (status, body) = raw(&fake, "GET", &q, &[], String::new()).await;
    assert_eq!(status, 200);
    assert_eq!(
        body.matches("<BlobPrefix><Name>t/sub/</Name></BlobPrefix>")
            .count(),
        1,
        "{body}"
    );
    assert!(body.contains("<Name>t/a</Name>"), "{body}");
    assert!(body.contains("<Name>t/x</Name>"), "{body}");
    assert!(body.contains("<NextMarker>t/z</NextMarker>"), "{body}");
    let q = [
        ("comp", "list"),
        ("marker", "t/z"),
        ("prefix", "t/"),
        ("restype", "container"),
    ];
    let (_, body) = raw(&fake, "GET", &q, &[], String::new()).await;
    assert_eq!(body.matches("<Blob>").count(), 1, "{body}");
    assert!(body.contains("<Name>t/z</Name>"), "{body}");
    assert!(!body.contains("BlobPrefix"), "{body}");
    assert!(body.contains("<NextMarker />"), "{body}");
}

/// Dropping the fake stops its listener.
#[tokio::test]
async fn a_dropped_fake_stops_answering() {
    let fake = FakeAzure::start().await.unwrap();
    let s = native(&fake);
    assert!(matches!(
        s.get(&Key::new("k")).await,
        Err(BlobError::NotFound(_))
    ));
    let config = fake.config();
    drop(fake);
    // A new client, so no pooled connection outlives the listener; the abort lands when
    // the runtime next runs the listener's task.
    let mut last = Ok(Bytes::new());
    for _ in 0..100 {
        let s = AzureBackend::new(config.clone(), AzureAuth::Key(KEY.to_owned()), caps()).unwrap();
        last = s.get(&Key::new("k")).await;
        if !matches!(last, Err(BlobError::NotFound(_))) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(
        matches!(last, Err(BlobError::Other(_))),
        "the dropped fake still answers: {last:?}"
    );
}
