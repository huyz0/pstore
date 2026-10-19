//! A GCS batch delete is one request (M107), against fake-gcs-server.
//!
//! ⚠️ **Ignored**: it needs the emulator. `scripts/fake-gcs.sh` starts one and runs this.
//! Emulator evidence (D-99): it says the request is shaped right and answered part by part,
//! and nothing about real GCS's throttling of a batch (M0b).

#![cfg(feature = "gcs")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

use bytes::Bytes;
use object_store::ClientOptions;
use object_store::client::{
    HttpClient, HttpConnector, HttpError, HttpRequest, HttpResponse, HttpService, ReqwestConnector,
};
use pstore_blob::{Accounted, BlobError, BlobStore, Key, ObjectStoreBackend, OpClass, TenantView};
use pstore_types::TenantId;
use std::sync::{Arc, Mutex};

type Log = Arc<Mutex<Vec<String>>>;

/// Every request either client sends, as `METHOD path`.
#[derive(Debug)]
struct Logged {
    inner: HttpClient,
    log: Log,
}

#[async_trait::async_trait]
impl HttpService for Logged {
    async fn call(&self, req: HttpRequest) -> Result<HttpResponse, HttpError> {
        self.log
            .lock()
            .unwrap()
            .push(format!("{} {}", req.method(), req.uri().path()));
        self.inner.execute(req).await
    }
}

#[derive(Debug)]
struct Connector(Log);

impl HttpConnector for Connector {
    fn connect(&self, options: &ClientOptions) -> object_store::Result<HttpClient> {
        Ok(HttpClient::new(Logged {
            inner: ReqwestConnector::default().connect(options)?,
            log: Arc::clone(&self.0),
        }))
    }
}

fn env(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_owned())
}

/// The emulator's store as one tenant sees it, its bill, the log both clients write to, and
/// a client for seeding.
fn emulator() -> (
    TenantView<ObjectStoreBackend>,
    Accounted<ObjectStoreBackend>,
    Log,
    HttpClient,
) {
    let endpoint = env("PSTORE_GCS_ENDPOINT", "http://localhost:4443");
    let bucket = env("PSTORE_GCS_BUCKET", "pstore");
    // fake-gcs-server checks no credential; the builder refuses to build without one.
    let key = format!(
        r#"{{"gcs_base_url": "{endpoint}", "disable_oauth": true,
            "client_email": "m107@pstore.invalid", "private_key_id": "m107", "private_key": ""}}"#
    );
    let log = Log::default();
    let options = ClientOptions::new().with_allow_http(true);
    let store = object_store::gcp::GoogleCloudStorageBuilder::new()
        .with_bucket_name(&bucket)
        .with_service_account_key(key)
        .with_client_options(options.clone())
        .with_http_connector(Connector(Arc::clone(&log)))
        .build()
        .unwrap();
    let client = Connector(Arc::clone(&log)).connect(&options).unwrap();
    let caps = ObjectStoreBackend::unprobed("gcs");
    let backend = ObjectStoreBackend::gcs(store, client.clone(), &endpoint, &bucket, caps);
    let bill = Accounted::new(backend);
    (bill.as_tenant(TenantId(7)), bill, log, client)
}

/// Writes `key` through the JSON API's media upload.
///
/// ⚠️ **Not `BlobStore::put`.** `object_store` writes GCS through the XML API, `PUT
/// /{bucket}/{name}`, and fake-gcs-server 1.52.2 and 1.55 both answer that `400 invalid
/// uploadType`, whatever the name. Reads through the XML API work.
async fn seed(client: &HttpClient, key: &Key) {
    let endpoint = env("PSTORE_GCS_ENDPOINT", "http://localhost:4443");
    let bucket = env("PSTORE_GCS_BUCKET", "pstore");
    let mut name = String::new();
    for b in key.as_str().bytes() {
        name.push_str(&format!("%{b:02X}"));
    }
    let req = http::Request::builder()
        .method(http::Method::POST)
        .uri(format!(
            "{endpoint}/upload/storage/v1/b/{bucket}/o?uploadType=media&name={name}"
        ))
        .body(Bytes::from_static(b"x").into())
        .unwrap();
    let res = client.execute(req).await.unwrap();
    assert!(
        res.status().is_success(),
        "seeding {}: {}",
        key.as_str(),
        res.status()
    );
}

#[tokio::test]
#[ignore = "needs fake-gcs-server: scripts/fake-gcs.sh"]
async fn one_request_deletes_a_hundred_objects() {
    let (store, bill, log, client) = emulator();
    assert_eq!(store.capabilities().max_batch_delete, 100);
    assert!(!store.capabilities().delete_is_free);
    let run = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    // `/`, a space and a `+` in every name: what the path encoding must carry.
    let keys: Vec<Key> = (0..100)
        .map(|i| Key::new(format!("m107/{run}/seg {i}+x")))
        .collect();
    // 97 written, 3 absent.
    for k in &keys[3..] {
        seed(&client, k).await;
    }
    // The seeding is visible to the store: what the delete must remove is there.
    assert_eq!(store.get(&keys[3]).await.unwrap(), Bytes::from_static(b"x"));
    log.lock().unwrap().clear();
    let before = bill.total(OpClass::Delete);

    store.delete_batch(&keys).await.unwrap();

    let sent = log.lock().unwrap().clone();
    assert_eq!(bill.total(OpClass::Delete) - before, 1);
    assert_eq!(
        sent.iter().filter(|r| r.starts_with("DELETE ")).count(),
        0,
        "{} DELETEs",
        sent.iter().filter(|r| r.starts_with("DELETE ")).count()
    );
    assert_eq!(sent, ["POST /batch/storage/v1"]);
    for k in &keys {
        assert!(
            matches!(store.get(k).await, Err(BlobError::NotFound(_))),
            "{} survived",
            k.as_str()
        );
    }
}

#[tokio::test]
#[ignore = "needs fake-gcs-server: scripts/fake-gcs.sh"]
async fn over_the_cap_is_refused_before_any_request() {
    let (store, _, log, _) = emulator();
    let keys: Vec<Key> = (0..101)
        .map(|i| Key::new(format!("m107/cap/{i}")))
        .collect();
    let r = store.delete_batch(&keys).await;
    assert!(
        matches!(&r, Err(BlobError::Other(m)) if m.contains("101") && m.contains("100")),
        "{r:?}"
    );
    assert_eq!(*log.lock().unwrap(), Vec::<String>::new());
}
