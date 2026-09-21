//! Runs the conformance suite against the emulators in `dev/docker-compose.yml`.
//!
//! ⚠️ **This is layer 2, and layer 2 tests plumbing.** D-99 is explicit: correctness is
//! proven against the in-process store, emulators validate HTTP wiring and error mapping,
//! and only a real cloud says anything about economics. A `Supported` printed here means
//! *this emulator answered ten probes correctly*, never *S3 does*.
//!
//! Not a `#[test]`: it needs containers, and a suite that cannot run without Docker is a
//! suite that stops running. `scripts/conformance.sh` is the entry point.

#![allow(
    clippy::print_stdout,
    clippy::print_stderr,
    reason = "this example's output IS its product -- scripts/conformance.sh captures it"
)]

use pstore_blob::{BlobStore, Capabilities, ObjectStoreBackend, Support};
use std::sync::Arc;

/// A backend we tried to reach, and what came back.
enum Outcome {
    Probed(pstore_testkit::conformance::Report),
    /// ⚠️ **Not a probe outcome.** `Report::conforms()` is `all()` over the probe list, which
    /// is `true` for an empty list — so a backend that could not be reached is
    /// indistinguishable from a perfect one unless it is a different variant entirely. That
    /// is the failure mode of every integration suite that reports green while connected to
    /// nothing.
    Unreachable(String),
}

fn env(k: &str, d: &str) -> String {
    std::env::var(k).unwrap_or_else(|_| d.to_owned())
}

fn s3() -> Result<Arc<dyn object_store::ObjectStore>, String> {
    let endpoint = env("PSTORE_S3_ENDPOINT", "http://127.0.0.1:9000");
    object_store::aws::AmazonS3Builder::new()
        .with_endpoint(endpoint)
        .with_bucket_name(env("PSTORE_S3_BUCKET", "pstore"))
        .with_access_key_id(env("PSTORE_ACCESS_KEY", "pstore"))
        .with_secret_access_key(env("PSTORE_SECRET_KEY", "pstore-dev-secret"))
        .with_allow_http(true)
        .with_region("us-east-1")
        // ⚠️ Already the default in `object_store` 0.14.1, and stated rather than assumed:
        // this mode sends `If-None-Match: *` on the wire with no client-side emulation, so
        // the create-if-absent probe measures the server and not the client. Pinned explicitly
        // because the day the default changes is the day the probe silently starts
        // measuring something else.
        .with_conditional_put(object_store::aws::S3ConditionalPut::ETagMatch)
        .build()
        .map(|s| Arc::new(s) as Arc<dyn object_store::ObjectStore>)
        .map_err(|e| e.to_string())
}

fn azure() -> Result<Arc<dyn object_store::ObjectStore>, String> {
    object_store::azure::MicrosoftAzureBuilder::new()
        .with_use_emulator(true)
        .with_endpoint(env("PSTORE_AZURE_ENDPOINT", "http://127.0.0.1:10000"))
        .with_container_name(env("PSTORE_AZURE_CONTAINER", "pstore"))
        .with_allow_http(true)
        .build()
        .map(|s| Arc::new(s) as Arc<dyn object_store::ObjectStore>)
        .map_err(|e| e.to_string())
}

fn gcs() -> Result<Arc<dyn object_store::ObjectStore>, String> {
    let endpoint = env("PSTORE_GCS_ENDPOINT", "http://localhost:4443");
    // ⚠️ **The endpoint goes in the service-account JSON, not in `with_url`.** `with_url`
    // parses only `gs://`, so passing the emulator's `http://` there is rejected before any
    // request is made — which is what the first run of this probe recorded, and half of
    // OQ-153's answer: the obstacle is the client, not the emulator.
    let key = FAKE_SERVICE_ACCOUNT.replace("GCS_BASE_URL", &endpoint);
    object_store::gcp::GoogleCloudStorageBuilder::new()
        .with_bucket_name(env("PSTORE_GCS_BUCKET", "pstore"))
        .with_service_account_key(key)
        .with_client_options(object_store::ClientOptions::new().with_allow_http(true))
        .build()
        .map(|s| Arc::new(s) as Arc<dyn object_store::ObjectStore>)
        .map_err(|e| e.to_string())
}

/// A syntactically valid service-account key with no privileges anywhere.
///
/// ⚠️ Not a secret and not usable: `fake-gcs-server` does not check credentials, but
/// `object_store`'s GCS builder refuses to construct without one. Recorded here rather than
/// in an env var so the obstacle is visible — it is half of OQ-153's answer.
const FAKE_SERVICE_ACCOUNT: &str = r#"{
  "gcs_base_url": "GCS_BASE_URL",
  "disable_oauth": true,
  "client_email": "probe@pstore.invalid",
  "private_key_id": "probe",
  "private_key": ""
}"#;

async fn probe(
    name: &str,
    build: fn() -> Result<Arc<dyn object_store::ObjectStore>, String>,
) -> Outcome {
    let inner = match build() {
        Ok(i) => i,
        Err(e) => return Outcome::Unreachable(format!("client could not be built: {e}")),
    };
    let store = ObjectStoreBackend::new(
        inner,
        Capabilities {
            backend: name.to_owned(),
            ..ObjectStoreBackend::unprobed(name)
        },
    );
    // ⚠️ Reachability is checked with a WRITE, not a read. A backend that 404s everything
    // answers a read happily and fails every probe, which would be recorded as ten
    // divergences rather than as "there is nothing there".
    let canary = pstore_blob::Key::new(format!("conformance/{name}/canary"));
    if let Err(e) = store.put(&canary, bytes::Bytes::from_static(b"ok")).await {
        return Outcome::Unreachable(e.to_string());
    }
    // ⚠️ **Nanoseconds, and the seconds version was a real bug.** The suite namespaces its
    // keys by `run_id`, so two runs sharing one make the second find the first's objects:
    // create-if-absent fails because the key exists, both CAS probes fail with it, and
    // `get_tag` reports "an absent key reported a tag". `scripts/conformance.sh --check` runs
    // seconds after a generate, so it flaked roughly one run in three — every backend
    // suddenly `Unsupported` — which looks exactly like a backend that broke overnight.
    #[expect(
        clippy::cast_possible_truncation,
        reason = "the low 64 bits of a nanosecond clock are what makes two runs differ"
    )]
    let run_id = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(1, |d| d.as_nanos() as u64);
    Outcome::Probed(pstore_testkit::conformance::run(&store, run_id).await)
}

fn support(s: &Support) -> String {
    match s {
        Support::Supported => "Supported".to_owned(),
        Support::Unsupported => "Unsupported".to_owned(),
        Support::Divergent(note) => format!("Divergent — {note}"),
    }
}

#[tokio::main]
async fn main() {
    let mut unreachable = 0usize;
    let mut divergent = 0usize;

    println!("# Capability matrix");
    println!();
    println!(
        "Generated by `scripts/conformance.sh` on {}. **Do not edit by hand** — \
         `scripts/conformance.sh --check` compares a fresh run against this file.",
        env("PSTORE_MATRIX_DATE", "an unrecorded date")
    );
    println!();
    println!(
        "⚠️ **These are emulators, and emulators test plumbing** (D-99). `Supported` here \
         means this emulator answered ten probes correctly. It says nothing about S3, GCS or \
         Azure, whose profiles need real accounts — M0b."
    );
    println!();
    println!(
        "⚠️ **Two of six fields are measured.** `cas` and `create_if_absent` come from the \
         probes. `backend` is a label; `delete_is_free` is a billing fact no probe can see; \
         `max_batch_delete` would need a search that is its own probe; `coalesce_gap` is `G*`, \
         which is OQ-2 and blocked on real clouds. Those four are **declared**."
    );

    for (name, build) in [
        ("rustfs", s3 as fn() -> _),
        ("azurite", azure as fn() -> _),
        ("fake-gcs-server", gcs as fn() -> _),
    ] {
        println!();
        println!("## {name}");
        println!();
        match probe(name, build).await {
            Outcome::Unreachable(why) => {
                unreachable += 1;
                println!("**UNREACHABLE** — {why}");
                println!();
                println!("No probe ran. This is recorded as an absence, never as a pass.");
                // ⚠️ **Absence is not ignorance here, and the matrix must not imply it is.**
                // OQ-153 asked the emulator directly over the JSON upload path, which is the
                // one route `object_store`'s XML client cannot take, and the answer is worse
                // than unreachable: `ifGenerationMatch` is **accepted and ignored**.
                if name.contains("gcs") {
                    println!();
                    println!(
                        "⚠️ **But the question was answered another way, and the answer is a \
                         no.** `scripts/conformance.sh --gcs-precondition` asks over the JSON \
                         upload path — the one route this client cannot take — and \
                         `ifGenerationMatch` is **accepted and ignored**: create-if-absent \
                         against an existing object returns 200 and overwrites, as does a \
                         compare-and-swap on a stale generation. Measured identically on \
                         1.52 and 1.55."
                    );
                    println!();
                    println!(
                        "⚠️ That is the worst shape a precondition can have — MinIO's wildcard \
                         had it (C-13) — because a CAS built on it reports success and loses \
                         the write. **Do not use this emulator as a CAS target.**"
                    );
                }
            }
            Outcome::Probed(r) => {
                if !r.conforms() {
                    divergent += 1;
                }
                println!("| probe | outcome |");
                println!("|---|---|");
                for p in &r.probes {
                    println!("| `{}` | {} |", p.name, support(&p.outcome));
                }
                println!();
                println!("| field | source | value |");
                println!("|---|---|---|");
                println!("| `cas` | measured | {} |", support(&r.observed.cas));
                println!(
                    "| `create_if_absent` | measured | {} |",
                    support(&r.observed.create_if_absent)
                );
                println!("| `backend` | declared | `{}` |", r.observed.backend);
                println!(
                    "| `delete_is_free` | declared | {} |",
                    r.observed.delete_is_free
                );
                println!(
                    "| `max_batch_delete` | declared | {} |",
                    r.observed.max_batch_delete
                );
                println!(
                    "| `coalesce_gap` | declared | {} |",
                    r.observed.coalesce_gap
                );
                println!();
                println!(
                    "Admits durable writes: **{}**",
                    r.observed.admits_durable_writes()
                );
            }
        }
    }

    // ⚠️ **A bitmask, not an ordering.** "We could not connect" and "it answered wrongly"
    // call for completely different actions, and the first version's `(0,_)=>3, _=>4` let any
    // unreachable backend MASK every divergence — which in today's real state is exactly what
    // happened: fake-gcs is unreachable and MinIO is divergent, so 3 was unreachable in
    // practice and the divergence OQ-150 rests on never reached the exit status.
    // 4 and 8 rather than 1 and 2, to stay clear of "general error" and "misuse".
    eprintln!("unreachable={unreachable} divergent={divergent}");
    let mut code = 0;
    if divergent > 0 {
        code |= 4;
    }
    if unreachable > 0 {
        code |= 8;
    }
    std::process::exit(code);
}
