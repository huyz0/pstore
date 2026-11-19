//! M287: `PSTORE_BLOB_CLIENT`, the server's S3 client as a setting. Both clients open the same
//! signature-checking fake S3 through `open_s3` and must answer every call alike.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

use bytes::Bytes;
use pstore_blob::{BlobStore, Key};
#[cfg(feature = "object-store-client")]
use pstore_blob::{CasError, Precondition};
use pstore_server::{AzureConfig, BlobClient, Config, ConfigError, Profile, open_azure, open_s3};
use pstore_testkit::fake_azure::FakeAzure;
use pstore_testkit::fake_s3::{BUCKET, FakeS3, KEY_ID, SECRET};

fn vars<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
    move |k| {
        pairs
            .iter()
            .find(|(n, _)| *n == k)
            .map(|(_, v)| (*v).to_owned())
    }
}

#[test]
fn blob_client_setting_is_parsed_and_refused_when_unknown() {
    let lane = ("PSTORE_LANE", "1");
    assert_eq!(
        Config::from_vars(vars(&[lane])).unwrap().client,
        BlobClient::Native,
        "the measured default"
    );
    for (v, want) in [
        ("native", BlobClient::Native),
        ("object_store", BlobClient::ObjectStore),
    ] {
        let c = Config::from_vars(vars(&[lane, ("PSTORE_BLOB_CLIENT", v)])).unwrap();
        assert_eq!(c.client, want);
    }
    // A typo is a refusal by name, never the default.
    assert_eq!(
        Config::from_vars(vars(&[lane, ("PSTORE_BLOB_CLIENT", "objectstore")])),
        Err(ConfigError::BlobClient("objectstore".to_owned()))
    );
    assert_eq!(BlobClient::default(), BlobClient::Native);
}

#[cfg(feature = "object-store-client")]
async fn open(fake: &FakeS3, client: BlobClient) -> std::sync::Arc<dyn BlobStore> {
    let keys = (KEY_ID.to_owned(), SECRET.to_owned());
    open_s3(
        client,
        &fake.endpoint(),
        BUCKET,
        Some(&keys),
        pstore_testkit::fake_s3::REGION,
        Profile::Conforming,
        |_| None,
    )
    .unwrap()
}

/// Everything the engine asks of a store, as text, so two clients' runs compare whole.
#[cfg(feature = "object-store-client")]
async fn run(s: &dyn BlobStore, prefix: &str) -> Vec<String> {
    let k = |n: &str| Key::new(format!("{prefix}/{n}"));
    let mut out = Vec::new();
    let body = Bytes::from((0..=255_u8).cycle().take(70_000).collect::<Vec<_>>());
    out.push(format!(
        "{:?}",
        s.put(&k("seg"), body.clone()).await.map(|_| ())
    ));
    out.push(format!("{:?}", s.get(&k("seg")).await.map(|b| b == body)));
    out.push(format!(
        "{:?}",
        s.get_range(&k("seg"), 10..4106).await.map(|b| b.len())
    ));
    out.push(format!(
        "{:?}",
        s.get_suffix(&k("seg"), 4096)
            .await
            .map(|b| b[..] == body[body.len() - 4096..])
    ));
    out.push(format!(
        "{:?}",
        s.get_range(&k("seg"), 69_990..70_010)
            .await
            .map(|b| b.len())
    ));
    out.push(format!("{:?}", s.head(&k("seg")).await));
    // The variant, not the message: the two clients word a 404 differently.
    let absent = |r: Result<Bytes, pstore_blob::BlobError>| {
        format!(
            "{:?}",
            r.map(|_| ())
                .map_err(|e| matches!(e, pstore_blob::BlobError::NotFound(_)))
        )
    };
    out.push(absent(s.get(&k("absent")).await));
    let created = s
        .put_conditional(
            &k("head"),
            Bytes::from_static(b"1"),
            Precondition::NotExists,
        )
        .await;
    out.push(format!("{:?}", created.as_ref().map(|_| ())));
    out.push(format!(
        "{:?}",
        s.put_conditional(
            &k("head"),
            Bytes::from_static(b"x"),
            Precondition::NotExists
        )
        .await
        .map(|_| ())
        .map_err(|e| matches!(e, CasError::Lost))
    ));
    let (_, tag) = s.get_with_tag(&k("head")).await.unwrap();
    out.push(format!(
        "{:?}",
        s.put_conditional(
            &k("head"),
            Bytes::from_static(b"2"),
            Precondition::Match(tag.clone())
        )
        .await
        .map(|_| ())
    ));
    out.push(format!(
        "{:?}",
        s.put_conditional(
            &k("head"),
            Bytes::from_static(b"3"),
            Precondition::Match(tag)
        )
        .await
        .map(|_| ())
        .map_err(|e| matches!(e, CasError::Lost))
    ));
    out.push(format!("{:?}", s.get(&k("head")).await));
    let mut listed = s.list_unrestricted(&Key::new(prefix)).await.unwrap();
    listed.sort();
    out.push(format!(
        "{:?}",
        listed
            .iter()
            .map(|k| k.as_str().trim_start_matches(prefix).to_owned())
            .collect::<Vec<_>>()
    ));
    out.push(format!(
        "{:?}",
        s.delete_batch(&[k("seg"), k("head")]).await
    ));
    out.push(absent(s.get(&k("seg")).await));
    out
}

#[cfg(feature = "object-store-client")]
#[tokio::test]
async fn both_s3_clients_serve_the_same_answers() {
    let fake = FakeS3::start().await.unwrap();
    let native = run(&*open(&fake, BlobClient::Native).await, "n").await;
    let object_store = run(&*open(&fake, BlobClient::ObjectStore).await, "o").await;
    assert_eq!(native, object_store);
    // And the run says something: the conditional writes won and lost as they should.
    assert!(native[7].starts_with("Ok"), "{native:?}");
    assert_eq!(native[8], "Err(true)");
    assert_eq!(native[10], "Err(true)");
    assert_eq!(fake.bad_signatures(), 0);
}

#[tokio::test]
async fn the_native_client_finds_keys_through_the_chain_without_static_ones() {
    let fake = FakeS3::start().await.unwrap();
    let s = open_s3(
        BlobClient::Native,
        &fake.endpoint(),
        BUCKET,
        None,
        pstore_testkit::fake_s3::REGION,
        Profile::Unprobed,
        vars(&[
            ("AWS_ACCESS_KEY_ID", KEY_ID),
            ("AWS_SECRET_ACCESS_KEY", SECRET),
        ]),
    )
    .unwrap();
    s.put(&Key::new("k"), Bytes::from_static(b"v"))
        .await
        .unwrap();
    assert_eq!(fake.bad_signatures(), 0);
    // Unprobed is divergent, as for `object_store`.
    assert!(matches!(
        s.capabilities().cas,
        pstore_blob::Support::Divergent(_)
    ));
    // No provider at all is a refusal to open, not an unsigned client.
    assert!(
        open_s3(
            BlobClient::Native,
            &fake.endpoint(),
            BUCKET,
            None,
            "us-east-1",
            Profile::Unprobed,
            vars(&[("AWS_EC2_METADATA_DISABLED", "true")]),
        )
        .is_err()
    );
    // Nor is a malformed endpoint.
    assert!(
        open_s3(
            BlobClient::Native,
            "not a url",
            BUCKET,
            None,
            "us-east-1",
            Profile::Unprobed,
            |_| None
        )
        .is_err()
    );
}

fn azure_config(fake: &FakeAzure) -> AzureConfig {
    AzureConfig {
        account: pstore_testkit::fake_azure::ACCOUNT.to_owned(),
        key: Some(pstore_testkit::fake_azure::KEY.to_owned()),
        container: pstore_testkit::fake_azure::CONTAINER.to_owned(),
        endpoint: Some(fake.endpoint()),
    }
}

/// M288: the same run through both clients, on S3 and on Azure, each on its fake.
#[cfg(feature = "object-store-client")]
#[tokio::test]
async fn both_blob_clients_serve_the_same_answers() {
    let s3 = FakeS3::start().await.unwrap();
    let s3_native = run(&*open(&s3, BlobClient::Native).await, "n").await;
    let s3_theirs = run(&*open(&s3, BlobClient::ObjectStore).await, "o").await;
    assert_eq!(s3_native, s3_theirs);

    let az = FakeAzure::start().await.unwrap();
    let open_az = |client| open_azure(client, &azure_config(&az), Profile::Conforming, |_| None);
    let az_native = run(&*open_az(BlobClient::Native).unwrap(), "n").await;
    let az_theirs = run(&*open_az(BlobClient::ObjectStore).unwrap(), "o").await;
    assert_eq!(az_native, az_theirs);
    // The conditional writes won and lost as they should, and Azure refused the suffix
    // read (C-14) where S3 served it: the one line the two backends differ on.
    assert!(az_native[7].starts_with("Ok"), "{az_native:?}");
    assert_eq!(az_native[8], "Err(true)");
    assert_eq!(az_native[10], "Err(true)");
    let differ: Vec<usize> = (0..s3_native.len())
        .filter(|&i| s3_native[i] != az_native[i])
        .collect();
    assert_eq!(differ, [3], "{s3_native:?}\n{az_native:?}");
    assert_eq!(az.bad_signatures(), 0);
    assert_eq!(s3.bad_signatures(), 0);
}

/// M288: a default build has only the native client and refuses `object_store` by name, at
/// open, for S3 and Azure alike. With the feature, the same calls open. CI runs this test in
/// both builds (`.github/workflows/ci.yml`, the `test` job).
#[tokio::test]
async fn object_store_mode_needs_its_feature() {
    let s3 = FakeS3::start().await.unwrap();
    let az = FakeAzure::start().await.unwrap();
    let keys = (KEY_ID.to_owned(), SECRET.to_owned());
    let s3_opened = open_s3(
        BlobClient::ObjectStore,
        &s3.endpoint(),
        BUCKET,
        Some(&keys),
        pstore_testkit::fake_s3::REGION,
        Profile::Conforming,
        |_| None,
    )
    .map(|_| ());
    let az_opened = open_azure(
        BlobClient::ObjectStore,
        &azure_config(&az),
        Profile::Conforming,
        |_| None,
    )
    .map(|_| ());
    if cfg!(feature = "object-store-client") {
        assert_eq!(s3_opened, Ok(()));
        assert_eq!(az_opened, Ok(()));
    } else {
        let refused = Err(pstore_server::OBJECT_STORE_ABSENT.to_owned());
        assert_eq!(s3_opened, refused);
        assert_eq!(az_opened, refused);
        assert!(pstore_server::OBJECT_STORE_ABSENT.contains("object-store-client"));
    }
    // The native client opens in every build.
    assert!(
        open_azure(
            BlobClient::Native,
            &azure_config(&az),
            Profile::Conforming,
            |_| None
        )
        .is_ok()
    );
}

/// M288: the native Azure client with no key takes a bearer token from the chain, here IMDS's.
#[tokio::test]
async fn the_native_azure_client_takes_a_token_without_a_key() {
    let az = FakeAzure::start().await.unwrap();
    az.accept_bearer("imds-token");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let imds = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        while let Ok((mut tcp, _)) = listener.accept().await {
            let mut buf = [0_u8; 4096];
            let _ = tcp.read(&mut buf).await;
            let body = r#"{"access_token":"imds-token","expires_on":"4102444800"}"#;
            let _ = tcp
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nconnection: close\r\ncontent-length: {}\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await;
        }
    });
    let mut config = azure_config(&az);
    config.key = None;
    let s = pstore_server::native_azure_store(&config, Profile::Conforming, |k| {
        (k == "AZURE_IMDS_ENDPOINT").then(|| imds.clone())
    })
    .unwrap();
    s.put(&Key::new("k"), Bytes::from_static(b"v"))
        .await
        .unwrap();
    assert_eq!(az.bad_signatures(), 0);
    assert_eq!(s.capabilities().backend, "azure(devstoreaccount1)");
    // The public endpoint when none is given: offline, so only that it builds.
    config.endpoint = None;
    assert!(pstore_server::native_azure_store(&config, Profile::Conforming, |_| None).is_ok());
}
