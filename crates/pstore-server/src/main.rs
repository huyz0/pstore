//! The composition root: it reads the environment, opens a store, and serves.
//!
//! ⚠️ **`main.rs` wires, `lib.rs` decides** — the rule `pstore-node` follows, for the reason
//! its own docs give: a decision inside `main` is unreachable from `cargo test` and is
//! measured at 0% coverage. Everything here is wiring, and everything with a branch worth
//! being wrong about is in the library.

use pstore_blob::{Accounted, MemoryStore};
use pstore_server::{Api, Config, serve};

#[tokio::main]
async fn main() -> std::process::ExitCode {
    let config = match Config::from_vars(|k| std::env::var(k).ok()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("pstore-server: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    // ⚠️ **In-memory, and this binary says so rather than implying durability it does not
    // have.** A real backend is one line through `ObjectStoreBackend`, and the deny.toml
    // wrapper entry that permits it belongs with the milestone that runs against one — M7c
    // has no cloud account to point it at, and an S3 code path nothing has ever executed is
    // worse than an honest local one.
    let api = match Api::new(Accounted::new(MemoryStore::new()), config.lane) {
        Ok(a) => a,
        Err(e) => {
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
        "pstore-server: listening on {}, lane {:?}, in-memory store",
        config.bind, config.lane
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
