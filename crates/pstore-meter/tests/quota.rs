//! M6b's criteria: what a quota admits, what it refuses, and what it bills.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::float_cmp,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

use bytes::Bytes;
use pstore_blob::{Accounted, BlobStore, Class, Key, MemoryStore, OpClass, Precondition};
use pstore_meter::{Meter, Metered, Quota, Rate, Usage};
use pstore_types::TenantId;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const A: TenantId = TenantId(1);
const B: TenantId = TenantId(2);
const KEY: &str = "seg/0001";

/// A clock a test moves by hand.
///
/// ⚠️ The whole reason `Bucket` is told the time: reading `Instant::now()` would make the
/// test's own duration an input, so "refills at the configured rate" would pass on a fast
/// machine and flake on a loaded one.
#[derive(Clone, Default)]
struct Clock(Arc<Mutex<Duration>>);

impl Clock {
    fn advance(&self, d: Duration) {
        if let Ok(mut t) = self.0.lock() {
            *t += d;
        }
    }
    fn f(&self) -> Arc<dyn Fn() -> Duration + Send + Sync> {
        let inner = Arc::clone(&self.0);
        Arc::new(move || inner.lock().map(|t| *t).unwrap_or_else(|e| *e.into_inner()))
    }
}

fn rate(per_sec: f64, burst: f64) -> Rate {
    Rate { per_sec, burst }
}

struct Fixture {
    acc: Accounted<MemoryStore>,
    meter: Arc<Meter>,
    clock: Clock,
}

impl Fixture {
    /// A store seeded with one 4 KiB object, and a meter with `quota`.
    async fn new(quota: Quota, gap: u64) -> Self {
        let acc = Accounted::new(MemoryStore::with_coalesce_gap(gap));
        // ⚠️ Seeded as a THIRD tenant. `Accounted`'s counters are per tenant and never reset,
        // so seeding as A would leave A's ledger holding a write the meter never saw — and
        // `the_meter_and_the_accountant_agree` would be comparing the fixture, not the code.
        acc.as_tenant(TenantId(999))
            .put(&Key::new(KEY), Bytes::from(vec![7u8; 4096]))
            .await
            .unwrap();
        Self {
            acc,
            meter: Arc::new(Meter::new(quota)),
            clock: Clock::default(),
        }
    }

    fn store(&self, t: TenantId) -> Metered<pstore_blob::TenantView<MemoryStore>> {
        Metered::new(
            Arc::new(self.acc.as_tenant(t)),
            Arc::clone(&self.meter),
            t,
            self.clock.f(),
        )
    }
}

fn quota(requests: Rate, bytes: Rate) -> Quota {
    Quota { requests, bytes }
}

#[tokio::test]
async fn a_tenant_inside_its_quota_is_untouched() {
    let f = Fixture::new(quota(rate(1e9, 1e9), rate(1e12, 1e12)), 0).await;
    let s = f.store(A);
    let k = Key::new(KEY);
    let before = f.acc.count(A, OpClass::Read);

    assert_eq!(s.get(&k).await.unwrap().len(), 4096);
    assert_eq!(&s.get_range(&k, 8..16).await.unwrap()[..], &[7u8; 8]);
    assert_eq!(s.get_suffix(&k, 4).await.unwrap().len(), 4);
    assert_eq!(s.head(&k).await.unwrap(), 4096);
    assert!(s.get_tag(&k).await.is_some());
    assert_eq!(s.get_immutable(&k, Class::Meta).await.unwrap().len(), 4096);

    // Six operations, six requests underneath: the meter neither batches nor drops.
    assert_eq!(f.acc.count(A, OpClass::Read) - before, 6);
}

#[tokio::test]
async fn an_exhausted_tenant_is_refused_by_name() {
    let f = Fixture::new(quota(rate(0.0, 2.0), rate(1e12, 1e12)), 0).await;
    let s = f.store(A);
    let k = Key::new(KEY);
    assert!(s.get(&k).await.is_ok());
    assert!(s.get(&k).await.is_ok());

    let err = s
        .get(&k)
        .await
        .expect_err("the third must be refused")
        .to_string();
    assert!(err.contains("429"), "{err}");
    assert!(err.contains("tenant 1"), "{err}");
    assert!(err.contains("blob requests"), "{err}");
}

#[tokio::test]
async fn one_tenants_quota_is_not_anothers() {
    // ⚠️ A single global bucket passes every single-tenant test in this file.
    let f = Fixture::new(quota(rate(0.0, 2.0), rate(1e12, 1e12)), 0).await;
    let (a, b) = (f.store(A), f.store(B));
    let k = Key::new(KEY);
    assert!(a.get(&k).await.is_ok());
    assert!(a.get(&k).await.is_ok());
    assert!(a.get(&k).await.is_err());
    assert!(b.get(&k).await.is_ok(), "B was charged for A's traffic");
    assert!(b.get(&k).await.is_ok());
}

#[tokio::test]
async fn a_fan_out_that_would_exceed_the_quota_issues_nothing() {
    // ⚠️ **Both entry points.** The dense read path uses the unhinted `get_ranges`; only text,
    // sparse and the cache use `get_ranges_as`. A meter that overrode one would reserve per
    // range inside the loop on the other -- and criterion 2 would still pass, because
    // *something* gets refused. Zero requests is the property; "refused" is not.
    for hinted in [false, true] {
        let f = Fixture::new(quota(rate(0.0, 4.0), rate(1e12, 1e12)), 0).await;
        let s = f.store(A);
        let k = Key::new(KEY);
        let ranges: Vec<_> = (0..8u64).map(|i| i * 64..i * 64 + 8).collect();
        let before = f.acc.count(A, OpClass::Read);

        let refused = if hinted {
            s.get_ranges_as(&k, &ranges, Class::Meta).await.is_err()
        } else {
            s.get_ranges(&k, &ranges).await.is_err()
        };
        assert!(
            refused,
            "hinted={hinted}: a fan-out over quota was admitted"
        );
        assert_eq!(
            f.acc.count(A, OpClass::Read) - before,
            0,
            "hinted={hinted}: requests were issued by a refused fan-out"
        );
    }
}

#[tokio::test]
async fn a_coalesced_fan_out_reserves_what_it_will_actually_issue() {
    // ⚠️ At gap 0 the plan length equals the range count, so that fixture cannot tell
    // "reserve the plan" from "reserve the range count". At a merging gap they differ: eight
    // nearby ranges are ONE request, and a meter reserving eight would refuse a fan-out that
    // was comfortably inside the quota.
    let f = Fixture::new(quota(rate(0.0, 4.0), rate(1e12, 1e12)), 1024).await;
    let s = f.store(A);
    let k = Key::new(KEY);
    let ranges: Vec<_> = (0..8u64).map(|i| i * 64..i * 64 + 8).collect();
    let before = f.acc.count(A, OpClass::Read);

    s.get_ranges(&k, &ranges)
        .await
        .expect("eight ranges that coalesce into one must fit a quota of four");
    assert_eq!(
        f.acc.count(A, OpClass::Read) - before,
        1,
        "the fixture must actually coalesce, or it proves nothing"
    );
    let (requests, _) = f.meter.balances(A);
    assert_eq!(requests, 3.0, "the meter charged more than it issued");
}

#[tokio::test]
async fn tokens_refill_at_the_configured_rate() {
    let f = Fixture::new(quota(rate(4.0, 4.0), rate(1e12, 1e12)), 0).await;
    let s = f.store(A);
    let k = Key::new(KEY);
    for _ in 0..4 {
        assert!(s.get(&k).await.is_ok());
    }
    assert!(s.get(&k).await.is_err(), "the burst was not exhausted");

    f.clock.advance(Duration::from_secs(1));
    for i in 0..4 {
        assert!(s.get(&k).await.is_ok(), "refill admitted only {i}");
    }
    assert!(
        s.get(&k).await.is_err(),
        "one second refilled more than the rate"
    );
}

#[tokio::test]
async fn a_long_idle_does_not_admit_more_than_the_burst() {
    // An unbounded accumulator is a tenant idle overnight and then issuing a million requests
    // in one instant, which is the thing a burst exists to price.
    let f = Fixture::new(quota(rate(4.0, 4.0), rate(1e12, 1e12)), 0).await;
    let s = f.store(A);
    let k = Key::new(KEY);
    f.clock.advance(Duration::from_secs(3600));
    for i in 0..4 {
        assert!(
            s.get(&k).await.is_ok(),
            "only {i} admitted after an hour idle"
        );
    }
    assert!(
        s.get(&k).await.is_err(),
        "an hour of idling admitted more than the burst"
    );
}

#[tokio::test]
async fn bytes_and_requests_are_separate_buckets() {
    // Requests are plentiful, bytes are not.
    let f = Fixture::new(quota(rate(1e9, 1e9), rate(0.0, 1000.0)), 0).await;
    let s = f.store(A);
    let k = Key::new(KEY);

    // 4 KiB against a 1,000-byte balance: admitted (the balance was positive), and it overruns.
    assert!(s.get(&k).await.is_ok());
    let err = s
        .get(&k)
        .await
        .expect_err("bytes must now refuse")
        .to_string();
    assert!(err.contains("blob bytes"), "{err}");
    assert!(!err.contains("blob requests"), "{err}");
}

#[tokio::test]
async fn an_overrun_is_still_owed_on_the_next_operation() {
    // ⚠️ The bound. A bucket that saturated at zero would forgive the overrun, and a tenant
    // could repeat "one giant read per refill tick" forever -- passing every other test here
    // while the byte quota bounded nothing over time.
    let f = Fixture::new(quota(rate(1e9, 1e9), rate(1000.0, 1000.0)), 0).await;
    let s = f.store(A);
    let k = Key::new(KEY);
    assert!(s.get(&k).await.is_ok());

    let (_, bytes) = f.meter.balances(A);
    assert!(bytes < 0.0, "the overrun was forgiven: balance {bytes}");

    // One second refills 1,000 of the ~3,096 owed, so it is still in debt.
    f.clock.advance(Duration::from_secs(1));
    assert!(
        s.get(&k).await.is_err(),
        "a debt of 3,096 cleared in one second"
    );

    // Four more seconds clear it.
    f.clock.advance(Duration::from_secs(4));
    assert!(s.get(&k).await.is_ok(), "the debt never cleared");
}

#[tokio::test]
async fn a_short_read_bills_what_it_returned() {
    let f = Fixture::new(quota(rate(1e9, 1e9), rate(1e12, 1e12)), 0).await;
    let s = f.store(A);
    // A suffix longer than the object returns the object, and that is what is billed.
    let got = s.get_suffix(&Key::new(KEY), 100_000).await.unwrap();
    assert_eq!(got.len(), 4096);
    assert_eq!(f.meter.usage(A).bytes_of(OpClass::Read), 4096);
}

#[tokio::test]
async fn a_refused_request_bills_nothing() {
    let f = Fixture::new(quota(rate(0.0, 1.0), rate(1e12, 1e12)), 0).await;
    let s = f.store(A);
    let k = Key::new(KEY);
    assert!(s.get(&k).await.is_ok());
    let after_one = f.meter.usage(A);
    assert!(s.get(&k).await.is_err());
    assert_eq!(
        f.meter.usage(A),
        after_one,
        "a refused request moved the usage counters"
    );
}

#[tokio::test]
async fn the_meter_and_the_accountant_agree_on_a_coalesced_fan_out() {
    // ⚠️ **The gap-0 fixture below cannot fail on the disagreement this exists to catch.**
    // At gap 0 every plan entry is exactly one requested range, so the sum of the slices
    // handed back and the sum of the merged buffers are the same number by construction --
    // and a meter billing the wrong one agrees with the accountant on every input the test
    // supplies. That is how a real 7x undercharge survived a 99.26%-region, 73-of-73-mutant
    // sweep. Criterion 3 already makes this argument for the request count; this is the same
    // argument for the byte count.
    let f = Fixture::new(quota(rate(1e9, 1e9), rate(1e12, 1e12)), 1024).await;
    let s = f.store(A);
    let k = Key::new(KEY);
    let ranges: Vec<_> = (0..8u64).map(|i| i * 64..i * 64 + 8).collect();

    let got = s.get_ranges(&k, &ranges).await.unwrap();
    assert_eq!(
        got.len(),
        8,
        "the caller must still get its own ranges back"
    );
    assert!(got.iter().all(|b| b.len() == 8));

    assert_eq!(
        f.acc.count(A, OpClass::Read),
        1,
        "the fixture must actually coalesce, or it proves nothing"
    );
    assert_eq!(
        f.meter.usage(A).bytes_of(OpClass::Read),
        f.acc.bytes(A, OpClass::Read),
        "the meter billed the slices it handed back, not the bytes that crossed the wire"
    );
    // And that is materially more than the caller asked for: 8 x 8 bytes requested, one
    // merged span actually moved.
    assert!(
        f.meter.usage(A).bytes_of(OpClass::Read) > 64,
        "the merged span was billed as if nothing between the ranges moved"
    );
}

#[tokio::test]
async fn a_byte_refusal_costs_no_request_tokens() {
    // ⚠️ The other direction of "the buckets are independent", and the one the existing test
    // could not see because it set the request rate to 1e9. Reserving requests before
    // discovering the byte quota is gone spends tokens on operations that never issue a
    // request -- so a client retrying against a byte quota drains its own request bucket and
    // is then refused for the wrong resource entirely.
    let f = Fixture::new(quota(rate(0.0, 10.0), rate(0.0, 1.0)), 0).await;
    let s = f.store(A);
    let k = Key::new(KEY);
    assert!(s.get(&k).await.is_ok(), "the first read is in credit");
    let (requests_after_one, bytes) = f.meter.balances(A);
    assert!(bytes < 0.0, "the fixture must put bytes into debt");

    for _ in 0..9 {
        let err = s
            .get(&k)
            .await
            .expect_err("bytes are exhausted")
            .to_string();
        assert!(err.contains("blob bytes"), "{err}");
    }
    let (requests, _) = f.meter.balances(A);
    assert_eq!(
        requests, requests_after_one,
        "nine byte-refusals spent request tokens on requests that were never issued"
    );
}

#[tokio::test]
async fn the_meter_and_the_accountant_agree() {
    // ⚠️ Scoped to operations that SUCCEEDED. On a fan-out that fails partway `try_join_all`
    // returns Err while the fetches that did return are already billed underneath, so the two
    // legitimately differ on the error path.
    let f = Fixture::new(quota(rate(1e9, 1e9), rate(1e12, 1e12)), 0).await;
    let s = f.store(A);
    let k = Key::new(KEY);
    s.get(&k).await.unwrap();
    s.get_range(&k, 0..100).await.unwrap();
    s.get_suffix(&k, 10).await.unwrap();
    s.head(&k).await.unwrap();
    s.get_tag(&k).await.unwrap();
    s.get_ranges(&k, &[0..8, 2000..2008]).await.unwrap();
    s.put(&Key::new("other"), Bytes::from_static(b"xyz"))
        .await
        .unwrap();
    s.put_conditional(
        &Key::new("cas"),
        Bytes::from_static(b"ab"),
        Precondition::NotExists,
    )
    .await
    .unwrap();
    s.delete_batch(&[Key::new("other")]).await.unwrap();
    s.list_unrestricted(&Key::new("seg")).await.unwrap();

    let u: Usage = f.meter.usage(A);
    for class in [
        OpClass::Read,
        OpClass::Write,
        OpClass::Delete,
        OpClass::List,
    ] {
        assert_eq!(
            u.requests_of(class),
            f.acc.count(A, class),
            "{class:?} request counts disagree"
        );
        assert_eq!(
            u.bytes_of(class),
            f.acc.bytes(A, class),
            "{class:?} byte counts disagree"
        );
    }
}

#[tokio::test]
async fn get_tag_is_counted_and_never_refused() {
    // ⚠️ It returns `Option` with no error channel, so a refusal could only be `None` -- which
    // means "the object is absent" on the CAS rebase step, turning a quota refusal into a
    // create-if-absent against an object that exists. A silent wrong answer.
    let f = Fixture::new(quota(rate(0.0, 0.0), rate(0.0, 0.0)), 0).await;
    let s = f.store(A);
    assert!(
        s.get(&Key::new(KEY)).await.is_err(),
        "the quota is not exhausted"
    );
    assert!(
        s.get_tag(&Key::new(KEY)).await.is_some(),
        "get_tag was refused, which reads as 'absent' to a committer"
    );
    assert!(
        f.meter.usage(A).requests_of(OpClass::Read) > 0,
        "it was not counted"
    );
}

#[tokio::test]
async fn every_method_is_metered_and_every_method_refuses() {
    // ⚠️ **The hole this crate exists to close, stated as a test.** A method that forgets to
    // call `admit` is unmetered, and nothing else here would notice: every other test drives
    // `get`. A method added to `BlobStore` later and forwarded without a reservation is the
    // same bug, and this is what catches it.
    //
    // `get_tag` is the one exception and it is deliberate — see its doc comment.
    let f = Fixture::new(quota(rate(0.0, 0.0), rate(0.0, 0.0)), 0).await;
    let s = f.store(A);
    let k = Key::new(KEY);

    assert!(s.get(&k).await.is_err(), "get");
    assert!(s.get_range(&k, 0..8).await.is_err(), "get_range");
    assert!(s.get_suffix(&k, 8).await.is_err(), "get_suffix");
    assert!(s.get_with_tag(&k).await.is_err(), "get_with_tag");
    assert!(s.head(&k).await.is_err(), "head");
    assert!(
        s.get_range_as(&k, 0..8, Class::Meta).await.is_err(),
        "get_range_as"
    );
    assert!(
        s.get_suffix_as(&k, 8, Class::Meta).await.is_err(),
        "get_suffix_as"
    );
    assert!(
        s.get_immutable(&k, Class::Meta).await.is_err(),
        "get_immutable"
    );
    assert!(
        s.get_ranges(&k, &[0..8, 64..72]).await.is_err(),
        "get_ranges"
    );
    assert!(
        s.get_ranges_as(&k, &[0..8, 64..72], Class::Meta)
            .await
            .is_err(),
        "get_ranges_as"
    );
    assert!(s.put(&k, Bytes::from_static(b"x")).await.is_err(), "put");
    assert!(
        s.delete_batch(std::slice::from_ref(&k)).await.is_err(),
        "delete_batch"
    );
    assert!(s.list_unrestricted(&Key::new("seg")).await.is_err(), "list");

    // ⚠️ `put_conditional` refuses with `CasError::Io`, never `Contended`: every caller in the
    // tree treats `Contended` as "retry the same attempt unchanged", which would turn one
    // quota refusal into a retry storm against the quota.
    let e = s
        .put_conditional(&k, Bytes::from_static(b"x"), Precondition::NotExists)
        .await
        .expect_err("put_conditional");
    assert!(matches!(e, pstore_blob::CasError::Io(_)), "{e:?}");
    assert!(e.to_string().contains("429"), "{e}");
    assert!(
        !e.should_rebase(),
        "a quota refusal asked the caller to rebase"
    );

    // Nothing reached the store.
    for class in [
        OpClass::Read,
        OpClass::Write,
        OpClass::Delete,
        OpClass::List,
    ] {
        assert_eq!(f.acc.count(A, class), 0, "{class:?} escaped the meter");
    }
}

#[tokio::test]
async fn every_method_costs_exactly_one_request() {
    // The other direction: metered, and metered once. A method that reserved twice would halve
    // every tenant's quota silently.
    let f = Fixture::new(quota(rate(1e9, 1e9), rate(1e12, 1e12)), 0).await;
    let s = f.store(A);
    let k = Key::new(KEY);
    let ops: Vec<(&str, u64)> = vec![
        ("get", 1),
        ("get_range", 1),
        ("get_suffix", 1),
        ("get_with_tag", 1),
        ("head", 1),
        ("get_range_as", 1),
        ("get_suffix_as", 1),
        ("get_immutable", 1),
    ];
    for (name, want) in ops {
        let before = f.meter.usage(A).requests_of(OpClass::Read);
        match name {
            "get" => drop(s.get(&k).await.unwrap()),
            "get_range" => drop(s.get_range(&k, 0..8).await.unwrap()),
            "get_suffix" => drop(s.get_suffix(&k, 8).await.unwrap()),
            "get_with_tag" => drop(s.get_with_tag(&k).await.unwrap()),
            "head" => drop(s.head(&k).await.unwrap()),
            "get_range_as" => drop(s.get_range_as(&k, 0..8, Class::Meta).await.unwrap()),
            "get_suffix_as" => drop(s.get_suffix_as(&k, 8, Class::Meta).await.unwrap()),
            _ => drop(s.get_immutable(&k, Class::Meta).await.unwrap()),
        }
        assert_eq!(
            f.meter.usage(A).requests_of(OpClass::Read) - before,
            want,
            "{name} was charged wrongly"
        );
    }
}

#[tokio::test]
async fn an_unlimited_quota_is_transparent() {
    // ⚠️ The safety net for putting this decorator in a stack: until someone sets a number it
    // must change nothing. `Quota::unlimited()` is the default a composition root would use,
    // and a bucket that treated an infinite rate as "zero tokens" would refuse everything —
    // which is the worst possible failure for a component whose job is to be invisible.
    let f = Fixture::new(Quota::unlimited(), 0).await;
    let s = f.store(A);
    let k = Key::new(KEY);
    for _ in 0..200 {
        assert_eq!(s.get(&k).await.unwrap().len(), 4096);
    }
    let ranges: Vec<_> = (0..8u64).map(|i| i * 64..i * 64 + 8).collect();
    let got = s.get_ranges_as(&k, &ranges, Class::Meta).await.unwrap();
    assert_eq!(got.len(), 8);
    assert!(got.iter().all(|b| b.len() == 8));
    assert_eq!(f.meter.usage(A).bytes_of(OpClass::Read), 200 * 4096 + 64);

    // The balances never move off full, however long the clock runs.
    f.clock.advance(Duration::from_secs(86_400));
    assert!(s.get(&k).await.is_ok());
    let (requests, bytes) = f.meter.balances(A);
    assert!(
        requests.is_infinite() && bytes.is_infinite(),
        "{requests} {bytes}"
    );
}

#[tokio::test]
async fn the_decorator_forwards_capabilities_and_names_its_tenant() {
    // `capabilities()` is not decoration here: `planned()` reads `coalesce_gap` through it, so
    // a decorator that reported a default profile would reserve the wrong number on every
    // fan-out.
    let f = Fixture::new(Quota::unlimited(), 4242).await;
    let s = f.store(B);
    assert_eq!(s.capabilities().coalesce_gap, 4242);
    // The clock is a closure with no `Debug`, so the impl is hand-written; what a reader needs
    // from it is which tenant they are looking at.
    assert!(format!("{s:?}").contains('2'), "{s:?}");
}

#[tokio::test]
async fn exactly_zero_credit_is_no_credit() {
    // ⚠️ The boundary, and the only input on which `> 0.0` and `>= 0.0` differ. A tenant whose
    // byte balance lands *exactly* on zero has spent everything it was given, and "in credit"
    // has to mean it has something left — otherwise the last operation of every budget is free
    // and a quota of N admits N + 1.
    //
    // Constructible rather than contrived: a burst of exactly one object's size, spent on one
    // read of it. `rate` is zero so nothing refills underneath the assertion.
    let f = Fixture::new(quota(rate(1e9, 1e9), rate(0.0, 4096.0)), 0).await;
    let s = f.store(A);
    let k = Key::new(KEY);

    assert!(
        s.get(&k).await.is_ok(),
        "the first read fits the budget exactly"
    );
    let (_, bytes) = f.meter.balances(A);
    assert_eq!(
        bytes, 0.0,
        "the fixture must land exactly on zero, not near it"
    );

    let err = s
        .get(&k)
        .await
        .expect_err("zero credit must refuse")
        .to_string();
    assert!(err.contains("blob bytes"), "{err}");
}

#[tokio::test]
async fn a_fan_out_whose_fetch_fails_is_an_error_and_bills_no_bytes() {
    // ⚠️ The error path criterion 8 is scoped around, made explicit. `try_join_all` gives the
    // meter no successful buffers to bill, so it debits nothing — while `Accounted` beneath
    // has already billed whatever did return. The two legitimately differ here, and the
    // requests are still charged because they were issued.
    // ⚠️ Ordinal **0**, not 1: `Flaky` wraps a store with the default 64 KiB gap, so these
    // four ranges coalesce into ONE fetch and there is no ordinal 1 to refuse.
    let store = Arc::new(pstore_testkit::flaky::Flaky::refusing_reads_at(&[0]));
    store
        .put(&Key::new(KEY), Bytes::from(vec![7u8; 4096]))
        .await
        .unwrap();
    let meter = Arc::new(Meter::new(quota(rate(1e9, 1e9), rate(1e12, 1e12))));
    let clock = Clock::default();
    let s = Metered::new(Arc::clone(&store), Arc::clone(&meter), A, clock.f());

    let ranges: Vec<_> = (0..4u64).map(|i| i * 1000..i * 1000 + 8).collect();
    assert!(
        s.get_ranges(&Key::new(KEY), &ranges).await.is_err(),
        "a failed fetch must fail the fan-out"
    );
    assert_eq!(
        meter.usage(A).bytes_of(OpClass::Read),
        0,
        "bytes were billed for a fan-out that returned nothing"
    );
    assert!(
        meter.usage(A).requests_of(OpClass::Read) > 0,
        "the requests were issued and must still be charged"
    );
}

#[tokio::test]
async fn a_short_coalesced_fetch_is_an_error_not_a_panic() {
    // ⚠️ The trait's own default slices the merged buffer by index, which **panics** if a
    // backend returns fewer bytes than the span — and HTTP permits exactly that (`206` with
    // the bytes that exist), which is the divergence M0a's conformance suite found on its
    // first run against a foreign backend. Re-implementing the fan-out here meant
    // re-implementing that slicing, so it is checked rather than indexed.
    let store = Arc::new(pstore_testkit::broken::Broken::new(
        pstore_testkit::broken::Defect::ShortReadsPastTheEnd,
    ));
    store
        .put(&Key::new(KEY), Bytes::from(vec![7u8; 64]))
        .await
        .unwrap();
    let meter = Arc::new(Meter::new(Quota::unlimited()));
    let clock = Clock::default();
    let s = Metered::new(Arc::clone(&store), meter, A, clock.f());

    // Ranges that run past the end: the backend answers short instead of refusing.
    let err = s
        .get_ranges(&Key::new(KEY), &[0..8, 900..2000])
        .await
        .expect_err("a short coalesced fetch must be an error");
    assert!(err.to_string().contains("short of"), "{err}");
}
