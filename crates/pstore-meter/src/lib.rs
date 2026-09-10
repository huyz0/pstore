//! Per-tenant quotas over the one thing that reaches the bill.
//!
//! **Design rule 13**: "every resource that can be consumed on behalf of a tenant must be
//! metered per index — CPU, cache bytes, and especially **blob requests**, because that is the
//! one that shows up on the bill."
//!
//! ⚠️ **The counters already existed and nothing read them.** `pstore-blob`'s `Accounted` has
//! counted requests and bytes per tenant since M0a, and every RA assertion in this repository
//! rests on it. What never existed is anything that *acts* on a number.
//!
//! ## Two resources, two settlement rules
//!
//! | | admitted on | debited on |
//! |---|---|---|
//! | **requests** | the balance before dispatch, for the whole fan-out | the same number, at reservation |
//! | **bytes** | the balance before dispatch, whatever it is | what actually transferred, after — going negative if it overran |
//!
//! Requests can be reserved exactly because a fan-out's cost is knowable before it is issued.
//! Bytes cannot: their size is unknown before the transfer, and for `get` there is not even an
//! upper bound without a `head`, which the read path forbids. So a byte quota refuses the
//! *next* operation rather than the one that overran — bounded by that operation's size, never
//! unbounded, because [`Bucket`] carries the debt rather than forgiving it.
//!
//! ## ⚠️ Per tenant, where the rule says per index
//!
//! This sits on `BlobStore` — the only place a request cannot escape — and that trait has no
//! index concept. So **one index can starve the other forty-nine in the same tenant**, and
//! this crate does not stop it. Metering per index means metering above the trait, where every
//! future caller has to remember to do it.
//!
//! ## ⚠️ Per node, and therefore not a fleet-wide quota
//!
//! Nodes own nothing and coordinate about nothing, so a tenant's true rate is its per-node
//! rate times the nodes it reaches. This bounds the damage one node does, which is what a
//! token bucket in a shared process can honestly claim. `usage` is likewise this node's view;
//! summing across the fleet is the billing rollup's problem and it is not built.

mod bucket;
mod metered;

pub use bucket::{Bucket, Rate};
pub use metered::Metered;

use pstore_blob::OpClass;
use pstore_types::TenantId;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

/// What a tenant may spend.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Quota {
    /// Blob requests, of every class.
    pub requests: Rate,
    /// Bytes moved, read and written.
    pub bytes: Rate,
}

impl Quota {
    /// A quota that refuses nothing — the default, so adding the decorator to a stack changes
    /// no behaviour until someone sets a number.
    #[must_use]
    pub fn unlimited() -> Self {
        Self {
            requests: Rate::unlimited(),
            bytes: Rate::unlimited(),
        }
    }
}

/// What one tenant has spent on this node, in the shape `Accounted` keeps it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    /// Requests per [`OpClass`], indexed by [`Usage::slot`].
    pub requests: [u64; 4],
    /// Bytes per [`OpClass`].
    pub bytes: [u64; 4],
}

impl Usage {
    /// Which slot an `OpClass` occupies.
    #[must_use]
    pub fn slot(class: OpClass) -> usize {
        match class {
            OpClass::Read => 0,
            OpClass::Write => 1,
            OpClass::Delete => 2,
            OpClass::List => 3,
        }
    }

    /// Requests of one class.
    #[must_use]
    pub fn requests_of(&self, class: OpClass) -> u64 {
        self.requests.get(Self::slot(class)).copied().unwrap_or(0)
    }

    /// Bytes of one class.
    #[must_use]
    pub fn bytes_of(&self, class: OpClass) -> u64 {
        self.bytes.get(Self::slot(class)).copied().unwrap_or(0)
    }
}

/// Why an operation was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resource {
    /// The request bucket.
    Requests,
    /// The byte bucket.
    Bytes,
}

impl std::fmt::Display for Resource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Requests => "blob requests",
            Self::Bytes => "blob bytes",
        })
    }
}

#[derive(Debug)]
struct Tenant {
    requests: Bucket,
    bytes: Bucket,
    usage: Usage,
}

/// Token buckets and usage, per tenant.
///
/// Shared by every [`Metered`] wrapping the same node's store, so a tenant's quota is one
/// quota however many handles reach it.
#[derive(Debug)]
pub struct Meter {
    quota: Quota,
    tenants: Mutex<HashMap<TenantId, Tenant>>,
}

impl Meter {
    /// A meter applying `quota` to every tenant.
    #[must_use]
    pub fn new(quota: Quota) -> Self {
        Self {
            quota,
            tenants: Mutex::new(HashMap::new()),
        }
    }

    fn with<T>(&self, tenant: TenantId, f: impl FnOnce(&mut Tenant) -> T) -> T {
        let mut g = self
            .tenants
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let t = g.entry(tenant).or_insert_with(|| Tenant {
            requests: Bucket::new(self.quota.requests),
            bytes: Bucket::new(self.quota.bytes),
            usage: Usage::default(),
        });
        f(t)
    }

    /// Reserves `n` requests for one operation, all or nothing.
    ///
    /// ⚠️ **`n` is the *coalesced fetch* count, not the range count.** `get_ranges` and
    /// `get_ranges_as` coalesce before dispatching and issue `plan.len()` requests, so
    /// reserving the range count would refuse fan-outs that were inside the quota and would
    /// disagree with `Accounted` beneath on every merged read.
    pub fn reserve_requests(&self, tenant: TenantId, n: u64, now: Duration) -> bool {
        #[expect(
            clippy::cast_precision_loss,
            reason = "a request count is exact in f64 far past any plausible fan-out"
        )]
        let n = n as f64;
        self.with(tenant, |t| t.requests.take(n, now))
    }

    /// Whether the tenant's byte balance is positive — the byte bucket's admission test.
    pub fn bytes_in_credit(&self, tenant: TenantId, now: Duration) -> bool {
        self.with(tenant, |t| t.bytes.in_credit(now))
    }

    /// Records what an operation actually did, debiting bytes and counting usage.
    pub fn settle(
        &self,
        tenant: TenantId,
        class: OpClass,
        requests: u64,
        bytes: u64,
        now: Duration,
    ) {
        let slot = Usage::slot(class);
        #[expect(
            clippy::cast_precision_loss,
            reason = "a byte count is exact in f64 to 2^53, far past one operation"
        )]
        let b = bytes as f64;
        self.with(tenant, |t| {
            t.bytes.debit(b, now);
            if let Some(r) = t.usage.requests.get_mut(slot) {
                *r = r.saturating_add(requests);
            }
            if let Some(x) = t.usage.bytes.get_mut(slot) {
                *x = x.saturating_add(bytes);
            }
        });
    }

    /// What this node has seen the tenant spend.
    #[must_use]
    pub fn usage(&self, tenant: TenantId) -> Usage {
        self.with(tenant, |t| t.usage)
    }

    /// The tenant's remaining request and byte balances, for a test or a `meta` field.
    #[must_use]
    pub fn balances(&self, tenant: TenantId) -> (f64, f64) {
        self.with(tenant, |t| (t.requests.balance(), t.bytes.balance()))
    }
}
