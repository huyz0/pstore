//! One index searched by several servers at once (M54).
//!
//! A server with peers sends each folded segment's dense and sparse legs to the server
//! [`pstore_engine::assign`] gives it, over `POST /v1/internal/part`, and merges what comes
//! back exactly as it merges its own segments. The per-segment work is
//! [`pstore_query::part`] on both sides, so the split answer is the single-server one.
//!
//! M55: a query with a text leg is split in two exchanges per part -- `open`, which returns
//! the share's BM25 statistics and holds its opened segments under an id, and `scan`, which
//! scores them against the sum the coordinator adds up from every share's.
//!
//! ⚠️ **Nothing here decides an answer.** A part that fails, for any reason, is run by the
//! coordinator itself: a peer costs rounds, never answers.

use crate::{Api, ApiError, ConfigError, predicate, tenant_of};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use pstore_blob::{BlobStore, Key};
use pstore_engine::{Part, Peers, Phased};
use pstore_index::text::Stats;
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

/// The two-phase exchange of a query with a text leg (M55). A peer of M54's build refuses it
/// with `409`, so a rolling deploy runs those shares on the coordinator; vector-only parts
/// stay at [`PROTOCOL`], which both builds speak.
pub(crate) const PHASED: u32 = 2;

/// The two-phase exchange of a `sum` query (M58): a part that runs its legs whole and is cut
/// by sum. A peer of M55's build would cut each leg at its limit, so it must refuse this, and
/// does: it speaks 1 and 2 only.
pub(crate) const SUMMED: u32 = 3;

/// How long a part is held between its phases (M55).
pub(crate) const HOLD_FOR: Duration = Duration::from_secs(10);
/// The most parts held at once (M55).
pub(crate) const HOLD_PARTS: usize = 256;
/// The most opened bytes held at once, by [`pstore_query::OpenPart::bytes`]'s estimate (M55).
pub(crate) const HOLD_BYTES: usize = 256 * 1024 * 1024;

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
        let timeout = timeout_of(&get)?;
        let (list, me) = match (get("PSTORE_PEERS"), get("PSTORE_PEER_SELF")) {
            (None, None) => return Ok(None),
            // M56: a server that gossips names only itself; `GossipConfig` reads the rest.
            (None, Some(_)) if get("PSTORE_PEER_GOSSIP_ADDR").is_some() => return Ok(None),
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
        if let Some(bad) = servers.iter().find(|s| peer_url(s).is_none()) {
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

/// `PSTORE_PEER_TIMEOUT_MS`, or the default.
fn timeout_of(get: &impl Fn(&str) -> Option<String>) -> Result<Duration, ConfigError> {
    match get("PSTORE_PEER_TIMEOUT_MS") {
        None => Ok(DEFAULT_TIMEOUT),
        Some(v) => match v.parse::<u64>() {
            Ok(ms) if ms > 0 => Ok(Duration::from_millis(ms)),
            _ => Err(ConfigError::Peers(
                "PSTORE_PEER_TIMEOUT_MS",
                v,
                "a positive number of milliseconds",
            )),
        },
    }
}

/// `s` as a server's URL, `http://host:port` without a trailing `/`; `None` for anything else
/// (M54's rule, and M56's filter on a member's zone).
fn peer_url(s: &str) -> Option<String> {
    let s = s.trim().trim_end_matches('/');
    let rest = s.strip_prefix("http://")?;
    (!rest.is_empty() && !rest.contains('/')).then(|| s.to_owned())
}

/// A server's peers from a SWIM membership of its own (M56): `PSTORE_PEER_SELF` and the
/// `PSTORE_PEER_GOSSIP_*` variables, read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GossipConfig {
    /// This server's URL, which its member declares as its zone.
    pub url: String,
    /// The UDP address to listen on, normalised.
    pub listen: String,
    /// The UDP address to advertise, normalised: an IP, never unspecified, never port 0.
    pub advertise: String,
    /// Members to join first, normalised.
    pub seeds: Vec<String>,
    /// The SWIM period.
    pub period: Duration,
    /// The cluster keys (M53), or `None` with `PSTORE_GOSSIP_INSECURE=1`.
    pub keys: Option<Vec<Vec<u8>>>,
    /// How long a peer has to answer a part, as M54's.
    pub timeout: Duration,
}

impl GossipConfig {
    /// The gossip configuration from the environment, or `None` when there is none.
    ///
    /// # Errors
    /// Each refusal of M56's rule 1: beside `PSTORE_PEERS`, without `PSTORE_PEER_SELF`, a
    /// gossip variable without `PSTORE_PEER_GOSSIP_ADDR`, an address that is not an IP literal
    /// with a port, an unroutable advertise address, a bad period, and the key's refusals.
    pub fn from_vars(get: impl Fn(&str) -> Option<String>) -> Result<Option<Self>, ConfigError> {
        let refuse = |var: &'static str, v: &str, why: &'static str| {
            ConfigError::Peers(var, v.to_owned(), why)
        };
        let Some(listen) = get("PSTORE_PEER_GOSSIP_ADDR") else {
            for var in [
                "PSTORE_PEER_GOSSIP_ADVERTISE",
                "PSTORE_PEER_GOSSIP_SEEDS",
                "PSTORE_PEER_GOSSIP_PERIOD_MS",
            ] {
                if let Some(v) = get(var) {
                    return Err(refuse(var, &v, "set PSTORE_PEER_GOSSIP_ADDR too"));
                }
            }
            return Ok(None);
        };
        if let Some(list) = get("PSTORE_PEERS") {
            return Err(refuse(
                "PSTORE_PEERS",
                &list,
                "a static list or gossip, never both",
            ));
        }
        let url = get("PSTORE_PEER_SELF").ok_or_else(|| {
            refuse(
                "PSTORE_PEER_GOSSIP_ADDR",
                &listen,
                "set PSTORE_PEER_SELF too: the URL this server's member declares",
            )
        })?;
        let url = peer_url(&url).ok_or_else(|| {
            refuse(
                "PSTORE_PEER_SELF",
                &url,
                "this server is http://host:port: no TLS, no path",
            )
        })?;
        // ⚠️ IP literals only (spec review): a member first heard by probe is held at its
        // source address, so a name and its IP would be two members. Re-displayed, so two
        // spellings of one IP are one.
        let addr = |var: &'static str, v: &str| {
            v.trim()
                .parse::<std::net::SocketAddr>()
                .map_err(|_| refuse(var, v, "an IP address and a port, never a hostname"))
        };
        let listen_at = addr("PSTORE_PEER_GOSSIP_ADDR", &listen)?;
        let (advertise_var, advertise_at) = match get("PSTORE_PEER_GOSSIP_ADVERTISE") {
            Some(v) => (
                "PSTORE_PEER_GOSSIP_ADVERTISE",
                addr("PSTORE_PEER_GOSSIP_ADVERTISE", &v)?,
            ),
            // Code review: name the variable the operator set, not one they never did.
            None => (
                "PSTORE_PEER_GOSSIP_ADDR (advertised, as PSTORE_PEER_GOSSIP_ADVERTISE is unset)",
                listen_at,
            ),
        };
        if advertise_at.ip().is_unspecified() || advertise_at.port() == 0 {
            return Err(refuse(
                advertise_var,
                &advertise_at.to_string(),
                "unroutable, and every server would derive the same identity from it: set an \
                 address the others can reach",
            ));
        }
        let seeds = match get("PSTORE_PEER_GOSSIP_SEEDS") {
            None => Vec::new(),
            Some(v) => v
                .split(',')
                .filter(|s| !s.trim().is_empty())
                .map(|s| addr("PSTORE_PEER_GOSSIP_SEEDS", s).map(|a| a.to_string()))
                .collect::<Result<_, _>>()?,
        };
        let period = match get("PSTORE_PEER_GOSSIP_PERIOD_MS") {
            None => Duration::from_millis(1_000),
            Some(v) => match v.parse::<u64>() {
                Ok(ms) if ms > 0 => Duration::from_millis(ms),
                _ => {
                    return Err(refuse(
                        "PSTORE_PEER_GOSSIP_PERIOD_MS",
                        &v,
                        "a positive number of milliseconds",
                    ));
                }
            },
        };
        // The key, read by `pstore-node`'s own functions, with their refusals (M53).
        let key = pstore_node::schedule::gossip_key_source(
            get("PSTORE_GOSSIP_KEY").as_deref(),
            get("PSTORE_GOSSIP_KEY_FILE").as_deref(),
        )
        .map_err(|e| ConfigError::Peers("PSTORE_GOSSIP_KEY", e, "the gossip key"))?;
        let keys = pstore_node::schedule::gossip_keys(
            key.as_deref(),
            get("PSTORE_GOSSIP_INSECURE").as_deref(),
            false,
        )
        .map_err(|e| ConfigError::Peers("PSTORE_GOSSIP_KEY", e, "the gossip key"))?;
        Ok(Some(Self {
            url,
            listen: listen_at.to_string(),
            advertise: advertise_at.to_string(),
            seeds,
            period,
            keys,
            timeout: timeout_of(&get)?,
        }))
    }
}

/// The peer list a view gives (M56): every member whose zone is a server's URL, and `me`;
/// deduplicated and sorted; with `me`'s position. A member with another zone -- a
/// `pstore-node` on the same key, or one whose zone gossip has not filled yet -- is no server.
#[must_use]
pub fn peer_list_of(members: &[(String, String)], me: &str) -> (Vec<String>, usize) {
    let me = peer_url(me).unwrap_or_else(|| me.to_owned());
    let mut list: Vec<String> = members
        .iter()
        .filter_map(|(_, zone)| peer_url(zone))
        .chain(std::iter::once(me.clone()))
        .collect();
    list.sort();
    list.dedup();
    let at = list.iter().position(|s| *s == me).unwrap_or(0);
    (list, at)
}

/// The servers a query may split across, and which one is this (M56: swapped whole, so a
/// query reads one list however the view moves).
#[derive(Debug)]
pub(crate) struct List {
    pub(crate) servers: Vec<String>,
    pub(crate) me: usize,
}

/// A server's own SWIM member and the task that refreshes its peer list from it (M56).
pub struct PeerGossip {
    member: Arc<pstore_node::swim::Member>,
    refresh: tokio::task::JoinHandle<()>,
}

impl PeerGossip {
    /// Starts the member, declaring `config.url` as its zone, and the refresh, once a period.
    pub(crate) async fn start(
        config: &GossipConfig,
        cluster: Arc<Cluster>,
    ) -> Result<Self, ConfigError> {
        let keys = config
            .keys
            .as_deref()
            .and_then(pstore_node::seal::Keys::new);
        let member = Arc::new(
            pstore_node::swim::start_with(
                &config.listen,
                &config.advertise,
                &config.url,
                &config.seeds,
                0.0,
                config.period,
                keys,
            )
            .await
            .map_err(|e| {
                ConfigError::Peers(
                    "PSTORE_PEER_GOSSIP_ADDR",
                    format!("{}: {e}", config.listen),
                    "the gossip member did not start",
                )
            })?,
        );
        let refresh = {
            let (member, url, period) = (Arc::clone(&member), config.url.clone(), config.period);
            tokio::spawn(async move {
                loop {
                    let view = member.members_zoned().await;
                    let (servers, me) = peer_list_of(&view, &url);
                    cluster.set_list(List { servers, me });
                    tokio::time::sleep(period).await;
                }
            })
        };
        Ok(Self { member, refresh })
    }

    /// Stops the refresh and the member; the member's port is free when this returns. The
    /// others declare this server dead by timeout: there is no leave message.
    pub async fn stop(&self) {
        self.refresh.abort();
        self.member.stop().await;
    }
}

impl Drop for PeerGossip {
    fn drop(&mut self) {
        // The refresh holds the member: once it is gone, so is the member, and with it its
        // tasks (M56.1).
        self.refresh.abort();
    }
}

impl std::fmt::Debug for PeerGossip {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PeerGossip")
            .field("member", &self.member.self_addr())
            .finish_non_exhaustive()
    }
}

/// The peers, a client to reach them, and what `GET /metrics` reports of them.
#[derive(Debug)]
pub(crate) struct Cluster {
    /// Swapped by the membership refresh (M56); a static list never moves.
    list: std::sync::RwLock<Arc<List>>,
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
            list: std::sync::RwLock::new(Arc::new(List {
                servers: config.servers,
                me: config.me,
            })),
            sent: AtomicU64::new(0),
            failed: AtomicU64::new(0),
        })
    }

    /// The list now: one snapshot, for one query.
    pub(crate) fn list(&self) -> Arc<List> {
        Arc::clone(
            &self
                .list
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
    }

    /// Replaces the list (M56). The client and the counters stay.
    pub(crate) fn set_list(&self, list: List) {
        *self
            .list
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Arc::new(list);
    }
}

/// One query's view of the peers: the cluster, and what a part carries that the engine does
/// not -- the tenant, and the filter as the client wrote it.
pub(crate) struct QueryPeers {
    pub(crate) cluster: Arc<Cluster>,
    /// The list as it was when this query began (M56).
    pub(crate) list: Arc<List>,
    pub(crate) tenant: TenantId,
    pub(crate) filters: Option<serde_json::Value>,
}

impl Peers for QueryPeers {
    fn servers(&self) -> &[String] {
        &self.list.servers
    }

    fn me(&self) -> usize {
        self.list.me
    }

    fn part(
        &self,
        server: usize,
        part: Part,
    ) -> futures_util::future::BoxFuture<'static, Result<PartHits, String>> {
        let cluster = Arc::clone(&self.cluster);
        let url = self
            .list
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

    fn phased(&self, server: usize, part: Part) -> Phased {
        let cluster = Arc::clone(&self.cluster);
        let base = self.list.servers.get(server).cloned();
        let url = base.as_ref().map(|s| format!("{s}/v1/internal/part"));
        let body = serde_json::to_vec(&WirePart::opening(&part, self.filters.clone()));
        let tenant = self.tenant;
        let index = part.index.clone();
        let sum_part = part.sum.is_some();
        // The id phase 1 returns, for phase 2; and whether this part's failure is counted yet:
        // once per part, whichever phase fails (spec rule 11).
        let held: Arc<std::sync::Mutex<Option<String>>> = Arc::default();
        let counted = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let fail = {
            let (cluster, counted) = (Arc::clone(&cluster), Arc::clone(&counted));
            move |f: &Failure| {
                if matches!(f, Failure::Counted(_)) && !counted.swap(true, Ordering::Relaxed) {
                    cluster.failed.fetch_add(1, Ordering::Relaxed);
                }
            }
        };
        let stats = {
            let (cluster, held, fail, url) = (
                Arc::clone(&cluster),
                Arc::clone(&held),
                fail.clone(),
                url.clone(),
            );
            Box::pin(async move {
                cluster.sent.fetch_add(1, Ordering::Relaxed);
                let got = open(&cluster, url, body, tenant).await;
                match got {
                    Ok((id, stats)) => {
                        *held
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(id);
                        Ok(stats)
                    }
                    Err(f) => {
                        fail(&f);
                        Err(f.message())
                    }
                }
            }) as futures_util::future::BoxFuture<'static, _>
        };
        let scan = Box::new(move |sum: Stats| {
            Box::pin(async move {
                let id = held
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .take();
                let got = match id {
                    Some(id) => {
                        let body = serde_json::to_vec(&WireScan {
                            protocol: if sum_part { SUMMED } else { PHASED },
                            phase: "scan".to_owned(),
                            id,
                            index,
                            stats: WireStats::of(&sum),
                        });
                        send(&cluster, url, body, tenant, &part).await
                    }
                    None => Err(Failure::Counted("no part was held".to_owned())),
                };
                got.map_err(|f| {
                    fail(&f);
                    f.message()
                })
            }) as futures_util::future::BoxFuture<'static, _>
        });
        Phased { stats, scan }
    }
}

impl Failure {
    fn message(self) -> String {
        match self {
            Self::Counted(m) | Self::Query(m) => m,
        }
    }
}

/// Phase 1 of a part (M55): the id it is held under, and the share's statistics.
async fn open(
    cluster: &Cluster,
    url: Option<String>,
    body: Result<Vec<u8>, serde_json::Error>,
    tenant: TenantId,
) -> Result<(String, Stats), Failure> {
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
            let wire: WireOpened =
                serde_json::from_slice(&bytes).map_err(|e| Failure::Counted(e.to_string()))?;
            Ok((wire.id, wire.stats.stats()))
        }
        422 => Err(Failure::Query(String::from_utf8_lossy(&bytes).into_owned())),
        s => Err(Failure::Counted(format!(
            "{s}: {}",
            String::from_utf8_lossy(&bytes)
        ))),
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
    /// `"open"` for phase 1 of a phased part (M55); absent for a part in one exchange.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    phase: Option<String>,
    index: String,
    fts: String,
    filters: Option<serde_json::Value>,
    shadow: usize,
    legs: Vec<WireLeg>,
    targets: Vec<WireTarget>,
    /// The text legs' analysed terms (M55).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    terms: Vec<String>,
    /// A `sum` part's cut (M58), protocol 3 only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sum: Option<WireSum>,
}

/// A `sum` part's cut on the wire (M58): every weight as its bits, and how many rows to keep.
#[derive(Debug, Serialize, Deserialize)]
struct WireSum {
    weights: Vec<u32>,
    keep: usize,
}

impl WireSum {
    fn of(cut: pstore_query::SumCut) -> Self {
        Self {
            weights: cut.weights.all().iter().map(|w| w.to_bits()).collect(),
            keep: cut.keep,
        }
    }

    fn cut(&self) -> Option<pstore_query::SumCut> {
        let weights: Vec<f32> = self.weights.iter().map(|b| f32::from_bits(*b)).collect();
        Some(pstore_query::SumCut {
            weights: pstore_query::Weights::of(&weights)?,
            keep: self.keep,
        })
    }
}

/// Phase 2 of a phased part (M55).
#[derive(Debug, Serialize, Deserialize)]
struct WireScan {
    protocol: u32,
    phase: String,
    id: String,
    index: String,
    stats: WireStats,
}

/// BM25's statistics on the wire (M55): counts are integers, so they travel as they are.
#[derive(Debug, Serialize, Deserialize)]
struct WireStats {
    doc_count: u64,
    total_tokens: u64,
    df: Vec<(String, u32)>,
}

impl WireStats {
    fn of(s: &Stats) -> Self {
        Self {
            doc_count: s.doc_count,
            total_tokens: s.total_tokens,
            df: s.df.iter().map(|(t, n)| (t.clone(), *n)).collect(),
        }
    }

    fn stats(self) -> Stats {
        Stats {
            doc_count: self.doc_count,
            total_tokens: self.total_tokens,
            df: self.df.into_iter().collect(),
        }
    }
}

/// What phase 1 answers (M55).
#[derive(Debug, Serialize, Deserialize)]
struct WireOpened {
    id: String,
    stats: WireStats,
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
    /// M55, phased parts only.
    Text {
        j: usize,
        field: String,
        query: String,
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
    /// Phase 1 of a phased part (M55): every leg, and the text legs' terms.
    fn opening(part: &Part, filters: Option<serde_json::Value>) -> Self {
        Self {
            protocol: if part.sum.is_some() { SUMMED } else { PHASED },
            phase: Some("open".to_owned()),
            terms: part.terms.clone(),
            sum: part.sum.map(WireSum::of),
            ..Self::of(part, filters)
        }
    }

    fn of(part: &Part, filters: Option<serde_json::Value>) -> Self {
        Self {
            protocol: PROTOCOL,
            phase: None,
            terms: Vec::new(),
            sum: None,
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
                    Prefetch::Text {
                        field,
                        query,
                        limit,
                    } => Some(WireLeg::Text {
                        j: *j,
                        field: field.clone(),
                        query: query.clone(),
                        limit: *limit,
                    }),
                    Prefetch::Trigram { .. } => None,
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
    // ⚠️ A leg's number is a query position, under `MAX_LEGS` and named once (code review,
    // M58): a cut by sum sizes its legs by it, so an unbounded one was an allocation the
    // caller chose.
    let mut named = std::collections::BTreeSet::new();
    for leg in &wire.legs {
        let j = match leg {
            WireLeg::Dense { j, .. } | WireLeg::Sparse { j, .. } | WireLeg::Text { j, .. } => *j,
        };
        if j >= pstore_query::MAX_LEGS || !named.insert(j) {
            return Err(refuse(format!(
                "leg {j}: each leg once, numbered under {}",
                pstore_query::MAX_LEGS
            )));
        }
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
            WireLeg::Text {
                j,
                field,
                query,
                limit,
            } => Ok((
                j,
                Prefetch::Text {
                    field,
                    query,
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
    let sum = match &wire.sum {
        None => None,
        Some(s) => Some(
            s.cut()
                .ok_or_else(|| refuse("a sum part's weights: at most one a leg"))?,
        ),
    };
    Ok((
        Part {
            index: wire.index,
            fts,
            legs,
            targets,
            shadow: wire.shadow,
            terms: wire.terms,
            sum,
        },
        wire.filters,
    ))
}

/// A query error a part raised: the client's, answered `422` so the coordinator counts no
/// failure (M54); anything else `500`.
fn part_error(e: &pstore_query::QueryError) -> ApiError {
    match e {
        pstore_query::QueryError::Unimplemented(_)
        | pstore_query::QueryError::Format(
            pstore_format::FormatError::UnknownField
            | pstore_format::FormatError::DimensionMismatch { .. },
        ) => ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "bad_query", e.to_string()),
        _ => ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string()),
    }
}

/// `POST /v1/internal/part`: runs one share of another server's query (M54), or one phase of
/// it (M55).
pub(crate) async fn serve_part<S: BlobStore + 'static>(
    State(api): State<Arc<Api<S>>>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<Response, ApiError> {
    let tenant = tenant_of(&headers)?;
    let raw: serde_json::Value = serde_json::from_slice(&body)
        .map_err(|e| ApiError::bad_request(format!("malformed part: {e}")))?;
    let protocol = raw.get("protocol").and_then(serde_json::Value::as_u64);
    let phase = raw.get("phase").and_then(serde_json::Value::as_str);
    match (protocol, phase) {
        (Some(p), None) if p == u64::from(PROTOCOL) => serve_whole(&api, tenant, raw).await,
        (Some(p), Some("open")) if p == u64::from(PHASED) || p == u64::from(SUMMED) => {
            serve_open(&api, tenant, raw).await
        }
        (Some(p), Some("scan")) if p == u64::from(PHASED) || p == u64::from(SUMMED) => {
            serve_scan(&api, tenant, raw).await
        }
        _ => Err(ApiError::new(
            StatusCode::CONFLICT,
            "protocol_mismatch",
            format!(
                "this server speaks part protocols {PROTOCOL}, and {PHASED} and {SUMMED} (open, \
                 scan), not {protocol:?} {phase:?}"
            ),
        )),
    }
}

fn wire_of<T: serde::de::DeserializeOwned>(raw: serde_json::Value) -> Result<T, ApiError> {
    serde_json::from_value(raw).map_err(|e| ApiError::bad_request(format!("malformed part: {e}")))
}

/// A part in one exchange (M54).
async fn serve_whole<S: BlobStore + 'static>(
    api: &Arc<Api<S>>,
    tenant: TenantId,
    raw: serde_json::Value,
) -> Result<Response, ApiError> {
    let wire: WirePart = wire_of(raw)?;
    guard(tenant, &wire)?;
    // M58 (code review): a cut by sum is protocol 3's alone, never one exchange's.
    if wire.sum.is_some() {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "bad_part",
            "a part cut by sum is protocol 3, in two phases",
        ));
    }
    let (part, filters) = decode(wire)?;
    if part.legs.iter().any(|(_, p)| !pstore_query::splittable(p)) {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "bad_part",
            "a text leg is split in two phases, never in one exchange",
        ));
    }
    let filter = filters.as_ref().map(predicate).transpose()?;
    let engine = api.engine(tenant).await;
    api.parts_served.fetch_add(1, Ordering::Relaxed);
    engine
        .part(&part, filter.as_ref())
        .await
        .map(|hits| axum::Json(WireHits::of(&hits)).into_response())
        .map_err(|e| part_error(&e))
}

/// Phase 1 of a phased part (M55): open, hold, and answer with the share's statistics.
async fn serve_open<S: BlobStore + 'static>(
    api: &Arc<Api<S>>,
    tenant: TenantId,
    raw: serde_json::Value,
) -> Result<Response, ApiError> {
    let wire: WirePart = wire_of(raw)?;
    guard(tenant, &wire)?;
    // M58: a cut by sum is protocol 3's, and protocol 3 is a cut by sum. `WirePart` ignores a
    // field it does not know, so nothing else would refuse a `sum` sent at 2.
    if (wire.protocol == SUMMED) != wire.sum.is_some() {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "bad_part",
            "a part cut by sum is protocol 3, and protocol 3 is cut by sum",
        ));
    }
    let (part, filters) = decode(wire)?;
    // Parsed now, so a filter that cannot be is refused before anything is held.
    filters.as_ref().map(predicate).transpose()?;
    let engine = api.engine(tenant).await;
    api.parts_served.fetch_add(1, Ordering::Relaxed);
    let (open, stats) = engine.open_part(&part).await.map_err(|e| part_error(&e))?;
    let size = open.bytes();
    let id = api
        .holder()
        .admit(
            tokio::time::Instant::now(),
            Held {
                tenant,
                part,
                filters,
                open,
            },
            size,
        )
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "part_too_large",
                "this part is larger than the bytes a server holds between phases",
            )
        })?;
    Ok(axum::Json(WireOpened {
        id,
        stats: WireStats::of(&stats),
    })
    .into_response())
}

/// Phase 2 of a phased part (M55): the held part, scanned against the global statistics.
async fn serve_scan<S: BlobStore + 'static>(
    api: &Arc<Api<S>>,
    tenant: TenantId,
    raw: serde_json::Value,
) -> Result<Response, ApiError> {
    let wire: WireScan = wire_of(raw)?;
    let held = api
        .holder()
        .take(tokio::time::Instant::now(), &wire.id, tenant, &wire.index)
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::GONE,
                "part_gone",
                "no part is held under this id for this tenant and index",
            )
        })?;
    // M58 (code review): a scan speaks its part's protocol, 3 for a cut by sum and 2 else.
    if (wire.protocol == SUMMED) != held.part.sum.is_some() {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "bad_part",
            "a scan's protocol is its part's: 3 for a part cut by sum, 2 otherwise",
        ));
    }
    let filter = held.filters.as_ref().map(predicate).transpose()?;
    let engine = api.engine(tenant).await;
    engine
        .scan_part(&held.part, &held.open, filter.as_ref(), &wire.stats.stats())
        .await
        .map(|hits| axum::Json(WireHits::of(&hits)).into_response())
        .map_err(|e| part_error(&e))
}

/// A part held between its phases (M55).
pub(crate) struct Held {
    tenant: TenantId,
    part: Part,
    filters: Option<serde_json::Value>,
    open: pstore_query::OpenPart,
}

/// The parts a server holds between their phases (M55): server-wide, so a tenant engine's
/// eviction neither drops one nor is held up by one.
///
/// ⚠️ **Ephemeral, and safe.** An id is 128 random bits. A part is taken -- out of the holder
/// -- when its scan starts, so eviction never drops one being scanned. A scan naming another
/// tenant or index finds nothing, exactly as for an id never issued. And the whole is bounded
/// by count, bytes and age, oldest out first.
pub(crate) struct Holder {
    parts: std::collections::HashMap<String, (tokio::time::Instant, usize, Held)>,
    order: std::collections::VecDeque<String>,
    bytes: usize,
    limits: (usize, usize, Duration),
    /// Parts dropped before their scan: `pstore_peer_parts_expired`.
    pub(crate) expired: u64,
}

impl std::fmt::Debug for Holder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Holder")
            .field("parts", &self.parts.len())
            .field("bytes", &self.bytes)
            .field("expired", &self.expired)
            .finish()
    }
}

impl Default for Holder {
    fn default() -> Self {
        Self::new(HOLD_PARTS, HOLD_BYTES, HOLD_FOR)
    }
}

impl Holder {
    pub(crate) fn new(parts: usize, bytes: usize, ttl: Duration) -> Self {
        Self {
            parts: std::collections::HashMap::new(),
            order: std::collections::VecDeque::new(),
            bytes: 0,
            limits: (parts, bytes, ttl),
            expired: 0,
        }
    }

    /// Holds `held`, of `size` bytes, returning its id; `None` when it alone is over the byte
    /// budget, and it then evicts nothing.
    pub(crate) fn admit(
        &mut self,
        now: tokio::time::Instant,
        held: Held,
        size: usize,
    ) -> Option<String> {
        let (max_parts, max_bytes, _) = self.limits;
        if size > max_bytes {
            return None;
        }
        self.sweep(now);
        while self.parts.len() >= max_parts || self.bytes + size > max_bytes {
            if !self.drop_oldest() {
                break;
            }
        }
        let mut raw = [0u8; 16];
        getrandom::fill(&mut raw).ok()?;
        let id: String = raw.iter().map(|b| format!("{b:02x}")).collect();
        self.bytes += size;
        self.order.push_back(id.clone());
        self.parts.insert(id.clone(), (now, size, held));
        Some(id)
    }

    /// The part held as `id` for `tenant`'s `index`, taken out of the holder; `None` for any
    /// other -- expired, already scanned, never issued, or another tenant's.
    pub(crate) fn take(
        &mut self,
        now: tokio::time::Instant,
        id: &str,
        tenant: TenantId,
        index: &str,
    ) -> Option<Held> {
        self.sweep(now);
        let (_, _, held) = self.parts.get(id)?;
        if held.tenant != tenant || held.part.index != index {
            return None;
        }
        let (_, size, held) = self.parts.remove(id)?;
        self.bytes -= size;
        Some(held)
    }

    /// Drops every part older than the hold.
    pub(crate) fn sweep(&mut self, now: tokio::time::Instant) {
        let ttl = self.limits.2;
        while let Some(id) = self.order.front() {
            match self.parts.get(id) {
                None => {
                    self.order.pop_front();
                }
                Some((at, _, _)) if now.duration_since(*at) > ttl => {
                    self.drop_oldest();
                }
                Some(_) => break,
            }
        }
    }

    /// Drops the oldest part still held, if any.
    fn drop_oldest(&mut self) -> bool {
        while let Some(id) = self.order.pop_front() {
            if let Some((_, size, _)) = self.parts.remove(&id) {
                self.bytes -= size;
                self.expired += 1;
                return true;
            }
        }
        false
    }

    /// Parts held now.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.parts.len()
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
            terms: vec!["alpha".to_owned(), "zeta".to_owned()],
            sum: None,
        };
        let filters = Some(serde_json::json!(["k", "Eq", 1]));
        // M55: phase 1 carries every leg -- the text leg too -- and the terms.
        let mut phased = part.clone();
        phased.legs.push((
            5,
            Prefetch::Text {
                field: "text".to_owned(),
                query: "Alpha zeta".to_owned(),
                limit: 9,
            },
        ));
        let raw = serde_json::to_vec(&WirePart::opening(&phased, filters.clone())).unwrap();
        let wire: WirePart = serde_json::from_slice(&raw).unwrap();
        assert_eq!(
            (wire.protocol, wire.phase.as_deref()),
            (PHASED, Some("open"))
        );
        let (back, _) = decode(wire).unwrap();
        assert_eq!(format!("{:?}", back.legs), format!("{:?}", phased.legs));
        assert_eq!(back.terms, phased.terms);
        // And the statistics, both ways.
        let stats = Stats {
            doc_count: 7,
            total_tokens: 91,
            df: [("alpha".to_owned(), 3), ("zeta".to_owned(), 1)].into(),
        };
        let wire: WireStats =
            serde_json::from_slice(&serde_json::to_vec(&WireStats::of(&stats)).unwrap()).unwrap();
        let back = wire.stats();
        assert_eq!(
            (back.doc_count, back.total_tokens, back.df),
            (stats.doc_count, stats.total_tokens, stats.df)
        );
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
            phase: None,
            terms: vec![],
            sum: None,
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

    /// A held part of `tenant`'s `index` with nothing opened.
    async fn held(tenant: u128, index: &str) -> Held {
        let (open, _) = pstore_query::open_part(&pstore_blob::MemoryStore::new(), &[], &[], &[])
            .await
            .unwrap();
        Held {
            tenant: TenantId(tenant),
            part: Part {
                index: index.to_owned(),
                fts: pstore_format::text::FullText::default(),
                legs: vec![],
                targets: vec![],
                shadow: 0,
                terms: vec![],
                sum: None,
            },
            filters: None,
            open,
        }
    }

    #[test]
    fn the_holder_keeps_what_the_spec_says() {
        // Spec rule 6: 256 parts, 256 MiB, 10 s.
        assert_eq!(
            (HOLD_PARTS, HOLD_BYTES, HOLD_FOR),
            (256, 268_435_456, Duration::from_secs(10))
        );
        assert_eq!(Holder::default().limits, (HOLD_PARTS, HOLD_BYTES, HOLD_FOR));
    }

    #[tokio::test]
    async fn a_failed_phase_says_why() {
        let cluster = Arc::new(
            Cluster::new(PeerConfig {
                servers: vec!["http://127.0.0.1:1".to_owned()],
                me: 0,
                timeout: Duration::from_secs(1),
            })
            .unwrap(),
        );
        let peers = QueryPeers {
            list: cluster.list(),
            cluster,
            tenant: TenantId(7),
            filters: None,
        };
        let part = || Part {
            index: "docs".to_owned(),
            fts: pstore_format::text::FullText::default(),
            legs: vec![],
            targets: vec![],
            shadow: 0,
            terms: vec![],
            sum: None,
        };
        let Phased { stats, scan } = peers.phased(5, part());
        assert_eq!(stats.await.unwrap_err(), "no such server");
        assert_eq!(
            scan(Stats::default()).await.unwrap_err(),
            "no part was held"
        );
        // Each phase's failure counted once for the part.
        assert_eq!(peers.cluster.failed.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn held_parts_are_bounded_safe_and_dropped() {
        let t0 = tokio::time::Instant::now();
        let mut h = Holder::new(3, 100, Duration::from_secs(10));
        // Taken once, then gone; another tenant's or index's take finds nothing.
        let a = h.admit(t0, held(7, "docs").await, 10).unwrap();
        assert!(
            h.take(t0, &a, TenantId(8), "docs").is_none(),
            "another tenant"
        );
        assert!(
            h.take(t0, &a, TenantId(7), "other").is_none(),
            "another index"
        );
        assert!(
            h.take(t0, "00", TenantId(7), "docs").is_none(),
            "never issued"
        );
        assert!(h.take(t0, &a, TenantId(7), "docs").is_some());
        assert!(
            h.take(t0, &a, TenantId(7), "docs").is_none(),
            "scanned once"
        );
        assert_eq!(h.expired, 0);
        // Ten seconds held, then gone, and counted.
        let b = h.admit(t0, held(7, "docs").await, 10).unwrap();
        let later = t0 + Duration::from_secs(10);
        assert!(
            h.take(later, &b, TenantId(7), "docs").is_some(),
            "at the hold"
        );
        let c = h.admit(t0, held(7, "docs").await, 10).unwrap();
        let past = t0 + Duration::from_millis(10_001);
        assert!(h.take(past, &c, TenantId(7), "docs").is_none(), "past it");
        assert_eq!(h.expired, 1);
        // Past three parts the oldest goes.
        let (d, e, f) = (
            h.admit(past, held(7, "docs").await, 10).unwrap(),
            h.admit(past, held(7, "docs").await, 10).unwrap(),
            h.admit(past, held(7, "docs").await, 10).unwrap(),
        );
        let g = h.admit(past, held(7, "docs").await, 10).unwrap();
        assert_eq!(h.len(), 3);
        assert!(
            h.take(past, &d, TenantId(7), "docs").is_none(),
            "the oldest went"
        );
        assert_eq!(h.expired, 2);
        // Past a hundred bytes the oldest goes too.
        let big = h.admit(past, held(7, "docs").await, 81).unwrap();
        assert!(h.take(past, &e, TenantId(7), "docs").is_none());
        assert!(h.take(past, &f, TenantId(7), "docs").is_none());
        assert_eq!(h.len(), 2, "{g} and {big}");
        // A part larger than the budget is refused, and evicts nothing.
        assert!(h.admit(past, held(7, "docs").await, 101).is_none());
        assert_eq!(h.len(), 2);
        // A part taken for its scan is out of reach of eviction: admitting more cannot drop it.
        let taken = h.take(past, &g, TenantId(7), "docs").unwrap();
        for _ in 0..5 {
            h.admit(past, held(7, "docs").await, 50).unwrap();
        }
        assert_eq!(taken.part.index, "docs");
        // Ids are 128 random bits: none repeats in a thousand.
        let mut many = Holder::new(2_000, usize::MAX, Duration::from_secs(10));
        let mut ids = std::collections::BTreeSet::new();
        for _ in 0..1_000 {
            let id = many.admit(t0, held(7, "docs").await, 0).unwrap();
            assert_eq!(id.len(), 32);
            ids.insert(id);
        }
        assert_eq!(ids.len(), 1_000);
    }

    #[tokio::test]
    async fn the_byte_budget_holds_exactly_its_size() {
        // Sweep: the budget's two comparisons, each at its boundary.
        let t0 = tokio::time::Instant::now();
        let mut h = Holder::new(3, 100, Duration::from_secs(10));
        assert!(
            h.admit(t0, held(7, "docs").await, 100).is_some(),
            "one part of the budget"
        );
        let mut h = Holder::new(3, 100, Duration::from_secs(10));
        let a = h.admit(t0, held(7, "docs").await, 50).unwrap();
        h.admit(t0, held(7, "docs").await, 50).unwrap();
        assert_eq!(h.len(), 2, "two halves fill it, and evict nothing");
        assert!(h.take(t0, &a, TenantId(7), "docs").is_some());
        // What an operator's log shows of it.
        let shown = format!("{h:?}");
        assert!(
            shown.contains("expired: 0") && shown.contains("bytes: 50"),
            "{shown}"
        );
    }
}
