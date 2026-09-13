//! Prints the three sensitivity curves D-101 asks for.
//!
//!     cargo run -p pstore-testkit --example contention
//!
//! This is the local half of OQ-5. It cannot say what S3 does; it says what our commit
//! protocol does as contention, latency and refusal rise, so one measurement in M0b locates
//! reality on a curve rather than starting the analysis from nothing.
//!
//! ⚠️ **The latency arm is real wall time** — there is no paused clock outside a test — which
//! is why it is here and not in `cargo test`, where `cargo mutants` would pay it once per
//! mutant.
#![allow(
    clippy::print_stdout,
    clippy::expect_used,
    reason = "a measurement harness that prints: a sweep that cannot seed its key has no \
              number to report, and panicking names that immediately"
)]

use pstore_blob::MemoryStore;
use pstore_testkit::sweep;
use std::sync::Arc;
use std::time::Duration;

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

#[tokio::main]
async fn main() {
    let started = std::time::Instant::now();

    println!("## contention: writers racing for one key\n");
    let store = Arc::new(MemoryStore::new());
    let points = sweep::contention_sweep(store, &[1, 2, 4, 8, 16, 32, 64, 128], 8)
        .await
        .expect("an uninjected store always seeds");
    print!("{}", sweep::render(&points));

    // ⚠️ Both ends of the spread are swept, because the ceiling alone is not the variable
    // with a reason to move the attempt ratio: a fixed delay scales the vulnerable
    // read-to-CAS window and the whole cycle together, and jitter does not.
    println!("\n## latency: 8 writers, a fixed delay then a widening spread\n");
    let spreads = [
        (ms(0), ms(0)),
        (ms(5), ms(5)),
        (ms(20), ms(20)),
        (ms(0), ms(20)),
        (ms(0), ms(40)),
    ];
    let points = sweep::latency_sweep(&spreads, 8, 4, 0xC0FFEE)
        .await
        .expect("seeded outside the injection");
    print!("{}", sweep::render(&points));

    println!("\n## 412/409: 8 writers against a backend that refuses conditional writes\n");
    let points = sweep::cas_error_sweep(&[0.0, 0.25, 0.5, 0.75, 0.9, 1.0], 8, 4, 0xC0FFEE)
        .await
        .expect("seeded outside the injection");
    print!("{}", sweep::render(&points));

    println!(
        "\nwall clock for all three arms: {:.1}s",
        started.elapsed().as_secs_f64()
    );
}
