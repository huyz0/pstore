//! One index searched by several servers at once (M54).
//!
//! A server with peers sends each folded segment's dense and sparse legs to the server
//! [`pstore_engine::assign`] gives it, over `POST /v1/internal/part`, and merges what comes
//! back exactly as it merges its own segments. The per-segment work is
//! [`pstore_query::part`] on both sides, so the split answer is the single-server one.
//!
//! ⚠️ **Nothing here decides an answer.** A part that fails, for any reason, is run by the
//! coordinator itself: a peer costs rounds, never answers.

use crate::{Api, ApiError, ConfigError, predicate, tenant_of};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use pstore_blob::{BlobStore, Key};
use pstore_engine::{Part, Peers};
use pstore_query::{Hit, PartHits, Prefetch, Target};
use pstore_types::TenantId;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// What a part's sender and its peer must agree on, bit for bit: the wire, and how a share is
/// scanned, widened and cut. Bumped whenever either changes, so a rolling deploy never mixes
/// two builds' scoring -- a peer on another version refuses, and the coordinator runs the part.
pub(crate) const PROTOCOL: u32 = 1;

/// The most segments one part may name.
pub(crate) const MAX_SEGMENTS: usize = pstore_engine::MAX_PART_SEGMENTS;

/// How long a peer has to answer a part, unless `PSTORE_PEER_TIMEOUT_MS` says otherwise.
pub(crate) const DEFAULT_TIMEOUT: Duration = Duration::from_millis(2_000);

/// `PSTORE_PEERS` and `PSTORE_PEER_SELF`, read (M54).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerConfig {
    /// Every server's base URL, this one included, without a trailing `/`.
    pub servers: Vec<String>,
    /// This server's position in `servers`.
    pub me: usize,
    /// How long a peer has to answer a part.
    pub timeout: Duration,
}

impl PeerConfig {
    /// The peer list from the environment, or `None` for a single server.
    ///
    /// # Errors
    /// Either variable without the other, this server absent from the list, a URL named
    /// twice, or a timeout that is not a positive number of milliseconds.
    pub fn from_vars(get: impl Fn(&str) -> Option<String>) -> Result<Option<Self>, ConfigError> {
        let refuse = |var: &'static str, v: &str, why: &'static str| {
            ConfigError::Peers(var, v.to_owned(), why)
        };
        let timeout = match get("PSTORE_PEER_TIMEOUT_MS") {
            None => DEFAULT_TIMEOUT,
            Some(v) => match v.parse::<u64>() {
                Ok(ms) if ms > 0 => Duration::from_millis(ms),
                _ => {
                    return Err(refuse(
                        "PSTORE_PEER_TIMEOUT_MS",
                        &v,
                        "a positive number of milliseconds",
                    ));
                }
            },
        };
        let (list, me) = match (get("PSTORE_PEERS"), get("PSTORE_PEER_SELF")) {
            (None, None) => return Ok(None),
            (Some(list), Some(me)) => (list, me),
            (Some(list), None) => {
                return Err(refuse(
                    "PSTORE_PEERS",
                    &list,
                    "set PSTORE_PEER_SELF too: both or neither",
                ));
            }
            (None, Some(me)) => {
                return Err(refuse(
                    "PSTORE_PEER_SELF",
                    &me,
                    "set PSTORE_PEERS too: both or neither",
                ));
            }
        };
        let servers: Vec<String> = list
            .split(',')
            .map(|s| s.trim().trim_end_matches('/').to_owned())
            .filter(|s| !s.is_empty())
            .collect();
        // ⚠️ Plain HTTP only (code review): this build's client has no TLS, so an `https://`
        // peer would fail every part at run time, counted, instead of here.
        if let Some(bad) = servers.iter().find(|s| {
            s.strip_prefix("http://")
                .is_none_or(|rest| rest.is_empty() || rest.contains('/'))
        }) {
            return Err(refuse(
                "PSTORE_PEERS",
                bad,
                "each server is http://host:port: no TLS, no path",
            ));
        }
        let mut seen = std::collections::BTreeSet::new();
        if !servers.iter().all(|s| seen.insert(s.as_str())) {
            return Err(refuse("PSTORE_PEERS", &list, "a server named twice"));
        }
        let me_url = me.trim().trim_end_matches('/');
        let me = servers.iter().position(|s| s == me_url).ok_or_else(|| {
            refuse(
                "PSTORE_PEER_SELF",
                me_url,
                "not one of PSTORE_PEERS: every server must find itself in the list",
            )
        })?;
        Ok(Some(Self {
            servers,
            me,
            timeout,
        }))
    }
}

/// The peers, a client to reach them, and what `GET /metrics` reports of them.
#[derive(Debug)]
pub(crate) struct Cluster {
    config: PeerConfig,
    client: reqwest::Client,
    pub(crate) sent: AtomicU64,
    pub(crate) failed: AtomicU64,
}

impl Cluster {
    /// The client every part is sent with.
    ///
    /// ⚠️ **Refused, never defaulted** (code review): a default client has no timeout, and a
    /// hung peer would then hang the query -- rule 5 rests on the timeout. And no proxy: peer
    /// traffic stays on the deployment's network whatever `HTTP_PROXY` says for blob egress.
    pub(crate) fn new(config: PeerConfig) -> Result<Self, ConfigError> {
        let client = reqwest::Client::builder()
            .timeout(config.timeout)
            .no_proxy()
            .build()
            .map_err(|e| {
                ConfigError::Peers("PSTORE_PEERS", e.to_string(), "no HTTP client for peers")
            })?;
        Ok(Self {
            client,
            config,
            sent: AtomicU64::new(0),
            failed: AtomicU64::new(0),
        })
    }
}

/// One query's view of the peers: the cluster, and what a part carries that the engine does
/// not -- the tenant, and the filter as the client wrote it.
pub(crate) struct QueryPeers {
    pub(crate) cluster: Arc<Cluster>,
    pub(crate) tenant: TenantId,
    pub(crate) filters: Option<serde_json::Value>,
}

impl Peers for QueryPeers {
    fn servers(&self) -> &[String] {
        &self.cluster.config.servers
    }

    fn me(&self) -> usize {
        self.cluster.config.me
    }

    fn part(
        &self,
        server: usize,
        part: Part,
    ) -> futures_util::future::BoxFuture<'static, Result<PartHits, String>> {
        let cluster = Arc::clone(&self.cluster);
        let url = cluster
            .config
            .servers
            .get(server)
            .map(|s| format!("{s}/v1/internal/part"));
        let body = serde_json::to_vec(&WirePart::of(&part, self.filters.clone()));
        let tenant = self.tenant;
        Box::pin(async move {
            cluster.sent.fetch_add(1, Ordering::Relaxed);
            let got = send(&cluster, url, body, tenant, &part).await;
            if let Err(Failure::Counted(_)) = &got {
                cluster.failed.fetch_add(1, Ordering::Relaxed);
            }
            got.map_err(|f| match f {
                Failure::Counted(m) | Failure::Query(m) => m,
            })
        })
    }
}

/// Why a part came back with no hits.
enum Failure {
    /// The peer, the network or the protocol: counted in `pstore_peer_parts_failed`.
    Counted(String),
    /// The query itself (`422`): the client's error, raised again when the coordinator runs
    /// the part, and not a peer's failure.
    Query(String),
}

async fn send(
    cluster: &Cluster,
    url: Option<String>,
    body: Result<Vec<u8>, serde_json::Error>,
    tenant: TenantId,
    part: &Part,
) -> Result<PartHits, Failure> {
    let url = url.ok_or_else(|| Failure::Counted("no such server".to_owned()))?;
    let body = body.map_err(|e| Failure::Counted(e.to_string()))?;
    let res = cluster
        .client
        .post(url)
        .header("x-pstore-tenant", tenant.0.to_string())
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await
        .map_err(|e| Failure::Counted(e.to_string()))?;
    let status = res.status().as_u16();
    let bytes = res
        .bytes()
        .await
        .map_err(|e| Failure::Counted(e.to_string()))?;
    match status {
        200 => {
            let wire: WireHits =
                serde_json::from_slice(&bytes).map_err(|e| Failure::Counted(e.to_string()))?;
            wire.hits(part).ok_or_else(|| {
                Failure::Counted("a part answered for a segment or leg it was not given".into())
            })
        }
        422 => Err(Failure::Query(String::from_utf8_lossy(&bytes).into_owned())),
        s => Err(Failure::Counted(format!(
            "{s}: {}",
            String::from_utf8_lossy(&bytes)
        ))),
    }
}

/// A part on the wire. Floats travel as their bits: the coordinator merges a peer's scores
/// with its own and breaks ties on them, so a decimal round trip that moved one would move
/// the answer.
#[derive(Debug, Serialize, Deserialize)]
struct WirePart {
    protocol: u32,
    index: String,
    fts: String,
    filters: Option<serde_json::Value>,
    shadow: usize,
    legs: Vec<WireLeg>,
    targets: Vec<WireTarget>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum WireLeg {
    Dense {
        j: usize,
        field: String,
        query: Vec<u32>,
        limit: usize,
        k: usize,
        p: usize,
        oversample: usize,
        rerank: String,
        exact: bool,
    },
    Sparse {
        j: usize,
        field: String,
        query: Vec<(u32, u32)>,
        limit: usize,
    },
}

#[derive(Debug, Serialize, Deserialize)]
struct WireTarget {
    i: usize,
    segment: String,
    segment_len: Option<u64>,
    centroids: bool,
    deleted: Option<String>,
    sparse_dict: bool,
    text_dict: bool,
    shadowed: bool,
}

/// One `(segment, leg)` pair's hits on the wire: `(segment, leg, [(row, score bits)])`.
type WireShare = (usize, usize, Vec<(usize, u32)>);

/// A part's hits.
#[derive(Debug, Serialize, Deserialize)]
struct WireHits {
    hits: Vec<WireShare>,
}

fn rerank_name(r: pstore_index::vec_index::Rerank) -> &'static str {
    match r {
        pstore_index::vec_index::Rerank::None => "none",
        pstore_index::vec_index::Rerank::Fast => "fast",
        pstore_index::vec_index::Rerank::Exact => "exact",
    }
}

fn rerank_of(name: &str) -> Option<pstore_index::vec_index::Rerank> {
    match name {
        "none" => Some(pstore_index::vec_index::Rerank::None),
        "fast" => Some(pstore_index::vec_index::Rerank::Fast),
        "exact" => Some(pstore_index::vec_index::Rerank::Exact),
        _ => None,
    }
}

impl WirePart {
    fn of(part: &Part, filters: Option<serde_json::Value>) -> Self {
        Self {
            protocol: PROTOCOL,
            index: part.index.clone(),
            fts: part.fts.encode(),
            filters,
            shadow: part.shadow,
            legs: part
                .legs
                .iter()
                .filter_map(|(j, p)| match p {
                    Prefetch::Dense {
                        field,
                        query,
                        limit,
                        tune,
                    } => Some(WireLeg::Dense {
                        j: *j,
                        field: field.clone(),
                        query: query.iter().map(|x| x.to_bits()).collect(),
                        limit: *limit,
                        k: tune.k,
                        p: tune.p,
                        oversample: tune.oversample,
                        rerank: rerank_name(tune.rerank).to_owned(),
                        exact: tune.exact,
                    }),
                    Prefetch::Sparse {
                        field,
                        query,
                        limit,
                    } => Some(WireLeg::Sparse {
                        j: *j,
                        field: field.clone(),
                        query: query.iter().map(|(d, x)| (*d, x.to_bits())).collect(),
                        limit: *limit,
                    }),
                    Prefetch::Text { .. } | Prefetch::Trigram { .. } => None,
                })
                .collect(),
            targets: part
                .targets
                .iter()
                .map(|(i, t)| WireTarget {
                    i: *i,
                    segment: t.segment.as_str().to_owned(),
                    segment_len: t.segment_len,
                    centroids: t.centroids.is_some(),
                    deleted: t.deleted.as_ref().map(|k| k.as_str().to_owned()),
                    sparse_dict: t.sparse_dict,
                    text_dict: t.text_dict,
                    shadowed: t.shadowed,
                })
                .collect(),
        }
    }
}

impl WireHits {
    fn of(hits: &PartHits) -> Self {
        Self {
            hits: hits
                .iter()
                .map(|(i, j, hs)| {
                    (
                        *i,
                        *j,
                        hs.iter().map(|h| (h.row, h.score.to_bits())).collect(),
                    )
                })
                .collect(),
        }
    }

    /// The hits, or `None` when a peer answered for a segment or a leg its part did not name,
    /// or for one pair twice.
    fn hits(self, part: &Part) -> Option<PartHits> {
        let segments: std::collections::BTreeSet<usize> =
            part.targets.iter().map(|(i, _)| *i).collect();
        let legs: std::collections::BTreeSet<usize> = part.legs.iter().map(|(j, _)| *j).collect();
        let mut seen = std::collections::BTreeSet::new();
        self.hits
            .into_iter()
            .map(|(i, j, hs)| {
                (segments.contains(&i) && legs.contains(&j) && seen.insert((i, j))).then(|| {
                    (
                        i,
                        j,
                        hs.into_iter()
                            .map(|(row, bits)| Hit {
                                segment: i,
                                row,
                                score: f32::from_bits(bits),
                            })
                            .collect(),
                    )
                })
            })
            .collect()
    }
}

/// Where a tenant's index objects live: every key a part may name begins with it. The same
/// string the engine writes segments under.
fn index_prefix(tenant: TenantId) -> String {
    format!("{:04x}/tnt/{}/idx/", tenant.0 as u16, tenant.0)
}

/// Why a part is refused before any read (rule 6).
fn guard(tenant: TenantId, wire: &WirePart) -> Result<(), ApiError> {
    let refuse = |m: String| ApiError::new(StatusCode::BAD_REQUEST, "bad_part", m);
    if wire.legs.len() > pstore_query::MAX_LEGS {
        return Err(refuse(format!(
            "at most {} legs a part",
            pstore_query::MAX_LEGS
        )));
    }
    if wire.targets.len() > MAX_SEGMENTS {
        return Err(refuse(format!("at most {MAX_SEGMENTS} segments a part")));
    }
    let prefix = index_prefix(tenant);
    // ⚠️ A `..` path COMPONENT, not substring (code review): an index may be named `a..b`.
    let ours = |k: &str| k.starts_with(&prefix) && !k.split('/').any(|c| c == "..");
    for t in &wire.targets {
        if !ours(&t.segment) || !t.segment.ends_with(".seg") {
            return Err(refuse(format!(
                "{} is not one of this tenant's segments",
                t.segment
            )));
        }
        if let Some(dv) = &t.deleted
            && (!ours(dv) || !dv.starts_with(&t.segment) || !dv.ends_with(".dv"))
        {
            return Err(refuse(format!(
                "{dv} is not a delete vector of {}",
                t.segment
            )));
        }
    }
    Ok(())
}

/// The part a wire part describes, or why it cannot be one.
fn decode(wire: WirePart) -> Result<(Part, Option<serde_json::Value>), ApiError> {
    let refuse = |m: &str| ApiError::new(StatusCode::BAD_REQUEST, "bad_part", m.to_owned());
    let fts = pstore_format::text::FullText::decode(&wire.fts)
        .ok_or_else(|| refuse("the full-text schema did not decode"))?;
    let legs = wire
        .legs
        .into_iter()
        .map(|l| match l {
            WireLeg::Dense {
                j,
                field,
                query,
                limit,
                k,
                p,
                oversample,
                rerank,
                exact,
            } => Ok((
                j,
                Prefetch::Dense {
                    field,
                    query: query.into_iter().map(f32::from_bits).collect(),
                    limit,
                    tune: pstore_index::vec_index::Query {
                        k,
                        p,
                        oversample,
                        rerank: rerank_of(&rerank).ok_or_else(|| refuse("no such rerank"))?,
                        exact,
                    },
                },
            )),
            WireLeg::Sparse {
                j,
                field,
                query,
                limit,
            } => Ok((
                j,
                Prefetch::Sparse {
                    field,
                    query: query
                        .into_iter()
                        .map(|(d, x)| (d, f32::from_bits(x)))
                        .collect(),
                    limit,
                },
            )),
        })
        .collect::<Result<Vec<_>, ApiError>>()?;
    let targets = wire
        .targets
        .into_iter()
        .map(|t| {
            let segment = Key::new(t.segment);
            (
                t.i,
                Target {
                    centroids: t
                        .centroids
                        .then(|| pstore_index::vec_index::centroid_key(&segment)),
                    segment_len: t.segment_len,
                    deleted: t.deleted.map(Key::new),
                    sparse_dict: t.sparse_dict,
                    text_dict: t.text_dict,
                    shadowed: t.shadowed,
                    segment,
                },
            )
        })
        .collect();
    Ok((
        Part {
            index: wire.index,
            fts,
            legs,
            targets,
            shadow: wire.shadow,
        },
        wire.filters,
    ))
}

/// `POST /v1/internal/part`: runs one share of another server's query (M54).
pub(crate) async fn serve_part<S: BlobStore + 'static>(
    State(api): State<Arc<Api<S>>>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<Response, ApiError> {
    let tenant = tenant_of(&headers)?;
    let wire: WirePart = serde_json::from_slice(&body)
        .map_err(|e| ApiError::bad_request(format!("malformed part: {e}")))?;
    if wire.protocol != PROTOCOL {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "protocol_mismatch",
            format!(
                "this server speaks part protocol {PROTOCOL}, not {}",
                wire.protocol
            ),
        ));
    }
    guard(tenant, &wire)?;
    let (part, filters) = decode(wire)?;
    let filter = filters.as_ref().map(predicate).transpose()?;
    let engine = api.engine(tenant).await;
    api.parts_served.fetch_add(1, Ordering::Relaxed);
    match engine.part(&part, filter.as_ref()).await {
        Ok(hits) => Ok(axum::Json(WireHits::of(&hits)).into_response()),
        Err(
            e @ (pstore_query::QueryError::Unimplemented(_)
            | pstore_query::QueryError::Format(
                pstore_format::FormatError::UnknownField
                | pstore_format::FormatError::DimensionMismatch { .. },
            )),
        ) => Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "bad_query",
            e.to_string(),
        )),
        Err(e) => Err(ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            e.to_string(),
        )),
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    reason = "assertions in tests are the reporting mechanism"
)]
mod tests {
    use super::*;

    #[test]
    fn a_part_survives_the_wire_bit_for_bit() {
        let segment = Key::new("0007/tnt/7/idx/docs/seg/L0/1.seg".to_owned());
        let part = Part {
            index: "docs".to_owned(),
            fts: pstore_format::text::FullText::default(),
            legs: vec![
                (
                    0,
                    Prefetch::Dense {
                        field: "vector".to_owned(),
                        query: vec![0.1, -0.0, f32::MIN_POSITIVE, 1.0 / 3.0],
                        limit: 10,
                        tune: pstore_index::vec_index::Query {
                            k: 7,
                            p: 3,
                            oversample: 5,
                            rerank: pstore_index::vec_index::Rerank::Exact,
                            exact: true,
                        },
                    },
                ),
                (
                    2,
                    Prefetch::Sparse {
                        field: "sparse".to_owned(),
                        query: vec![(4, 0.7), (9, -2.5)],
                        limit: 4,
                    },
                ),
            ],
            targets: vec![(
                3,
                Target {
                    centroids: Some(pstore_index::vec_index::centroid_key(&segment)),
                    segment_len: Some(99),
                    deleted: Some(Key::new(format!("{}.1-2.dv", segment.as_str()))),
                    sparse_dict: false,
                    text_dict: true,
                    shadowed: true,
                    segment,
                },
            )],
            shadow: 6,
        };
        let filters = Some(serde_json::json!(["k", "Eq", 1]));
        let raw = serde_json::to_vec(&WirePart::of(&part, filters.clone())).unwrap();
        let wire: WirePart = serde_json::from_slice(&raw).unwrap();
        guard(TenantId(7), &wire).unwrap();
        let (back, f) = decode(wire).unwrap();
        assert_eq!(f, filters);
        assert_eq!(back.index, part.index);
        assert_eq!(back.shadow, 6);
        assert_eq!(format!("{:?}", back.legs), format!("{:?}", part.legs));
        assert_eq!(format!("{:?}", back.targets), format!("{:?}", part.targets));
        // Every rerank by name, both ways.
        for r in [
            pstore_index::vec_index::Rerank::None,
            pstore_index::vec_index::Rerank::Fast,
            pstore_index::vec_index::Rerank::Exact,
        ] {
            assert_eq!(
                format!("{:?}", rerank_of(rerank_name(r))),
                format!("{:?}", Some(r))
            );
        }
        assert!(rerank_of("best").is_none());
        // -0.0 and 1/3 by their bits, not their decimal.
        let Prefetch::Dense { query, .. } = &back.legs[0].1 else {
            panic!("dense")
        };
        assert_eq!((-0.0f32).to_bits(), query[1].to_bits());

        let hits: PartHits = vec![(
            3,
            2,
            vec![Hit {
                segment: 3,
                row: 8,
                score: 1.0 / 7.0,
            }],
        )];
        let back =
            serde_json::from_slice::<WireHits>(&serde_json::to_vec(&WireHits::of(&hits)).unwrap())
                .unwrap()
                .hits(&part)
                .unwrap();
        assert_eq!(format!("{back:?}"), format!("{hits:?}"));
        // A hit for a segment or leg the part did not name, or a pair twice, refuses the
        // whole answer.
        for stray in [vec![(4, 2)], vec![(3, 1)], vec![(3, 2), (3, 2)]] {
            let wire = WireHits {
                hits: stray.iter().map(|(i, j)| (*i, *j, vec![])).collect(),
            };
            assert!(wire.hits(&part).is_none(), "{stray:?}");
        }
    }

    #[test]
    fn a_part_is_bounded_at_its_limits_exactly() {
        let target = |i: usize| WireTarget {
            i,
            segment: format!("0007/tnt/7/idx/docs/seg/L0/{i}.seg"),
            segment_len: None,
            centroids: false,
            deleted: None,
            sparse_dict: false,
            text_dict: false,
            shadowed: false,
        };
        let leg = |j: usize| WireLeg::Sparse {
            j,
            field: "s".to_owned(),
            query: vec![],
            limit: 1,
        };
        let part = |legs: usize, targets: usize| WirePart {
            protocol: PROTOCOL,
            index: "docs".to_owned(),
            fts: String::new(),
            filters: None,
            shadow: 0,
            legs: (0..legs).map(leg).collect(),
            targets: (0..targets).map(target).collect(),
        };
        let t = TenantId(7);
        assert!(guard(t, &part(pstore_query::MAX_LEGS, 1)).is_ok());
        assert!(guard(t, &part(pstore_query::MAX_LEGS + 1, 1)).is_err());
        assert!(guard(t, &part(1, MAX_SEGMENTS)).is_ok());
        assert!(guard(t, &part(1, MAX_SEGMENTS + 1)).is_err());
    }
}
