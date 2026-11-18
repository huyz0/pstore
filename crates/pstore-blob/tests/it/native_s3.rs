//! M286: pstore's own S3 client against a fake S3 over real HTTP, beside `object_store`'s.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

use bytes::Bytes;
use pstore_blob::s3::S3Backend;
use pstore_blob::{BlobError, BlobStore, CasError, Key, ObjectStoreBackend, Precondition};
use pstore_testkit::fake_s3::{BUCKET, FakeS3, Fault, KEY_ID, REGION, SECRET};
use std::sync::Arc;

fn native(fake: &FakeS3) -> S3Backend {
    S3Backend::new(fake.config(), S3Backend::unprobed("fake-s3")).unwrap()
}

/// `object_store`'s S3 client on the same fake, with its retries off so a fault is seen
/// once, as the native client sees it.
fn object_store(fake: &FakeS3) -> ObjectStoreBackend {
    let s3 = object_store::aws::AmazonS3Builder::new()
        .with_endpoint(fake.endpoint())
        .with_bucket_name(BUCKET)
        .with_region(REGION)
        .with_access_key_id(KEY_ID)
        .with_secret_access_key(SECRET)
        .with_allow_http(true)
        .with_retry(object_store::RetryConfig {
            max_retries: 0,
            ..Default::default()
        })
        .build()
        .unwrap();
    ObjectStoreBackend::new(Arc::new(s3), ObjectStoreBackend::unprobed("fake-s3"))
}

fn puts(fake: &FakeS3) -> usize {
    fake.requests()
        .iter()
        .filter(|(m, _)| m == hyper::Method::PUT)
        .count()
}

#[tokio::test]
async fn native_s3_conformance() {
    let fake = FakeS3::start().await.unwrap();
    let ours = pstore_testkit::conformance::run(&native(&fake), 1).await;
    let theirs = pstore_testkit::conformance::run(&object_store(&fake), 2).await;
    assert_eq!(ours.probes, theirs.probes);
    assert_eq!(ours.observed, theirs.observed);
    // The one divergence is the fake's, and S3's: content-derived ETags (MD5), so ABA is
    // live. Everything else conforms, through both clients.
    let names: Vec<&str> = ours.divergences().iter().map(|p| p.name).collect();
    assert_eq!(names, ["aba_resistance"]);
    // ⚠️ The fake verifies every signature with pstore's signer, so `object_store`'s
    // requests passing is a second implementation agreeing with it.
    assert_eq!(fake.bad_signatures(), 0);
    assert!(fake.requests().len() > 20);
}

#[tokio::test]
async fn a_bad_secret_is_refused_by_the_fake() {
    // The control for the line above: a fake that accepted anything would pass it too.
    let fake = FakeS3::start().await.unwrap();
    let mut cfg = fake.config();
    cfg.credentials.secret = "wrong".to_owned();
    let s = S3Backend::new(cfg, S3Backend::unprobed("x")).unwrap();
    assert!(
        matches!(s.get(&Key::new("k")).await, Err(BlobError::Other(m)) if m.contains("SignatureDoesNotMatch"))
    );
    assert_eq!(fake.bad_signatures(), 1);
}

#[tokio::test]
async fn a_503_is_slowdown_after_one_request() {
    let fake = FakeS3::start().await.unwrap();
    let s = native(&fake);
    for status in [503, 429] {
        fake.fail_next(Fault {
            status,
            code: "SlowDown",
            apply: false,
        });
        assert_eq!(s.get(&Key::new("k")).await, Err(BlobError::SlowDown));
    }
    assert_eq!(
        fake.requests().len(),
        2,
        "the client retried: {:?}",
        fake.requests()
    );
}

#[tokio::test]
async fn a_409_is_contended() {
    let fake = FakeS3::start().await.unwrap();
    let s = native(&fake);
    fake.fail_next(Fault {
        status: 409,
        code: "ConditionalRequestConflict",
        apply: false,
    });
    let r = s
        .put_conditional(
            &Key::new("k"),
            Bytes::from_static(b"x"),
            Precondition::NotExists,
        )
        .await;
    assert_eq!(r, Err(CasError::Contended));
    assert_eq!(fake.requests().len(), 1);
}

#[tokio::test]
async fn a_conditional_write_that_lands_and_then_gets_a_503_is_not_repeated() {
    // M282's case: repeating it would fail its own precondition and answer `Lost`.
    let fake = FakeS3::start().await.unwrap();
    let s = native(&fake);
    fake.fail_next(Fault {
        status: 503,
        code: "SlowDown",
        apply: true,
    });
    let k = Key::new("bundle");
    let r = s
        .put_conditional(&k, Bytes::from_static(b"x"), Precondition::NotExists)
        .await;
    assert!(matches!(r, Err(CasError::Io(_))), "{r:?}");
    assert_eq!(puts(&fake), 1);
    assert_eq!(&s.get(&k).await.unwrap()[..], b"x");
}

#[tokio::test]
async fn conditional_writes_win_and_lose_by_status() {
    let fake = FakeS3::start().await.unwrap();
    let s = native(&fake);
    let k = Key::new("head");
    let first = s
        .put_conditional(&k, Bytes::from_static(b"1"), Precondition::NotExists)
        .await
        .unwrap();
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
        (Bytes::from_static(b"2"), second.tag)
    );
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

#[tokio::test]
async fn reads_answer_exactly_the_bytes_asked_for_or_an_error() {
    let fake = FakeS3::start().await.unwrap();
    let s = native(&fake);
    let k = Key::new("dir/seg ment+1");
    let body: Bytes = (0..=255u8).collect::<Vec<_>>().into();
    s.put(&k, body.clone()).await.unwrap();
    assert_eq!(s.get(&k).await.unwrap(), body);
    assert_eq!(
        &s.get_range(&k, 10..14).await.unwrap()[..],
        &[10, 11, 12, 13]
    );
    assert!(s.get_range(&k, 7..7).await.unwrap().is_empty());
    assert_eq!(s.get_suffix(&k, 3).await.unwrap(), body.slice(253..));
    assert!(s.get_suffix(&k, 0).await.unwrap().is_empty());
    assert_eq!(s.get_suffix(&k, 999).await.unwrap(), body);
    // An overrun is a short 206: an error, not a truncation.
    assert_eq!(
        s.get_range(&k, 250..300).await,
        Err(BlobError::RangeOutOfBounds(250..300, 6))
    );
    // Wholly past the end is a 416 that names the length.
    assert_eq!(
        s.get_range(&k, 300..310).await,
        Err(BlobError::RangeOutOfBounds(300..310, 256))
    );
    assert!(matches!(
        s.get_range(&k, std::ops::Range { start: 5, end: 4 }).await,
        Err(BlobError::RangeOutOfBounds(..))
    ));
    assert_eq!(s.stat(&k).await.unwrap().0, 256);
    assert_eq!(s.head(&k).await.unwrap(), 256);

    let gone = Key::new("absent");
    assert!(matches!(s.get(&gone).await, Err(BlobError::NotFound(_))));
    assert!(matches!(
        s.get_range(&gone, 0..1).await,
        Err(BlobError::NotFound(_))
    ));
    assert!(matches!(s.head(&gone).await, Err(BlobError::NotFound(_))));
    assert_eq!(s.get_tag(&gone).await, Ok(None));
    fake.fail_next(Fault {
        status: 503,
        code: "SlowDown",
        apply: false,
    });
    assert_eq!(s.get_tag(&gone).await, Err(BlobError::SlowDown));
    fake.fail_next(Fault {
        status: 500,
        code: "InternalError",
        apply: false,
    });
    assert!(matches!(
        s.put(&k, Bytes::new()).await,
        Err(BlobError::Other(_))
    ));
    assert_eq!(fake.bad_signatures(), 0);
}

#[tokio::test]
async fn a_batch_delete_is_one_request_and_a_listing_reads_every_page() {
    let fake = FakeS3::start().await.unwrap();
    fake.set_page(2);
    let s = native(&fake);
    let keys: Vec<Key> = ["p/a", "p/b&<c>", "p/c", "p/d", "p/sub/e", "q/f"]
        .into_iter()
        .map(Key::new)
        .collect();
    for k in &keys {
        s.put(k, Bytes::from_static(b"x")).await.unwrap();
    }
    let mut listed: Vec<String> = s
        .list_unrestricted(&Key::new("p"))
        .await
        .unwrap()
        .iter()
        .map(|k| k.as_str().to_owned())
        .collect();
    listed.sort();
    assert_eq!(listed, ["p/a", "p/b&<c>", "p/c", "p/d"]);
    assert_eq!(s.list_unrestricted(&Key::new("")).await.unwrap().len(), 0);

    let before = fake.requests().len();
    s.delete_batch(&keys[..5]).await.unwrap();
    assert_eq!(fake.requests().len(), before + 1);
    assert!(matches!(s.get(&keys[1]).await, Err(BlobError::NotFound(_))));
    assert!(s.get(&keys[5]).await.is_ok());

    s.delete_batch(&[]).await.unwrap();
    let over: Vec<Key> = (0..1001).map(|i| Key::new(format!("k{i}"))).collect();
    assert!(s.delete_batch(&over).await.is_err());
    assert_eq!(
        fake.requests().len(),
        before + 3,
        "an empty or over-cap batch sent a request"
    );
    fake.fail_next(Fault {
        status: 503,
        code: "SlowDown",
        apply: false,
    });
    assert_eq!(s.delete_batch(&keys[5..]).await, Err(BlobError::SlowDown));
    fake.fail_next(Fault {
        status: 503,
        code: "SlowDown",
        apply: false,
    });
    assert_eq!(
        s.list_unrestricted(&Key::new("p")).await,
        Err(BlobError::SlowDown)
    );
}

#[tokio::test]
async fn an_unreachable_endpoint_is_an_error_not_a_hang() {
    let fake = FakeS3::start().await.unwrap();
    let cfg = fake.config();
    drop(fake);
    let s = S3Backend::new(cfg, S3Backend::unprobed("x")).unwrap();
    assert!(matches!(
        s.get(&Key::new("k")).await,
        Err(BlobError::Other(_))
    ));
}
