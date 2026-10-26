//! The composition root: it reads the environment, opens a store, and serves.
//!
//! ⚠️ **`main.rs` wires, `lib.rs` decides** — the rule `pstore-node` follows, for the reason
//! its own docs give: a decision inside `main` is unreachable from `cargo test` and is
//! measured at 0% coverage. Everything here is wiring, and everything with a branch worth
//! being wrong about is in the library.

use pstore_blob::{Accounted, BlobStore, MemoryStore, ObjectStoreBackend};
use pstore_server::{
    Api, Backend, Config, Profile, SourceConfig, azure_store, open_cache, s3_capabilities,
    serve_folding,
};
use std::sync::Arc;

#[tokio::main]
async fn main() -> std::process::ExitCode {
    let auth = match pstore_server::Auth::from_vars(|k| std::env::var(k).ok()) {
        Ok(auth) => auth,
        Err(e) => {
            eprintln!("pstore-server: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    let config = match Config::from_vars(|k| std::env::var(k).ok()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("pstore-server: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    let peer = match pstore_server::PeerListenerConfig::from_vars(
        config.peers.is_some() || config.gossip.is_some(),
        |key| std::env::var(key).ok(),
    ) {
        Ok(peer) => peer,
        Err(error) => {
            eprintln!("pstore-server: {error}");
            return std::process::ExitCode::FAILURE;
        }
    };
    let auth = match &peer {
        Some(peer) => auth.with_peer_tls(peer.tls.clone(), peer.transport),
        None => auth,
    };
    match config.backend {
        Backend::Memory => run(Accounted::new(MemoryStore::new()), &config, auth, peer).await,
        Backend::S3 => match s3(&config) {
            Ok(store) => run(Accounted::new(store), &config, auth, peer).await,
            Err(e) => {
                eprintln!("pstore-server: cannot open {}: {e}", config.endpoint);
                std::process::ExitCode::FAILURE
            }
        },
        Backend::Azure => match config
            .azure
            .as_ref()
            .map(|a| azure_store(a, config.profile))
        {
            Some(Ok(store)) => run(Accounted::new(store), &config, auth, peer).await,
            Some(Err(e)) => {
                eprintln!("pstore-server: cannot open the Azure container: {e}");
                std::process::ExitCode::FAILURE
            }
            None => {
                eprintln!("pstore-server: PSTORE_BACKEND=azure without its configuration");
                std::process::ExitCode::FAILURE
            }
        },
    }
}

/// Builds the S3 backend, opened with the capabilities the operator's profile claims.
///
/// ⚠️ The credentials are read here and nowhere else, and `with_allow_http` is what lets this
/// reach MinIO and an in-VPC endpoint. TLS to a real bucket is the default because the URL
/// scheme decides it.
fn s3(config: &Config) -> Result<impl BlobStore, Box<dyn std::error::Error>> {
    bucket(
        &config.endpoint,
        &config.bucket,
        config.credentials.as_ref(),
        &env("PSTORE_REGION", "us-east-1"),
        config.profile,
    )
}

/// One S3-compatible bucket.
///
/// ⚠️ The credentials are applied only when the operator gave both. Unset, `AmazonS3Builder`
/// uses its own provider chain -- which is how an instance profile or a service-account role
/// works, and the only way this image is usable on EC2 or EKS. `Config` decides; this applies.
fn bucket(
    endpoint: &str,
    name: &str,
    credentials: Option<&(String, String)>,
    region: &str,
    profile: Profile,
) -> Result<ObjectStoreBackend, Box<dyn std::error::Error>> {
    let mut builder = object_store::aws::AmazonS3Builder::new()
        .with_endpoint(endpoint)
        .with_bucket_name(name)
        .with_allow_http(true)
        .with_region(region);
    if let Some((key, secret)) = credentials {
        builder = builder
            .with_access_key_id(key)
            .with_secret_access_key(secret);
    }
    let s3 = builder.build()?;
    Ok(ObjectStoreBackend::new(
        Arc::new(s3),
        s3_capabilities(endpoint, profile),
    ))
}

/// A replication source (M22): read only, so it needs no fencing and no profile.
fn source(s: &SourceConfig) -> Result<Arc<dyn BlobStore>, Box<dyn std::error::Error>> {
    let b = bucket(
        &s.endpoint,
        &s.bucket,
        s.credentials.as_ref(),
        &s.region,
        Profile::Unprobed,
    )?;
    Ok(Arc::new(b))
}

fn env(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_owned())
}

/// Serves `store`, or refuses.
///
/// ⚠️ Generic over the store so the two backends share one tail: a duplicated tail is how one
/// of them quietly loses the shutdown handler.
async fn run<S: BlobStore + 'static>(
    store: Accounted<S>,
    config: &Config,
    auth: pstore_server::Auth,
    peer: Option<pstore_server::PeerListenerConfig>,
) -> std::process::ExitCode {
    let cache = match open_cache(&store, config).await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("pstore-server: cannot read the store id the read cache needs: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    let built = match &cache {
        Some(c) => Api::with_cache_and_auth(store, config.lane, Arc::clone(c), auth),
        None => Api::with_auth(store, config.lane, auth),
    };
    let api = match built {
        Ok(a) => a,
        Err(e) => {
            // ⚠️ The refusal criterion 3 rests on. An operator who has not run the
            // conformance suite gets this instead of a server that will accept a `durable`
            // write it cannot fence.
            eprintln!("pstore-server: refusing to serve: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    api.limit_engines(config.engines);
    // M129: `durability: "applied"` only once the operator says every node runs a release
    // that folds it. Any other value, or none, leaves it refused.
    // M129: `1` serves applied writes; M130: `2` also serves their affected ids.
    let applied = std::env::var("PSTORE_APPLIED_WRITES");
    api.set_applied_writes(matches!(applied.as_deref(), Ok("1" | "2" | "3")));
    api.set_affected_ids(matches!(applied.as_deref(), Ok("2" | "3")));
    // M132: idempotency keys need every node at level 3.
    api.set_idempotency(applied.as_deref() == Ok("3"));
    // M54: a query's vector legs split across the servers PSTORE_PEERS names.
    if let Err(e) = api.set_peers(config.peers.clone()) {
        eprintln!("pstore-server: refusing to serve: {e}");
        return std::process::ExitCode::FAILURE;
    }
    // M56: or from a SWIM membership of its own, held for the life of the process.
    let _gossip = match config.gossip.clone() {
        None => None,
        Some(g) => match api.join_peers(g).await {
            Ok(handle) => Some(handle),
            Err(e) => {
                eprintln!("pstore-server: refusing to serve: {e}");
                return std::process::ExitCode::FAILURE;
            }
        },
    };
    let mut sources = std::collections::BTreeMap::new();
    for s in &config.sources {
        match source(s) {
            Ok(store) => {
                sources.insert(s.name.clone(), store);
            }
            Err(e) => {
                eprintln!(
                    "pstore-server: cannot open replication source {}: {e}",
                    s.name
                );
                return std::process::ExitCode::FAILURE;
            }
        }
    }
    api.configure_replication(config.replication.unwrap_or_default(), sources);
    api.recheck_lanes_within(config.lane_recheck);
    if let Some(prefixes) = config.external_sources.clone() {
        api.allow_external_sources(prefixes);
    }
    let listener = match tokio::net::TcpListener::bind(&config.bind).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("pstore-server: cannot bind {}: {e}", config.bind);
            return std::process::ExitCode::FAILURE;
        }
    };
    eprintln!(
        "pstore-server: listening on {}, lane {:?}, backend {:?}, profile {:?}, fold {:?}, cache {:?}",
        config.bind,
        config.lane,
        config.backend,
        config.profile,
        config.fold,
        cache.as_ref().map(|c| c.disk_state())
    );
    let served = match peer {
        None => {
            serve_folding(
                api,
                listener,
                stopped(),
                config.fold,
                config.gc,
                config.replication.is_some(),
            )
            .await
        }
        Some(peer) => {
            let tcp = match tokio::net::TcpListener::bind(peer.bind).await {
                Ok(tcp) => tcp,
                Err(error) => {
                    eprintln!("pstore-server: cannot bind peer TCP listener: {error}");
                    return std::process::ExitCode::FAILURE;
                }
            };
            let udp = match tcp.local_addr().and_then(std::net::UdpSocket::bind) {
                Ok(udp) => udp,
                Err(error) => {
                    eprintln!("pstore-server: cannot bind peer UDP listener: {error}");
                    return std::process::ExitCode::FAILURE;
                }
            };
            let (halt, mut peer_halt) = tokio::sync::watch::channel(false);
            let mut public_halt = peer_halt.clone();
            let mut peers = Box::pin(pstore_server::serve_peers(
                api.clone(),
                tcp,
                udp,
                async move {
                    let _ = peer_halt.changed().await;
                },
            ));
            let mut public = Box::pin(serve_folding(
                api,
                listener,
                async move {
                    let _ = public_halt.changed().await;
                },
                config.fold,
                config.gc,
                config.replication.is_some(),
            ));
            tokio::select! {
                result = &mut public => { let _ = halt.send(true); result.and(peers.await) },
                result = &mut peers => { let _ = halt.send(true); result.and(public.await) },
                () = stopped() => {
                    let _ = halt.send(true);
                    let (public, peers) = tokio::join!(public, peers);
                    public.and(peers)
                },
            }
        }
    };
    // ⚠️ After the server has stopped: the disk tier's writes in flight are flushed, or a
    // deploy would lose them every time (M20).
    if let Some(c) = &cache {
        c.close().await;
    }
    match served {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("pstore-server: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

/// Resolves on Ctrl-C, or on SIGTERM where there is one.
///
/// ⚠️ SIGTERM is what an orchestrator sends on every deploy. Stopping only on Ctrl-C, a deploy
/// killed the process before the read cache's writes in flight were flushed (M20's code
/// review), and before the fold and reap loops were awaited.
async fn stopped() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = term.recv() => {}
                }
            }
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
