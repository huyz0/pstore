//! M287: the native S3 client's credential chain, against fakes of IMDS, the ECS container
//! endpoint and STS, signing real requests to the fake S3.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

use bytes::Bytes;
use pstore_blob::s3::S3Backend;
use pstore_blob::s3::creds::Provider;
use pstore_blob::{BlobStore, Key};
use pstore_testkit::fake_s3::{FakeS3, KEY_ID, SECRET};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// One request a fake saw.
#[derive(Debug, Clone)]
pub(crate) struct Seen {
    pub(crate) method: String,
    pub(crate) target: String,
    pub(crate) headers: Vec<(String, String)>,
    pub(crate) body: String,
}

impl Seen {
    pub(crate) fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// A scripted HTTP endpoint: answers `route(&request)` and records every request. M288's
/// Azure token tests use it too.
pub(crate) struct Endpoint {
    pub(crate) url: String,
    seen: Arc<Mutex<Vec<Seen>>>,
}

pub(crate) type Route = dyn Fn(&Seen) -> (u16, String) + Send + Sync;

impl Endpoint {
    pub(crate) async fn start(route: Arc<Route>) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut tcp, _)) = listener.accept().await else {
                    return;
                };
                let (route, log) = (route.clone(), log.clone());
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut chunk = [0_u8; 4096];
                    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                        match tcp.read(&mut chunk).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => buf.extend_from_slice(&chunk[..n]),
                        }
                    }
                    let text = String::from_utf8_lossy(&buf).into_owned();
                    let mut lines = text.split("\r\n");
                    let mut first = lines.next().unwrap_or_default().split(' ');
                    let method = first.next().unwrap_or_default().to_owned();
                    let target = first.next().unwrap_or_default().to_owned();
                    let headers = lines
                        .take_while(|l| !l.is_empty())
                        .filter_map(|l| l.split_once(':'))
                        .map(|(k, v)| (k.trim().to_lowercase(), v.trim().to_owned()))
                        .collect();
                    let headers: Vec<(String, String)> = headers;
                    let len: usize = headers
                        .iter()
                        .find(|(k, _)| k == "content-length")
                        .and_then(|(_, v)| v.parse().ok())
                        .unwrap_or(0);
                    let start = buf.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
                    while buf.len() < start + len {
                        match tcp.read(&mut chunk).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => buf.extend_from_slice(&chunk[..n]),
                        }
                    }
                    let body = String::from_utf8_lossy(&buf[start..start + len]).into_owned();
                    let req = Seen {
                        method,
                        target,
                        headers,
                        body,
                    };
                    let (status, body) = route(&req);
                    log.lock().unwrap().push(req);
                    let reply = format!(
                        "HTTP/1.1 {status} X\r\nconnection: close\r\ncontent-length: {}\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = tcp.write_all(reply.as_bytes()).await;
                });
            }
        });
        Self { url, seen }
    }

    pub(crate) fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }
}

pub(crate) fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// `t` as RFC 3339, from the crate's own `amz_date` (whose every day is tested).
fn iso(t: u64) -> String {
    let a = pstore_blob::s3::sign::amz_date(t);
    format!(
        "{}-{}-{}T{}:{}:{}Z",
        &a[0..4],
        &a[4..6],
        &a[6..8],
        &a[9..11],
        &a[11..13],
        &a[13..15]
    )
}

/// The JSON IMDS and ECS answer, with the fake S3's own keys.
fn json_credentials(expires: u64) -> String {
    format!(
        r#"{{"Code":"Success","AccessKeyId":"{KEY_ID}","SecretAccessKey":"{SECRET}","Token":"session-token","Expiration":"{}"}}"#,
        iso(expires)
    )
}

/// An IMDSv2 fake whose credential answers come from `answers`, in order (the last repeats).
async fn imds(answers: Vec<(u16, String)>) -> Endpoint {
    let answers = Arc::new(Mutex::new(VecDeque::from(answers)));
    Endpoint::start(Arc::new(move |r: &Seen| {
        match (r.method.as_str(), r.target.as_str()) {
            ("PUT", "/latest/api/token")
                if r.header("x-aws-ec2-metadata-token-ttl-seconds").is_some() =>
            {
                (200, "imds-session".to_owned())
            }
            ("GET", t) if r.header("x-aws-ec2-metadata-token") != Some("imds-session") => {
                // IMDSv1: refused, as an instance with `HttpTokens=required` refuses it.
                (401, format!("no token on {t}"))
            }
            ("GET", "/latest/meta-data/iam/security-credentials/") => {
                (200, "the-role\n".to_owned())
            }
            ("GET", "/latest/meta-data/iam/security-credentials/the-role") => {
                let mut a = answers.lock().unwrap();
                if a.len() > 1 {
                    a.pop_front().unwrap()
                } else {
                    a.front().cloned().unwrap()
                }
            }
            _ => (404, String::new()),
        }
    }))
    .await
}

async fn store_with(fake: &FakeS3, provider: Provider) -> S3Backend {
    let mut cfg = fake.config();
    // Keys the fake refuses: only the provider's can sign a request it accepts.
    cfg.credentials.secret = "not-the-secret".to_owned();
    S3Backend::new(cfg, S3Backend::unprobed("fake"))
        .unwrap()
        .with_provider(provider)
        .unwrap()
}

fn credential_fetches(e: &Endpoint) -> usize {
    e.seen()
        .iter()
        .filter(|s| s.target.ends_with("/the-role"))
        .count()
}

#[tokio::test]
async fn imds_v2_takes_a_token_then_the_role_then_its_credentials() {
    let fake = FakeS3::start().await.unwrap();
    let meta = imds(vec![(200, json_credentials(unix_now() + 6 * 3600))]).await;
    let s = store_with(
        &fake,
        Provider::Imds {
            endpoint: meta.url.clone(),
        },
    )
    .await;
    s.put(&Key::new("k"), Bytes::from_static(b"v"))
        .await
        .unwrap();
    assert_eq!(
        s.get(&Key::new("k")).await.unwrap(),
        Bytes::from_static(b"v")
    );
    let seen = meta.seen();
    let order: Vec<_> = seen
        .iter()
        .map(|s| format!("{} {}", s.method, s.target))
        .collect();
    assert_eq!(
        order,
        [
            "PUT /latest/api/token",
            "GET /latest/meta-data/iam/security-credentials/",
            "GET /latest/meta-data/iam/security-credentials/the-role",
        ],
        "one fetch for two requests, in IMDSv2's order"
    );
    assert_eq!(
        seen[0].header("x-aws-ec2-metadata-token-ttl-seconds"),
        Some("21600")
    );
    assert_eq!(fake.bad_signatures(), 0);
}

#[tokio::test]
async fn an_instance_that_answers_no_session_token_is_refused_not_read_as_v1() {
    let fake = FakeS3::start().await.unwrap();
    // Serves v1 only: the token PUT is not found.
    let meta = Endpoint::start(Arc::new(|r: &Seen| match r.method.as_str() {
        "GET" => (200, json_credentials(unix_now() + 3600)),
        _ => (404, String::new()),
    }))
    .await;
    let s = store_with(
        &fake,
        Provider::Imds {
            endpoint: meta.url.clone(),
        },
    )
    .await;
    let err = s.get(&Key::new("k")).await.unwrap_err();
    assert!(format!("{err:?}").contains("imds token"), "{err:?}");
    assert_eq!(meta.seen().len(), 1, "no GET after a refused token");
    assert!(fake.requests().is_empty(), "nothing unsigned reached S3");
}

#[tokio::test]
async fn ecs_credentials_come_from_the_relative_uri_with_its_token() {
    let fake = FakeS3::start().await.unwrap();
    let ecs = Endpoint::start(Arc::new(|r: &Seen| {
        if r.target == "/v2/credentials/abc" && r.header("authorization") == Some("from-file") {
            (200, json_credentials(unix_now() + 3600))
        } else {
            (403, String::new())
        }
    }))
    .await;
    let file = std::env::temp_dir().join(format!("pstore-ecs-token-{}", std::process::id()));
    std::fs::write(&file, "from-file\n").unwrap();
    let provider = Provider::from_vars(
        |k| match k {
            "AWS_CONTAINER_CREDENTIALS_FULL_URI" => Some(format!("{}/v2/credentials/abc", ecs.url)),
            // The file wins over the variable: EKS Pod Identity rotates the file.
            "AWS_CONTAINER_AUTHORIZATION_TOKEN" => Some("stale".to_owned()),
            "AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE" => Some(file.display().to_string()),
            _ => None,
        },
        "us-east-1",
    )
    .unwrap();
    let s = store_with(&fake, provider).await;
    s.put(&Key::new("k"), Bytes::from_static(b"v"))
        .await
        .unwrap();
    assert_eq!(ecs.seen().len(), 1);
    assert_eq!(fake.bad_signatures(), 0);
    std::fs::remove_file(&file).unwrap();

    // And with only the variable.
    let ecs2 = Endpoint::start(Arc::new(|r: &Seen| {
        if r.header("authorization") == Some("tok") {
            (200, json_credentials(unix_now() + 3600))
        } else {
            (403, String::new())
        }
    }))
    .await;
    let s = store_with(
        &fake,
        Provider::Ecs {
            uri: format!("{}/c", ecs2.url),
            token: Some("tok".to_owned()),
            token_file: None,
        },
    )
    .await;
    s.put(&Key::new("k2"), Bytes::from_static(b"v"))
        .await
        .unwrap();
    assert_eq!(fake.bad_signatures(), 0);
}

#[tokio::test]
async fn web_identity_is_exchanged_with_sts() {
    let fake = FakeS3::start().await.unwrap();
    let expires = iso(unix_now() + 3600);
    let sts = Endpoint::start(Arc::new(move |r: &Seen| {
        let q = r.body.as_str();
        let has = |p: &str| q.split('&').any(|x| x == p);
        if r.method == "POST"
            && r.target == "/"
            && r.header("content-type") == Some("application/x-www-form-urlencoded")
            && has("Action=AssumeRoleWithWebIdentity")
            && has("Version=2011-06-15")
            && has("RoleArn=arn%3Aaws%3Aiam%3A%3A1%3Arole%2Fr")
            && has("RoleSessionName=pstore")
            && has("WebIdentityToken=the.jwt")
        {
            (
                200,
                format!(
                    "<AssumeRoleWithWebIdentityResponse><AssumeRoleWithWebIdentityResult>\
                     <Credentials><SessionToken>st</SessionToken>\
                     <SecretAccessKey>{SECRET}</SecretAccessKey>\
                     <Expiration>{expires}</Expiration>\
                     <AccessKeyId>{KEY_ID}</AccessKeyId></Credentials>\
                     </AssumeRoleWithWebIdentityResult></AssumeRoleWithWebIdentityResponse>"
                ),
            )
        } else {
            (
                400,
                format!("<Error><Code>Bad</Code><Message>{q}</Message></Error>"),
            )
        }
    }))
    .await;
    let file = std::env::temp_dir().join(format!("pstore-irsa-token-{}", std::process::id()));
    std::fs::write(&file, "the.jwt\n").unwrap();
    let s = store_with(
        &fake,
        Provider::WebIdentity {
            token_file: file.clone(),
            role_arn: "arn:aws:iam::1:role/r".to_owned(),
            session: "pstore".to_owned(),
            sts: sts.url.clone(),
        },
    )
    .await;
    s.put(&Key::new("k"), Bytes::from_static(b"v"))
        .await
        .unwrap();
    assert_eq!(sts.seen().len(), 1);
    assert_eq!(fake.bad_signatures(), 0);

    // An unreadable token is an error, not a fall-through to another provider.
    std::fs::remove_file(&file).unwrap();
    let s = store_with(
        &fake,
        Provider::WebIdentity {
            token_file: file,
            role_arn: "r".to_owned(),
            session: "pstore".to_owned(),
            sts: sts.url.clone(),
        },
    )
    .await;
    assert!(s.get(&Key::new("k")).await.is_err());
    assert_eq!(sts.seen().len(), 1);
}

#[tokio::test]
async fn credentials_refresh_before_they_expire() {
    let fake = FakeS3::start().await.unwrap();
    let now = unix_now();
    // First due within the refresh window, then good for hours.
    let meta = imds(vec![
        (200, json_credentials(now + 200)),
        (200, json_credentials(now + 6 * 3600)),
    ])
    .await;
    let s = store_with(
        &fake,
        Provider::Imds {
            endpoint: meta.url.clone(),
        },
    )
    .await;
    for i in 0..3 {
        s.put(&Key::new(format!("k{i}")), Bytes::from_static(b"v"))
            .await
            .unwrap();
    }
    assert_eq!(
        credential_fetches(&meta),
        2,
        "fetched once, refreshed once ahead of expiry, then cached"
    );
    assert_eq!(fake.bad_signatures(), 0);
}

/// The cache's bounds are strict: credentials exactly [`REFRESH_AHEAD`] from expiry are
/// due, and credentials expiring this second are expired. Each assertion holds whatever the
/// clock does between the two requests; only a bound moved to `<=` changes the count.
#[tokio::test]
async fn the_refresh_bounds_are_strict() {
    use pstore_blob::s3::creds::REFRESH_AHEAD;
    let fake = FakeS3::start().await.unwrap();
    let now = unix_now();
    let meta = imds(vec![
        (200, json_credentials(now + REFRESH_AHEAD)),
        (200, json_credentials(now + 6 * 3600)),
    ])
    .await;
    let s = store_with(
        &fake,
        Provider::Imds {
            endpoint: meta.url.clone(),
        },
    )
    .await;
    for i in 0..2 {
        s.put(&Key::new(format!("k{i}")), Bytes::from_static(b"v"))
            .await
            .unwrap();
    }
    assert_eq!(credential_fetches(&meta), 2, "due at exactly the window");

    // Expiring now, and the refresh fails: expired, so an error, not the old keys.
    let meta = imds(vec![(200, json_credentials(now)), (500, "busy".to_owned())]).await;
    let s = store_with(
        &fake,
        Provider::Imds {
            endpoint: meta.url.clone(),
        },
    )
    .await;
    s.put(&Key::new("first"), Bytes::from_static(b"v"))
        .await
        .unwrap();
    assert!(s.put(&Key::new("second"), Bytes::new()).await.is_err());
}

/// Callers that queue behind one refresh use what it fetched, and fetch again only when
/// that is itself already expired.
#[tokio::test]
async fn callers_waiting_on_a_refresh_share_its_answer() {
    let fake = FakeS3::start().await.unwrap();
    let now = unix_now();
    let meta = imds(vec![(200, json_credentials(now + 6 * 3600))]).await;
    let s = store_with(
        &fake,
        Provider::Imds {
            endpoint: meta.url.clone(),
        },
    )
    .await;
    let (ka, kb, kc) = (Key::new("a"), Key::new("b"), Key::new("c"));
    let (a, b, c) = tokio::join!(
        s.put(&ka, Bytes::from_static(b"v")),
        s.put(&kb, Bytes::from_static(b"v")),
        s.put(&kc, Bytes::from_static(b"v")),
    );
    a.unwrap();
    b.unwrap();
    c.unwrap();
    assert_eq!(credential_fetches(&meta), 1);

    // What the first caller fetched expires this second: the second caller fetches again.
    let meta = imds(vec![(200, json_credentials(unix_now()))]).await;
    let s = store_with(
        &fake,
        Provider::Imds {
            endpoint: meta.url.clone(),
        },
    )
    .await;
    let (kd, ke) = (Key::new("d"), Key::new("e"));
    let (a, b) = tokio::join!(
        s.put(&kd, Bytes::from_static(b"v")),
        s.put(&ke, Bytes::from_static(b"v")),
    );
    a.unwrap();
    b.unwrap();
    assert_eq!(credential_fetches(&meta), 2);
    assert_eq!(fake.bad_signatures(), 0);
}

#[tokio::test]
async fn a_failed_refresh_keeps_credentials_that_have_not_expired() {
    let fake = FakeS3::start().await.unwrap();
    let now = unix_now();
    let meta = imds(vec![
        (200, json_credentials(now + 200)),
        (500, "busy".to_owned()),
    ])
    .await;
    let s = store_with(
        &fake,
        Provider::Imds {
            endpoint: meta.url.clone(),
        },
    )
    .await;
    for i in 0..3 {
        s.put(&Key::new(format!("k{i}")), Bytes::from_static(b"v"))
            .await
            .unwrap();
    }
    assert_eq!(
        credential_fetches(&meta),
        3,
        "each request in the window tries again, and signs with what it has"
    );

    // Expired, and the refresh fails: an error, never a request signed with dead keys.
    let meta = imds(vec![
        (200, json_credentials(now - 10)),
        (500, "busy".to_owned()),
    ])
    .await;
    let s = store_with(
        &fake,
        Provider::Imds {
            endpoint: meta.url.clone(),
        },
    )
    .await;
    s.put(&Key::new("first"), Bytes::from_static(b"v"))
        .await
        .unwrap();
    let before = fake.requests().len();
    assert!(s.get(&Key::new("first")).await.is_err());
    assert_eq!(fake.requests().len(), before);
    assert_eq!(credential_fetches(&meta), 2);
}

#[tokio::test]
async fn credential_answers_missing_a_field_are_errors() {
    let fake = FakeS3::start().await.unwrap();
    for body in [
        "not json".to_owned(),
        format!(r#"{{"AccessKeyId":"{KEY_ID}"}}"#),
        format!(r#"{{"AccessKeyId":"{KEY_ID}","SecretAccessKey":"{SECRET}","Expiration":"soon"}}"#),
    ] {
        let meta = imds(vec![(200, body.clone())]).await;
        let s = store_with(
            &fake,
            Provider::Imds {
                endpoint: meta.url.clone(),
            },
        )
        .await;
        assert!(s.get(&Key::new("k")).await.is_err(), "{body}");
    }
    // No Token and no Expiration: long-lived keys, accepted.
    let meta = imds(vec![(
        200,
        format!(r#"{{"AccessKeyId":"{KEY_ID}","SecretAccessKey":"{SECRET}"}}"#),
    )])
    .await;
    let s = store_with(
        &fake,
        Provider::Imds {
            endpoint: meta.url.clone(),
        },
    )
    .await;
    s.put(&Key::new("k"), Bytes::from_static(b"v"))
        .await
        .unwrap();
    s.put(&Key::new("k"), Bytes::from_static(b"v"))
        .await
        .unwrap();
    assert_eq!(credential_fetches(&meta), 1);
    // An instance with no role.
    let meta = Endpoint::start(Arc::new(|r: &Seen| match r.method.as_str() {
        "PUT" => (200, "t".to_owned()),
        _ => (200, "\n".to_owned()),
    }))
    .await;
    let s = store_with(
        &fake,
        Provider::Imds {
            endpoint: meta.url.clone(),
        },
    )
    .await;
    let err = s.get(&Key::new("k")).await.unwrap_err();
    assert!(format!("{err:?}").contains("no IAM role"), "{err:?}");
}

#[tokio::test]
async fn static_keys_through_the_provider_need_no_endpoint() {
    let fake = FakeS3::start().await.unwrap();
    let s = store_with(&fake, Provider::Static(fake.config().credentials)).await;
    s.put(&Key::new("k"), Bytes::from_static(b"v"))
        .await
        .unwrap();
    assert_eq!(fake.bad_signatures(), 0);
}
