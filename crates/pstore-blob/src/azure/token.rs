//! Where an Azure bearer token comes from when there is no account key (M288).
//!
//! ⚠️ **Chosen by configuration, as M287's S3 chain is.** The first provider whose variables
//! are present is the provider: a client secret, then workload identity, then the managed
//! identity of App Service or Container Apps (`IDENTITY_ENDPOINT`), then IMDS. A configured
//! provider that fails is an error, never a fall-through to the next. Tokens are cached and
//! refreshed by M287's rule, through the same [`crate::s3::creds::Cache`].
//!
//! No certificate credentials: they would need `rsa`, and nothing deployed asks for them.

use crate::BlobError;
use crate::s3::creds::{self, Expiring};
use bytes::Bytes;
use http::Method;
use std::sync::Arc;

/// The token's audience: Azure Storage.
pub const RESOURCE: &str = "https://storage.azure.com";
/// The OAuth v2 scope for it.
pub const SCOPE: &str = "https://storage.azure.com/.default";

/// One provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Provider {
    /// A service principal's secret: `AZURE_CLIENT_ID`, `AZURE_TENANT_ID`, `AZURE_CLIENT_SECRET`.
    ClientSecret {
        /// `{authority}/{tenant}/oauth2/v2.0/token`.
        token_url: String,
        /// `AZURE_CLIENT_ID`.
        client_id: String,
        /// `AZURE_CLIENT_SECRET`.
        secret: String,
    },
    /// AKS workload identity: a federated token file exchanged for an access token.
    Workload {
        /// `{authority}/{tenant}/oauth2/v2.0/token`.
        token_url: String,
        /// `AZURE_CLIENT_ID`.
        client_id: String,
        /// `AZURE_FEDERATED_TOKEN_FILE`, read at every refresh: the kubelet rotates it.
        token_file: std::path::PathBuf,
    },
    /// App Service and Container Apps: `IDENTITY_ENDPOINT` with `IDENTITY_HEADER`.
    IdentityEndpoint {
        /// `IDENTITY_ENDPOINT`.
        endpoint: String,
        /// `IDENTITY_HEADER`, sent as `X-IDENTITY-HEADER`.
        header: String,
        /// `AZURE_CLIENT_ID`, for a user-assigned identity.
        client_id: Option<String>,
    },
    /// A VM's or VMSS's managed identity, through IMDS.
    Imds {
        /// `AZURE_IMDS_ENDPOINT`, `http://169.254.169.254` unset.
        endpoint: String,
        /// `AZURE_CLIENT_ID`, for a user-assigned identity.
        client_id: Option<String>,
    },
}

impl Provider {
    /// The chain's choice for `lookup`. An empty variable counts as unset.
    #[must_use]
    pub fn from_vars(lookup: impl Fn(&str) -> Option<String>) -> Self {
        let var = |k: &str| lookup(k).filter(|v| !v.is_empty());
        let authority = var("AZURE_AUTHORITY_HOST")
            .unwrap_or_else(|| "https://login.microsoftonline.com".to_owned());
        let token_url = |tenant: &str| {
            format!(
                "{}/{tenant}/oauth2/v2.0/token",
                authority.trim_end_matches('/')
            )
        };
        let client_id = var("AZURE_CLIENT_ID");
        if let (Some(id), Some(tenant), Some(secret)) = (
            client_id.clone(),
            var("AZURE_TENANT_ID"),
            var("AZURE_CLIENT_SECRET"),
        ) {
            return Self::ClientSecret {
                token_url: token_url(&tenant),
                client_id: id,
                secret,
            };
        }
        if let (Some(id), Some(tenant), Some(file)) = (
            client_id.clone(),
            var("AZURE_TENANT_ID"),
            var("AZURE_FEDERATED_TOKEN_FILE"),
        ) {
            return Self::Workload {
                token_url: token_url(&tenant),
                client_id: id,
                token_file: file.into(),
            };
        }
        if let (Some(endpoint), Some(header)) = (var("IDENTITY_ENDPOINT"), var("IDENTITY_HEADER")) {
            return Self::IdentityEndpoint {
                endpoint,
                header,
                client_id,
            };
        }
        Self::Imds {
            endpoint: var("AZURE_IMDS_ENDPOINT")
                .unwrap_or_else(|| "http://169.254.169.254".to_owned()),
            client_id,
        }
    }

    async fn fetch_once(&self, http: &crate::s3::Http) -> Result<Expiring<String>, BlobError> {
        match self {
            Self::ClientSecret {
                token_url,
                client_id,
                secret,
            } => {
                let form = form(&[
                    ("client_id", client_id),
                    ("client_secret", secret),
                    ("scope", SCOPE),
                    ("grant_type", "client_credentials"),
                ]);
                oauth(http, token_url, form, "azure client secret").await
            }
            Self::Workload {
                token_url,
                client_id,
                token_file,
            } => {
                let assertion = creds::read_token(token_file)?;
                let form = form(&[
                    ("client_id", client_id),
                    (
                        "client_assertion_type",
                        "urn:ietf:params:oauth:client-assertion-type:jwt-bearer",
                    ),
                    ("client_assertion", &assertion),
                    ("scope", SCOPE),
                    ("grant_type", "client_credentials"),
                ]);
                oauth(http, token_url, form, "azure workload identity").await
            }
            Self::IdentityEndpoint {
                endpoint,
                header,
                client_id,
            } => {
                let uri = msi_uri(endpoint, "2019-08-01", client_id.as_deref());
                let body = creds::get(
                    http,
                    Method::GET,
                    &uri,
                    &[("x-identity-header", header)],
                    "azure identity endpoint",
                )
                .await?;
                token_json(&body, "azure identity endpoint")
            }
            Self::Imds {
                endpoint,
                client_id,
            } => {
                let uri = msi_uri(
                    &format!(
                        "{}/metadata/identity/oauth2/token",
                        endpoint.trim_end_matches('/')
                    ),
                    "2019-08-01",
                    client_id.as_deref(),
                );
                let body = creds::get(
                    http,
                    Method::GET,
                    &uri,
                    &[("metadata", "true")],
                    "azure imds",
                )
                .await?;
                token_json(&body, "azure imds")
            }
        }
    }
}

impl creds::Source for Provider {
    type Secret = String;
    fn fetch(
        &self,
        http: &crate::s3::Http,
    ) -> impl std::future::Future<Output = Result<Expiring<String>, BlobError>> + Send {
        self.fetch_once(http)
    }
}

fn form(pairs: &[(&str, &str)]) -> Bytes {
    let enc = |s: &str| crate::s3::sign::uri_encode(s, false);
    Bytes::from(
        pairs
            .iter()
            .map(|(k, v)| format!("{}={}", enc(k), enc(v)))
            .collect::<Vec<_>>()
            .join("&"),
    )
}

fn msi_uri(endpoint: &str, version: &str, client_id: Option<&str>) -> String {
    let enc = |s: &str| crate::s3::sign::uri_encode(s, false);
    let mut uri = format!(
        "{endpoint}?api-version={version}&resource={}",
        enc(RESOURCE)
    );
    if let Some(id) = client_id {
        uri.push_str("&client_id=");
        uri.push_str(&enc(id));
    }
    uri
}

async fn oauth(
    http: &crate::s3::Http,
    url: &str,
    form: Bytes,
    what: &str,
) -> Result<Expiring<String>, BlobError> {
    let body = creds::send(
        http,
        Method::POST,
        url,
        &[
            ("content-type", "application/x-www-form-urlencoded"),
            ("accept", "application/json"),
        ],
        form,
        what,
    )
    .await?;
    token_json(&body, what)
}

/// A token answer: `access_token`, and `expires_on` (Unix seconds, managed identity, a string
/// or a number) or `expires_in` (seconds from now, OAuth).
///
/// ⚠️ A token with neither is refused: a token cached forever is a pod that stops working an
/// hour after start, every time, with nothing in the log until then.
fn token_json(body: &[u8], what: &str) -> Result<Expiring<String>, BlobError> {
    let v: serde_json::Value = serde_json::from_slice(body)
        .map_err(|e| BlobError::Other(format!("{what}: not JSON: {e}")))?;
    let token = v
        .get("access_token")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| BlobError::Other(format!("{what}: no access_token in the answer")))?;
    let seconds = |name: &str| {
        v.get(name).and_then(|f| {
            f.as_u64()
                .or_else(|| f.as_str().and_then(|s| s.trim().parse().ok()))
        })
    };
    let expires = match (seconds("expires_on"), seconds("expires_in")) {
        (Some(on), _) => on,
        (None, Some(within)) => creds::now() + within,
        (None, None) => {
            return Err(BlobError::Other(format!(
                "{what}: no expires_on or expires_in in the answer"
            )));
        }
    };
    Ok(Expiring {
        credentials: Arc::new(token.to_owned()),
        expires: Some(expires),
    })
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    reason = "assertions in tests are the reporting mechanism"
)]
mod tests {
    use super::*;

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
        let id = ("AZURE_CLIENT_ID", "cid");
        let tenant = ("AZURE_TENANT_ID", "tid");
        let secret = ("AZURE_CLIENT_SECRET", "s");
        let file = ("AZURE_FEDERATED_TOKEN_FILE", "/t");
        let msi = [
            ("IDENTITY_ENDPOINT", "http://e/t"),
            ("IDENTITY_HEADER", "h"),
        ];
        let all = [id, tenant, secret, file, msi[0], msi[1]];
        assert_eq!(
            Provider::from_vars(vars(&all)),
            Provider::ClientSecret {
                token_url: "https://login.microsoftonline.com/tid/oauth2/v2.0/token".to_owned(),
                client_id: "cid".to_owned(),
                secret: "s".to_owned(),
            }
        );
        let host = ("AZURE_AUTHORITY_HOST", "https://login.example/");
        assert_eq!(
            Provider::from_vars(vars(&[id, tenant, file, host, msi[0], msi[1]])),
            Provider::Workload {
                token_url: "https://login.example/tid/oauth2/v2.0/token".to_owned(),
                client_id: "cid".to_owned(),
                token_file: "/t".into(),
            }
        );
        // No tenant: neither OAuth provider, whatever else is set.
        assert_eq!(
            Provider::from_vars(vars(&[id, secret, file, msi[0], msi[1]])),
            Provider::IdentityEndpoint {
                endpoint: "http://e/t".to_owned(),
                header: "h".to_owned(),
                client_id: Some("cid".to_owned()),
            }
        );
        assert_eq!(
            Provider::from_vars(vars(&[msi[0], ("IDENTITY_HEADER", "")])),
            Provider::Imds {
                endpoint: "http://169.254.169.254".to_owned(),
                client_id: None,
            }
        );
        assert_eq!(
            Provider::from_vars(vars(&[("AZURE_IMDS_ENDPOINT", "http://127.0.0.1:9")])),
            Provider::Imds {
                endpoint: "http://127.0.0.1:9".to_owned(),
                client_id: None,
            }
        );
    }

    #[test]
    fn token_answers_name_their_expiry_or_are_refused() {
        let on = token_json(br#"{"access_token":"t","expires_on":"1795089600"}"#, "x").unwrap();
        assert_eq!(on.expires, Some(1_795_089_600));
        assert_eq!(*on.credentials, "t");
        let on = token_json(br#"{"access_token":"t","expires_on":1795089600}"#, "x").unwrap();
        assert_eq!(on.expires, Some(1_795_089_600));
        let before = creds::now();
        let within = token_json(br#"{"access_token":"t","expires_in":3599}"#, "x").unwrap();
        let at = within.expires.unwrap();
        assert!((before + 3599..=creds::now() + 3599).contains(&at), "{at}");
        assert!(token_json(br#"{"access_token":"t"}"#, "x").is_err());
        assert!(token_json(br#"{"expires_in":1}"#, "x").is_err());
        assert!(token_json(b"<html>", "x").is_err());
    }

    #[test]
    fn forms_and_identity_uris_are_encoded() {
        assert_eq!(
            &form(&[("a b", "c&d"), ("scope", SCOPE)])[..],
            b"a%20b=c%26d&scope=https%3A%2F%2Fstorage.azure.com%2F.default"
        );
        assert_eq!(
            msi_uri("http://e/t", "2019-08-01", Some("u/1")),
            "http://e/t?api-version=2019-08-01&resource=https%3A%2F%2Fstorage.azure.com&client_id=u%2F1"
        );
    }
}
