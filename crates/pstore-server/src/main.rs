//! The composition root: it reads the environment, opens a store, and serves.
//!
//! ⚠️ **`main.rs` wires, `lib.rs` decides** — the rule `pstore-node` follows, for the reason
//! its own docs give: a decision inside `main` is unreachable from `cargo test` and is
//! measured at 0% coverage. Everything here is wiring, and everything with a branch worth
//! being wrong about is in the library.

use pstore_blob::{Accounted, BlobStore, MemoryStore, ObjectStoreBackend};
use pstore_server::{Api, Backend, Config, s3_capabilities, serve};
use std::sync::Arc;

#[tokio::main]
async fn main() -> std::process::ExitCode {
    let config = match Config::from_vars(|k| std::env::var(k).ok()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("pstore-server: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    match config.backend {
        Backend::Memory => run(Accounted::new(MemoryStore::new()), &config).await,
        Backend::S3 => match s3(&config) {
            Ok(store) => run(Accounted::new(store), &config).await,
            Err(e) => {
                eprintln!("pstore-server: cannot open {}: {e}", config.endpoint);
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
    let mut builder = object_store::aws::AmazonS3Builder::new()
        .with_endpoint(&config.endpoint)
        .with_bucket_name(&config.bucket)
        .with_allow_http(true)
        .with_region(env("PSTORE_REGION", "us-east-1"));
    // ⚠️ Only when the operator gave both. Unset, `AmazonS3Builder` uses its own provider
    // chain -- which is how an instance profile or a service-account role works, and the only
    // way this image is usable on EC2 or EKS. `Config` decides; this applies.
    if let Some((key, secret)) = &config.credentials {
        builder = builder
            .with_access_key_id(key)
            .with_secret_access_key(secret);
    }
    let s3 = builder.build()?;
    Ok(ObjectStoreBackend::new(
        Arc::new(s3),
        s3_capabilities(&config.endpoint, config.profile),
    ))
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
) -> std::process::ExitCode {
    let api = match Api::new(store, config.lane) {
        Ok(a) => a,
        Err(e) => {
            // ⚠️ The refusal criterion 3 rests on. An operator who has not run the
            // conformance suite gets this instead of a server that will accept a `durable`
            // write it cannot fence.
            eprintln!("pstore-server: refusing to serve: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    let listener = match tokio::net::TcpListener::bind(&config.bind).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("pstore-server: cannot bind {}: {e}", config.bind);
            return std::process::ExitCode::FAILURE;
        }
    };
    eprintln!(
        "pstore-server: listening on {}, lane {:?}, backend {:?}, profile {:?}",
        config.bind, config.lane, config.backend, config.profile
    );
    let shutdown = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    match serve(api, listener, shutdown).await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("pstore-server: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}
