//! Where S3 credentials come from (M287): the chain `object_store` and the AWS SDKs walk,
//! and a cache that refreshes temporary credentials before they expire.
//!
//! ⚠️ **Chosen by configuration, not by probing.** The first provider whose configuration is
//! present is the provider, in the SDKs' order: static keys, `AWS_*` keys, web identity, the
//! ECS container endpoint, then IMDSv2. A provider that is configured and fails is an error,
//! never a fall-through to the next: a pod whose IRSA token is unreadable must not quietly
//! sign as the node's instance role.
//!
//! ⚠️ **IMDSv2 only.** IMDSv1 answers any process that can reach `169.254.169.254` with no
//! token, which is the SSRF the session token exists to stop. A host that serves only v1 is
//! refused, not served.

use super::sign::Credentials;
use super::xml;
use crate::BlobError;
use bytes::Bytes;
use http::{Method, StatusCode};
use http_body_util::{BodyExt, Full};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

/// Temporary credentials are refreshed this long before they expire.
pub const REFRESH_AHEAD: u64 = 300;

/// One provider of the chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Provider {
    /// Keys that never expire: the operator's, or `AWS_ACCESS_KEY_ID`'s.
    Static(Credentials),
    /// EKS IRSA: a token file exchanged with STS `AssumeRoleWithWebIdentity`.
    WebIdentity {
        /// `AWS_WEB_IDENTITY_TOKEN_FILE`, read at every refresh: the kubelet rotates it.
        token_file: std::path::PathBuf,
        /// `AWS_ROLE_ARN`.
        role_arn: String,
        /// `AWS_ROLE_SESSION_NAME`, `pstore` unset.
        session: String,
        /// STS's root: `AWS_ENDPOINT_URL_STS`, or `https://sts.{region}.amazonaws.com`.
        sts: String,
    },
    /// The ECS (and EKS Pod Identity) container endpoint.
    Ecs {
        /// `http://169.254.170.2` + `AWS_CONTAINER_CREDENTIALS_RELATIVE_URI`, or `_FULL_URI`.
        uri: String,
        /// `AWS_CONTAINER_AUTHORIZATION_TOKEN`.
        token: Option<String>,
        /// `AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE`, read at every refresh. Wins over `token`.
        token_file: Option<std::path::PathBuf>,
    },
    /// EC2's instance metadata service, v2.
    Imds {
        /// `AWS_EC2_METADATA_SERVICE_ENDPOINT`, `http://169.254.169.254` unset.
        endpoint: String,
    },
}

/// Credentials and when they stop working, in Unix seconds (`None`: never).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Expiring {
    /// The keys.
    pub credentials: Arc<Credentials>,
    /// When AWS stops accepting them.
    pub expires: Option<u64>,
}

impl Provider {
    /// The chain's choice for `lookup` (the environment, in the server), with `region` for
    /// STS. An empty variable counts as unset.
    ///
    /// # Errors
    /// `AWS_EC2_METADATA_DISABLED=true` with nothing else configured: no provider at all,
    /// which is refused rather than sent unsigned.
    pub fn from_vars(
        lookup: impl Fn(&str) -> Option<String>,
        region: &str,
    ) -> Result<Self, BlobError> {
        let var = |k: &str| lookup(k).filter(|v| !v.is_empty());
        if let Ok(c) = super::S3Config::credentials_from(&lookup) {
            return Ok(Self::Static(c));
        }
        if let (Some(file), Some(role_arn)) =
            (var("AWS_WEB_IDENTITY_TOKEN_FILE"), var("AWS_ROLE_ARN"))
        {
            return Ok(Self::WebIdentity {
                token_file: file.into(),
                role_arn,
                session: var("AWS_ROLE_SESSION_NAME").unwrap_or_else(|| "pstore".to_owned()),
                sts: var("AWS_ENDPOINT_URL_STS")
                    .unwrap_or_else(|| format!("https://sts.{region}.amazonaws.com")),
            });
        }
        let ecs = var("AWS_CONTAINER_CREDENTIALS_RELATIVE_URI")
            .map(|rel| format!("http://169.254.170.2{rel}"))
            .or_else(|| var("AWS_CONTAINER_CREDENTIALS_FULL_URI"));
        if let Some(uri) = ecs {
            return Ok(Self::Ecs {
                uri,
                token: var("AWS_CONTAINER_AUTHORIZATION_TOKEN"),
                token_file: var("AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE").map(Into::into),
            });
        }
        if var("AWS_EC2_METADATA_DISABLED").is_some_and(|v| v.eq_ignore_ascii_case("true")) {
            return Err(BlobError::Other(
                "no S3 credentials: no keys, no web identity, no container endpoint, and \
                 AWS_EC2_METADATA_DISABLED=true"
                    .to_owned(),
            ));
        }
        Ok(Self::Imds {
            endpoint: var("AWS_EC2_METADATA_SERVICE_ENDPOINT")
                .unwrap_or_else(|| "http://169.254.169.254".to_owned()),
        })
    }

    /// Asks the provider once.
    async fn fetch(&self, http: &super::Http) -> Result<Expiring, BlobError> {
        match self {
            Self::Static(c) => Ok(Expiring {
                credentials: Arc::new(c.clone()),
                expires: None,
            }),
            Self::WebIdentity {
                token_file,
                role_arn,
                session,
                sts,
            } => {
                let token = read_token(token_file)?;
                let query = super::sign::canonical_query(&[
                    ("Action", "AssumeRoleWithWebIdentity"),
                    ("RoleArn", role_arn),
                    ("RoleSessionName", session),
                    ("Version", "2011-06-15"),
                    ("WebIdentityToken", &token),
                ]);
                // A form POST, as the SDKs send it: the token stays out of the URL, and out of
                // any proxy's access log.
                let body = send(
                    http,
                    Method::POST,
                    &format!("{}/", sts.trim_end_matches('/')),
                    &[("content-type", "application/x-www-form-urlencoded")],
                    Bytes::from(query),
                    "sts",
                )
                .await?;
                let field = |name: &str| {
                    xml::text(&body, name)
                        .ok_or_else(|| BlobError::Other(format!("sts: no {name} in the answer")))
                };
                Ok(Expiring {
                    credentials: Arc::new(Credentials {
                        key_id: field("AccessKeyId")?,
                        secret: field("SecretAccessKey")?,
                        token: Some(field("SessionToken")?),
                    }),
                    expires: Some(rfc3339(&field("Expiration")?)?),
                })
            }
            Self::Ecs {
                uri,
                token,
                token_file,
            } => {
                let token = match token_file {
                    Some(f) => Some(read_token(f)?),
                    None => token.clone(),
                };
                let headers: Vec<(&str, &str)> = token
                    .as_deref()
                    .map(|t| ("authorization", t))
                    .into_iter()
                    .collect();
                let body = get(http, Method::GET, uri, &headers, "container credentials").await?;
                from_json(&body, "container credentials")
            }
            Self::Imds { endpoint } => {
                let root = endpoint.trim_end_matches('/');
                let session = get(
                    http,
                    Method::PUT,
                    &format!("{root}/latest/api/token"),
                    &[("x-aws-ec2-metadata-token-ttl-seconds", "21600")],
                    "imds token",
                )
                .await?;
                let session = String::from_utf8_lossy(&session).trim().to_owned();
                let auth = [("x-aws-ec2-metadata-token", session.as_str())];
                let base = format!("{root}/latest/meta-data/iam/security-credentials/");
                let roles = get(http, Method::GET, &base, &auth, "imds role").await?;
                let roles = String::from_utf8_lossy(&roles);
                let role = roles
                    .lines()
                    .map(str::trim)
                    .find(|l| !l.is_empty())
                    .ok_or_else(|| {
                        BlobError::Other("imds: this instance has no IAM role".to_owned())
                    })?;
                let body = get(
                    http,
                    Method::GET,
                    &format!("{base}{role}"),
                    &auth,
                    "imds credentials",
                )
                .await?;
                from_json(&body, "imds credentials")
            }
        }
    }
}

fn read_token(path: &std::path::Path) -> Result<String, BlobError> {
    std::fs::read_to_string(path)
        .map(|t| t.trim().to_owned())
        .map_err(|e| BlobError::Other(format!("cannot read {}: {e}", path.display())))
}

/// One bodiless request to a credential endpoint: [`send`].
async fn get(
    http: &super::Http,
    method: Method,
    uri: &str,
    headers: &[(&str, &str)],
    what: &str,
) -> Result<Bytes, BlobError> {
    send(http, method, uri, headers, Bytes::new(), what).await
}

/// One request to a credential endpoint, and its body if it answered 200.
async fn send(
    http: &super::Http,
    method: Method,
    uri: &str,
    headers: &[(&str, &str)],
    body: Bytes,
    what: &str,
) -> Result<Bytes, BlobError> {
    let mut req = http::Request::builder().method(method).uri(uri);
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let req = req
        .body(Full::new(body))
        .map_err(|e| BlobError::Other(format!("{what}: {e}")))?;
    let exchange = async {
        let resp = http
            .request(req)
            .await
            .map_err(|e| BlobError::Other(format!("{what}: {e}")))?;
        let status = resp.status();
        let body = resp
            .into_body()
            .collect()
            .await
            .map_err(|e| BlobError::Other(format!("{what}: {e}")))?
            .to_bytes();
        if status == StatusCode::OK {
            Ok(body)
        } else {
            Err(BlobError::Other(format!(
                "{what}: {status}: {}",
                String::from_utf8_lossy(&body)
            )))
        }
    };
    tokio::time::timeout(Duration::from_secs(5), exchange)
        .await
        .map_err(|_| BlobError::Other(format!("{what}: no answer within 5 s")))?
}

/// The JSON the ECS endpoint and IMDS both answer.
fn from_json(body: &[u8], what: &str) -> Result<Expiring, BlobError> {
    let v: serde_json::Value = serde_json::from_slice(body)
        .map_err(|e| BlobError::Other(format!("{what}: not JSON: {e}")))?;
    let field = |name: &str| {
        v.get(name)
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| BlobError::Other(format!("{what}: no {name} in the answer")))
    };
    Ok(Expiring {
        credentials: Arc::new(Credentials {
            key_id: field("AccessKeyId")?,
            secret: field("SecretAccessKey")?,
            token: field("Token").ok(),
        }),
        expires: match field("Expiration") {
            Ok(e) => Some(rfc3339(&e)?),
            Err(_) => None,
        },
    })
}

/// `2026-11-19T12:00:00Z`, with optional fractional seconds, as Unix seconds. Only `Z` (and
/// `+00:00`): every AWS credential endpoint answers in UTC, and an offset this does not
/// understand is refused rather than misread.
///
/// # Errors
/// Anything else.
pub fn rfc3339(s: &str) -> Result<u64, BlobError> {
    let bad = || BlobError::Other(format!("not an RFC 3339 UTC time: {s:?}"));
    let s = s
        .strip_suffix('Z')
        .or_else(|| s.strip_suffix("+00:00"))
        .ok_or_else(bad)?;
    let (date, time) = s.split_once('T').ok_or_else(bad)?;
    let time = time.split_once('.').map_or(time, |(t, _)| t);
    let num = |p: Option<&str>, len: usize| -> Result<i64, BlobError> {
        p.filter(|p| p.len() == len && p.bytes().all(|b| b.is_ascii_digit()))
            .and_then(|p| p.parse().ok())
            .ok_or_else(bad)
    };
    let mut d = date.split('-');
    let (y, m, day) = (num(d.next(), 4)?, num(d.next(), 2)?, num(d.next(), 2)?);
    let mut t = time.split(':');
    let (h, min, sec) = (num(t.next(), 2)?, num(t.next(), 2)?, num(t.next(), 2)?);
    if d.next().is_some()
        || t.next().is_some()
        || !(1..=12).contains(&m)
        || !(1..=31).contains(&day)
        || h > 23
        || min > 59
        || sec > 60
    {
        return Err(bad());
    }
    // Hinnant's days_from_civil, the inverse of `amz_date`'s.
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    u64::try_from(days * 86_400 + h * 3600 + min * 60 + sec).map_err(|_| bad())
}

/// A provider and what it last answered.
#[derive(Debug)]
pub struct Cache {
    provider: Provider,
    http: super::Http,
    current: Mutex<Option<Expiring>>,
    refresh: tokio::sync::Mutex<()>,
}

impl Cache {
    /// Nothing fetched yet: the first request fetches.
    #[must_use]
    pub fn new(provider: Provider, http: super::Http) -> Self {
        Self {
            provider,
            http,
            current: Mutex::new(None),
            refresh: tokio::sync::Mutex::new(()),
        }
    }

    fn cached(&self) -> Option<Expiring> {
        self.current
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// The credentials to sign with now.
    ///
    /// - Fresh (more than [`REFRESH_AHEAD`] from expiry): the cached ones, no request.
    /// - Due but unexpired: one caller refreshes; any other caller meanwhile signs with the
    ///   cached ones rather than waiting. A failed refresh keeps them.
    /// - Expired or none: callers wait for one refresh.
    ///
    /// # Errors
    /// The provider's, when there is nothing unexpired to fall back on.
    pub async fn get(&self) -> Result<Arc<Credentials>, BlobError> {
        let now = now();
        let cached = self.cached();
        if let Some(c) = &cached {
            match c.expires {
                None => return Ok(c.credentials.clone()),
                Some(e) if now + REFRESH_AHEAD < e => return Ok(c.credentials.clone()),
                Some(e) if now < e => {
                    // Due: refresh unless someone already is.
                    let Ok(_guard) = self.refresh.try_lock() else {
                        return Ok(c.credentials.clone());
                    };
                    return Ok(match self.provider.fetch(&self.http).await {
                        Ok(fresh) => self.store(fresh),
                        Err(_) => c.credentials.clone(),
                    });
                }
                Some(_) => {}
            }
        }
        let _guard = self.refresh.lock().await;
        // Someone else may have refreshed while this waited.
        if let Some(c) = self.cached()
            && c.expires.is_none_or(|e| self::now() < e)
            && Some(&c) != cached.as_ref()
        {
            return Ok(c.credentials);
        }
        Ok(self.store(self.provider.fetch(&self.http).await?))
    }

    fn store(&self, fresh: Expiring) -> Arc<Credentials> {
        let out = fresh.credentials.clone();
        *self.current.lock().unwrap_or_else(PoisonError::into_inner) = Some(fresh);
        out
    }
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    reason = "assertions in tests are the reporting mechanism"
)]
mod tests {
    use super::*;

    /// Every second of a few days around each month end and leap day, and one a day for
    /// 600 years: `rfc3339` is `amz_date`'s inverse.
    #[test]
    fn rfc3339_round_trips() {
        let check = |t: u64| {
            let a = super::super::sign::amz_date(t);
            let iso = format!(
                "{}-{}-{}T{}:{}:{}Z",
                &a[0..4],
                &a[4..6],
                &a[6..8],
                &a[9..11],
                &a[11..13],
                &a[13..15]
            );
            assert_eq!(rfc3339(&iso).unwrap(), t, "{iso}");
        };
        for day in 0..220_000_u64 {
            check(day * 86_400 + 43_261);
        }
        for t in 951_696_000..951_696_000 + 3 * 86_400 {
            check(t);
        }
        assert_eq!(rfc3339("2026-11-19T12:00:00.123Z").unwrap(), 1_795_089_600);
        assert_eq!(rfc3339("2026-11-19T12:00:00+00:00").unwrap(), 1_795_089_600);
    }

    #[test]
    fn rfc3339_refuses_what_it_does_not_understand() {
        for bad in [
            "2026-11-19T12:00:00",
            "2026-11-19T12:00:00+01:00",
            "2026-13-19T12:00:00Z",
            "2026-11-32T12:00:00Z",
            "2026-11-19T24:00:00Z",
            "2026-11-19T12:60:00Z",
            "2026-11-19T12:00:61Z",
            "2026-11-19 12:00:00Z",
            "26-11-19T12:00:00Z",
            "2026-11-19-01T12:00:00Z",
            "2026-11-19T12:00:00:00Z",
            "2026-1a-19T12:00:00Z",
            // A field of the wrong width, and one that `parse` would take: each check alone
            // lets one through.
            "2026-011-19T12:00:00Z",
            "2026-+1-19T12:00:00Z",
            "1969-12-31T23:59:59Z",
        ] {
            assert!(rfc3339(bad).is_err(), "{bad}");
        }
        assert_eq!(rfc3339("1970-01-01T00:00:00Z").unwrap(), 0);
        assert_eq!(rfc3339("2016-12-31T23:59:60Z").unwrap(), 1_483_228_800);
    }

    fn vars<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |k| {
            pairs
                .iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| (*v).to_owned())
        }
    }

    #[test]
    fn the_chain_takes_the_first_provider_that_is_configured() {
        let keys = [("AWS_ACCESS_KEY_ID", "a"), ("AWS_SECRET_ACCESS_KEY", "s")];
        let web = [
            ("AWS_WEB_IDENTITY_TOKEN_FILE", "/t"),
            ("AWS_ROLE_ARN", "arn:aws:iam::1:role/r"),
        ];
        let ecs = [("AWS_CONTAINER_CREDENTIALS_RELATIVE_URI", "/v2/creds/x")];
        let all: Vec<_> = keys.iter().chain(&web).chain(&ecs).copied().collect();
        assert!(matches!(
            Provider::from_vars(vars(&all), "eu-west-1").unwrap(),
            Provider::Static(_)
        ));
        let no_keys: Vec<_> = web.iter().chain(&ecs).copied().collect();
        assert_eq!(
            Provider::from_vars(vars(&no_keys), "eu-west-1").unwrap(),
            Provider::WebIdentity {
                token_file: "/t".into(),
                role_arn: "arn:aws:iam::1:role/r".to_owned(),
                session: "pstore".to_owned(),
                sts: "https://sts.eu-west-1.amazonaws.com".to_owned(),
            }
        );
        // Half a web identity is no web identity.
        let half: Vec<_> = [web[0]].iter().chain(&ecs).copied().collect();
        assert_eq!(
            Provider::from_vars(vars(&half), "eu-west-1").unwrap(),
            Provider::Ecs {
                uri: "http://169.254.170.2/v2/creds/x".to_owned(),
                token: None,
                token_file: None,
            }
        );
        let full = [
            ("AWS_CONTAINER_CREDENTIALS_FULL_URI", "http://127.0.0.1:9/c"),
            ("AWS_CONTAINER_AUTHORIZATION_TOKEN", "tok"),
            ("AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE", "/f"),
        ];
        assert_eq!(
            Provider::from_vars(vars(&full), "eu-west-1").unwrap(),
            Provider::Ecs {
                uri: "http://127.0.0.1:9/c".to_owned(),
                token: Some("tok".to_owned()),
                token_file: Some("/f".into()),
            }
        );
        assert_eq!(
            Provider::from_vars(vars(&[]), "eu-west-1").unwrap(),
            Provider::Imds {
                endpoint: "http://169.254.169.254".to_owned()
            }
        );
        assert_eq!(
            Provider::from_vars(vars(&[("AWS_EC2_METADATA_DISABLED", "false")]), "eu-west-1")
                .unwrap(),
            Provider::Imds {
                endpoint: "http://169.254.169.254".to_owned()
            }
        );
        assert!(Provider::from_vars(vars(&[("AWS_EC2_METADATA_DISABLED", "TRUE")]), "x").is_err());
        let named = [
            ("AWS_WEB_IDENTITY_TOKEN_FILE", "/t"),
            ("AWS_ROLE_ARN", "r"),
            ("AWS_ROLE_SESSION_NAME", "s"),
            ("AWS_ENDPOINT_URL_STS", "http://127.0.0.1:1"),
        ];
        assert_eq!(
            Provider::from_vars(vars(&named), "x").unwrap(),
            Provider::WebIdentity {
                token_file: "/t".into(),
                role_arn: "r".to_owned(),
                session: "s".to_owned(),
                sts: "http://127.0.0.1:1".to_owned(),
            }
        );
        assert_eq!(
            Provider::from_vars(
                vars(&[("AWS_EC2_METADATA_SERVICE_ENDPOINT", "http://127.0.0.1:2")]),
                "x"
            )
            .unwrap(),
            Provider::Imds {
                endpoint: "http://127.0.0.1:2".to_owned()
            }
        );
    }
}
