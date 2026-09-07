//! Membership roster and placement. **Nodes own nothing** — that is what makes this cheap.
//!
//! Placement decides which nodes *cache* an index's shards. Because the blob store is the
//! only durable tier, changing the fleet copies **no bytes between machines**: it changes
//! who will fetch what next. `routing-and-placement.md` states both halves in one line —
//! growing 1,000 → 2,000 nodes remaps "~50% of keys… **Nothing is copied.**" Churn is large
//! *and* data movement is zero, and conflating those two is how a scaling story turns into
//! a rebalancing protocol.

pub mod placement;
pub mod roster;

pub use placement::Placement;
pub use roster::{Cell, Roster, RosterError};
