//! Vector quantization and the clustered index.
//!
//! Layer 3. Nothing here touches the blob store: it takes vectors and returns codes and
//! candidate sets, so it can be tested without a store and reused by whatever fetches.

pub mod rabitq;
