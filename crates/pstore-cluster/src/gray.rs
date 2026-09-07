//! Gray failure: an availability zone degraded but alive, with every liveness check green.
//!
//! ⚠️ **Per-AZ cells caused this blind spot.** D-79 removes cross-AZ traffic, and eliminating
//! traffic eliminates the signal that would let one zone observe another. These decisions are
//! the bill for that, and `gray-failure.md` puts the exposure at **17–67× effective latency**
//! while every health check stays green.
//!
//! Everything here is a pure function of a set of health samples. The transports that would
//! gather them — the cross-AZ probe mesh (D-82) and the blob-store health bulletin (D-83) —
//! are deliberately not here: they need a query path and a real multi-AZ deployment to mean
//! anything, and the decisions are the part that is dangerous to get wrong.

use std::collections::HashSet;

/// How much worse than its peers a zone must be to count as an outlier: **50% slower**, or
/// 5 percentage points less successful.
///
/// ⚠️ **Not Envoy's `mean − stdev × 1.9`, and the reason is arithmetic.** D-84 adopts that
/// default, but it is tuned for a fleet of many hosts. With *n* samples the largest z-score
/// any one of them can reach is `(n − 1) / √n` — for **three zones that is 1.155**, so a
/// factor of 1.9 can never fire, whatever the degradation. Implemented as specified, gray
/// failure would have been undetectable at exactly the fleet shape D-79 prescribes.
///
/// Excluding the candidate from its own baseline fixes the masking but not the sensitivity:
/// with two peers the standard deviation is nearly meaningless, and a zone 4% slower than its
/// two neighbours becomes a 3σ event. So the test is a **relative margin against the peer
/// median** — robust to two peers, and scale-invariant, which is what keeps it peer-relative
/// in D-84's sense rather than an absolute threshold in disguise.
pub const DEFAULT_MARGIN: f64 = 0.5;

/// Success-rate margin, in absolute terms — rates live in a narrow band near 1.0 where a
/// relative margin says nothing useful.
const SUCCESS_MARGIN: f64 = 0.05;

/// Utilization above which degradation is assumed to be load rather than infrastructure.
///
/// ⚠️ The whole of D-85 turns on this line. Below it, a slow zone is slow while doing little
/// work — infrastructure. Above it, the zone is slow *because* of what it is being asked to
/// do, and draining it moves that load onto its peers.
const BUSY: f64 = 0.75;

/// Headroom a survivor must have before it can be asked to absorb a drained zone's share.
const SURVIVOR_HEADROOM: f64 = 0.80;

/// One zone's health, as its peers see it.
#[derive(Debug, Clone, PartialEq)]
pub struct ZoneHealth {
    /// Which zone.
    pub zone: String,
    /// Fraction of requests that succeeded, 0..=1.
    pub success_rate: f64,
    /// Observed latency.
    pub latency_ms: f64,
    /// How hard the zone is working, 0..=1. ⚠️ Load-bearing: it is what separates a zone that
    /// is slow from a zone that is busy.
    pub utilization: f64,
}

/// What to do about a zone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Nothing.
    Healthy,
    /// Degraded, but under load — shed work, never drain.
    Shed,
    /// Degraded while doing little work, and the survivors can take it.
    Drain,
}

/// The median of a slice, which is what a two-peer baseline needs: one bad peer cannot drag
/// it the way it drags a mean.
fn median(xs: &mut [f64]) -> f64 {
    xs.sort_by(f64::total_cmp);
    let n = xs.len();
    if n == 0 {
        return 0.0;
    }
    if n % 2 == 1 {
        xs.get(n / 2).copied().unwrap_or(0.0)
    } else {
        let (a, b) = (
            xs.get(n / 2 - 1).copied().unwrap_or(0.0),
            xs.get(n / 2).copied().unwrap_or(0.0),
        );
        f64::midpoint(a, b)
    }
}

/// Zones that are outliers against their peers **right now**.
///
/// ⚠️ Peer-relative and self-calibrating (D-84), never "slower than 100 ms". A fixed threshold
/// fires on a fleet that is uniformly busy — a Tuesday — and drains a zone for being normal.
/// Scale every zone's latency by ten and this returns exactly the same answer.
///
/// Both signals count: a zone can answer quickly and wrongly.
#[must_use]
pub fn outliers(samples: &[ZoneHealth], margin: f64) -> Vec<&str> {
    // ⚠️ Fewer than three zones cannot support an outlier test: with two, whichever is worse
    // is always "the outlier", and the fleet drains half of itself on noise.
    if samples.len() < 3 {
        return Vec::new();
    }
    samples
        .iter()
        .filter(|me| {
            // ⚠️ The baseline EXCLUDES the candidate. Including it lets a badly degraded zone
            // drag the very statistic meant to catch it — the masking that makes an outlier
            // invisible in proportion to how bad it is.
            let mut lat: Vec<f64> = samples
                .iter()
                .filter(|z| z.zone != me.zone)
                .map(|z| z.latency_ms)
                .collect();
            let mut ok: Vec<f64> = samples
                .iter()
                .filter(|z| z.zone != me.zone)
                .map(|z| z.success_rate)
                .collect();
            let peer_lat = median(&mut lat);
            let peer_ok = median(&mut ok);
            me.latency_ms > peer_lat * (1.0 + margin) || me.success_rate < peer_ok - SUCCESS_MARGIN
        })
        .map(|z| z.zone.as_str())
        .collect()
}

/// What to do about one zone.
///
/// ⚠️ The order of the guards is the safety argument, so it is written out:
///
/// 1. not an outlier → `Healthy`;
/// 2. **a majority of the fleet is degraded** → `Shed`. Two of three zones bad is a fleet-wide
///    event, and the answer to a fleet-wide event is not to switch the fleet off;
/// 3. **the zone is busy** → `Shed`. Degradation that rises with utilization is load, and
///    draining an overloaded AZ moves that load onto its peers — the detector causing the
///    outage it was built to prevent (D-85);
/// 4. **the survivors have no headroom** → `Shed`. Draining into a fleet that cannot absorb
///    the share is the same cascade by another route;
/// 5. otherwise → `Drain`.
#[must_use]
pub fn verdict(samples: &[ZoneHealth], zone: &str, margin: f64) -> Verdict {
    let bad: HashSet<&str> = outliers(samples, margin).into_iter().collect();
    if !bad.contains(zone) {
        return Verdict::Healthy;
    }
    if bad.len() * 2 >= samples.len() {
        return Verdict::Shed;
    }
    let Some(me) = samples.iter().find(|z| z.zone == zone) else {
        return Verdict::Healthy;
    };
    if me.utilization >= BUSY {
        return Verdict::Shed;
    }
    let survivors_ready = samples
        .iter()
        .filter(|z| z.zone != zone)
        .all(|z| z.utilization < SURVIVOR_HEADROOM);
    if survivors_ready {
        Verdict::Drain
    } else {
        Verdict::Shed
    }
}

/// Whether this node should start failing its load-balancer health check.
///
/// ⚠️ Draining is **self-eviction from the load balancer** (D-86) — pure data plane, no API
/// call, no control plane in the recovery path, so static stability holds and it works with
/// any load balancer.
///
/// ⚠️ The verdict comes from what *external* observers say, never from the node's own opinion
/// of itself. That inversion is the whole point: differential observability is the definition
/// of gray failure, and a node's self-assessment is exactly what it defeats.
///
/// The one exception is D-87. A node that cannot reach the blob store is definitionally
/// useless — it is our only dependency — and it must not wait for a bulletin it cannot fetch,
/// which is a deadlock that arrives precisely when it matters.
#[must_use]
pub fn should_self_evict(blob_reachable: bool, my_zone: Verdict) -> bool {
    !blob_reachable || my_zone == Verdict::Drain
}
