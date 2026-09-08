//! Vector quantization and the clustered index.
//!
//! Layer 3. Nothing here touches the blob store: it takes vectors and returns codes and
//! candidate sets, so it can be tested without a store and reused by whatever fetches.

pub mod cluster;
pub mod ladder;
pub mod lire;
pub mod rabitq;
pub mod search;
pub mod sparse;
pub mod sq8;
pub mod vec_index;
