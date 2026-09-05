//! Prints the CAS contention curve.
//!
//!     cargo run -p pstore-testkit --example contention
//!
//! This is the local half of OQ-5. It cannot say what S3 does; it says what our commit
//! protocol does as contention rises, so one measurement in M0b locates reality on a
//! curve rather than starting the analysis from nothing.
#![allow(
    clippy::print_stdout,
    reason = "this example's whole purpose is to print"
)]

use pstore_blob::MemoryStore;
use pstore_testkit::sweep;
use std::sync::Arc;

#[tokio::main]
async fn main() {
    let store = Arc::new(MemoryStore::new());
    let points = sweep::contention_sweep(store, &[1, 2, 4, 8, 16, 32, 64, 128], 8).await;
    print!("{}", sweep::render(&points));
}
