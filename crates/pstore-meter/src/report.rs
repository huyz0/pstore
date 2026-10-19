//! Each node's monthly usage report, and the rollup that sums them (M109).
//!
//! ⚠️ **Requests scale with nodes, never with tenants.** A node writes one object per month
//! holding every tenant it served, and a rollup reads one per node. A report's *bytes* grow
//! with the tenants a node served, which is the trade the design rules allow.
//!
//! ⚠️ **The month has its own index, because the roster is the wrong list.** The roster names
//! the members now. A node that served half the month and died is not in it, and a node that
//! dies is the one most likely to have failed requests. So a node adds its id to the month's
//! index the first time it reports, and the rollup reads the index, then the reports: two
//! rounds, no LIST.
//!
//! ⚠️ **Sharded [`INDEX_SHARDS`] ways**, because every node registers at the start of every
//! month, and one object would put the whole fleet on one CAS in the same minute.
//!
//! ⚠️ **Not guarded by the fencing profile**, as the roster's CAS is not. This is telemetry: a
//! backend that ignores a precondition costs a miscount, never tenant data.

use crate::{Meter, Usage};
use pstore_blob::{BlobError, BlobStore, CasError, Key, Precondition};
use pstore_types::{CasTag, MAX_CAS_ATTEMPTS, TenantId};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::time::Duration;

/// How many objects a month's index is spread over.
///
/// At 10,000 nodes that is about 156 per shard, contending once a month, and a rollup reads
/// all of them in one round whatever the fleet's size.
pub const INDEX_SHARDS: u32 = 64;

const REPORT_MAGIC: &[u8; 4] = b"PSU1";
const INDEX_MAGIC: &[u8; 4] = b"PSI1";

/// What can go wrong reporting or rolling up.
#[derive(Debug, thiserror::Error)]
pub enum ReportError {
    /// The blob store refused.
    #[error("blob store: {0}")]
    Blob(#[from] BlobError),
    /// An object did not decode. Never skipped: a rollup that quietly drops a node's report
    /// is a budget that quietly under-reports.
    #[error("usage object at {0} is malformed")]
    Corrupt(String),
    /// The node's report changed under this reporter: another process holds the same node id.
    /// Refused rather than overwritten, because overwriting drops the other one's counts.
    #[error("node {0}'s report was written by another process with the same id")]
    Contended(String),
    /// The month's index shard stayed contended. The report is written; the next publish
    /// retries the registration.
    #[error("index shard {0} stayed contended for {MAX_CAS_ATTEMPTS} attempts")]
    IndexContended(u32),
}

/// A calendar month, in UTC.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Month {
    /// The year.
    pub year: i64,
    /// 1 to 12.
    pub month: u8,
}

impl Month {
    /// The month `since_epoch` falls in.
    ///
    /// Howard Hinnant's `civil_from_days`, which needs no calendar table and is exact over
    /// the proleptic Gregorian calendar.
    #[must_use]
    pub fn of(since_epoch: Duration) -> Self {
        let days = i64::try_from(since_epoch.as_secs() / 86_400).unwrap_or(i64::MAX);
        let z = days + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z.rem_euclid(146_097);
        let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let month = if mp < 10 { mp + 3 } else { mp - 9 };
        let year = yoe + era * 400 + i64::from(month <= 2);
        Self {
            year,
            month: u8::try_from(month).unwrap_or(0),
        }
    }

    fn label(self) -> String {
        format!("{:04}-{:02}", self.year, self.month)
    }
}

fn fnv(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, b| {
        (h ^ u64::from(*b)).wrapping_mul(0x0100_0000_01b3)
    })
}

/// A spread prefix and the identity, the shape `Roster::key` settled on: the prefix spreads
/// keys across the store's partitions, and the path keeps them distinct.
fn spread(path: &str) -> Key {
    #[expect(
        clippy::cast_possible_truncation,
        reason = "sixteen bits of spread is the intent; the path carries the identity"
    )]
    let s = fnv(path.as_bytes()) as u16;
    Key::new(format!("{s:04x}/{path}"))
}

fn report_key(month: Month, node: &str) -> Key {
    spread(&format!("use/{}/node/{node}", month.label()))
}

fn shard_of(node: &str) -> u32 {
    #[expect(
        clippy::cast_possible_truncation,
        reason = "reduced modulo INDEX_SHARDS first"
    )]
    let s = (fnv(node.as_bytes()) % u64::from(INDEX_SHARDS)) as u32;
    s
}

fn index_key(month: Month, shard: u32) -> Key {
    spread(&format!("use/{}/idx/{shard:02x}", month.label()))
}

struct Cur<'a> {
    b: &'a [u8],
    i: usize,
    what: &'a str,
}

impl Cur<'_> {
    fn take(&mut self, n: usize) -> Result<&[u8], ReportError> {
        let end = self.i.checked_add(n).filter(|e| *e <= self.b.len());
        let Some(end) = end else {
            return Err(ReportError::Corrupt(self.what.to_owned()));
        };
        let out = self.b.get(self.i..end).unwrap_or_default();
        self.i = end;
        Ok(out)
    }
    fn u64(&mut self) -> Result<u64, ReportError> {
        let mut a = [0u8; 8];
        a.copy_from_slice(self.take(8)?);
        Ok(u64::from_le_bytes(a))
    }
    fn u32(&mut self) -> Result<u32, ReportError> {
        let mut a = [0u8; 4];
        a.copy_from_slice(self.take(4)?);
        Ok(u32::from_le_bytes(a))
    }
    fn u128(&mut self) -> Result<u128, ReportError> {
        let mut a = [0u8; 16];
        a.copy_from_slice(self.take(16)?);
        Ok(u128::from_le_bytes(a))
    }
    fn magic(&mut self, m: &[u8; 4]) -> Result<(), ReportError> {
        if self.take(4)? == m {
            Ok(())
        } else {
            Err(ReportError::Corrupt(self.what.to_owned()))
        }
    }
    fn end(&self) -> Result<(), ReportError> {
        if self.i == self.b.len() {
            Ok(())
        } else {
            Err(ReportError::Corrupt(self.what.to_owned()))
        }
    }
}

fn encode_report(tenants: &BTreeMap<TenantId, Usage>) -> Vec<u8> {
    let mut out = REPORT_MAGIC.to_vec();
    out.extend(
        u32::try_from(tenants.len())
            .unwrap_or(u32::MAX)
            .to_le_bytes(),
    );
    for (t, u) in tenants {
        out.extend(t.0.to_le_bytes());
        for n in u.requests.iter().chain(&u.bytes) {
            out.extend(n.to_le_bytes());
        }
        out.extend(u.answered.to_le_bytes());
        out.extend(u.failed.to_le_bytes());
    }
    out
}

fn decode_report(b: &[u8], what: &str) -> Result<BTreeMap<TenantId, Usage>, ReportError> {
    let mut c = Cur { b, i: 0, what };
    c.magic(REPORT_MAGIC)?;
    let n = c.u32()?;
    let mut out = BTreeMap::new();
    for _ in 0..n {
        let t = TenantId(c.u128()?);
        let mut u = Usage::default();
        for r in u.requests.iter_mut().chain(u.bytes.iter_mut()) {
            *r = c.u64()?;
        }
        u.answered = c.u64()?;
        u.failed = c.u64()?;
        out.insert(t, u);
    }
    c.end()?;
    Ok(out)
}

fn encode_index(nodes: &BTreeSet<String>) -> Vec<u8> {
    let mut out = INDEX_MAGIC.to_vec();
    out.extend(u32::try_from(nodes.len()).unwrap_or(u32::MAX).to_le_bytes());
    for n in nodes {
        out.extend(u32::try_from(n.len()).unwrap_or(u32::MAX).to_le_bytes());
        out.extend(n.as_bytes());
    }
    out
}

fn decode_index(b: &[u8], what: &str) -> Result<BTreeSet<String>, ReportError> {
    let mut c = Cur { b, i: 0, what };
    c.magic(INDEX_MAGIC)?;
    let n = c.u32()?;
    let mut out = BTreeSet::new();
    for _ in 0..n {
        let len = usize::try_from(c.u32()?).unwrap_or(usize::MAX);
        let raw = c.take(len)?;
        let id = std::str::from_utf8(raw).map_err(|_| ReportError::Corrupt(what.to_owned()))?;
        out.insert(id.to_owned());
    }
    c.end()?;
    Ok(out)
}

/// A month's index shard, and the tag to condition its next write on. Absent is empty.
async fn read_index<S: BlobStore>(
    store: &S,
    month: Month,
    shard: u32,
) -> Result<(BTreeSet<String>, Option<CasTag>), ReportError> {
    let key = index_key(month, shard);
    match store.get_with_tag(&key).await {
        Ok((b, tag)) => Ok((decode_index(&b, key.as_str())?, Some(tag))),
        Err(BlobError::NotFound(_)) => Ok((BTreeSet::new(), None)),
        Err(e) => Err(e.into()),
    }
}

/// One node's view of what it reported this month.
#[derive(Debug)]
pub struct Reporter {
    node: String,
    month: Month,
    /// What the report held when `mark` was taken.
    base: BTreeMap<TenantId, Usage>,
    /// The meter's counts when `base` was taken. A report is `base + (meter − mark)`.
    mark: HashMap<TenantId, Usage>,
    /// The report's tag as this reporter last saw it. `None` is "not there".
    tag: Option<CasTag>,
    registered: bool,
    /// A write that answered `Io`, which may have landed: the report it held, the meter's
    /// counts it was taken at, and its bytes.
    uncertain: Option<Uncertain>,
}

#[derive(Debug)]
struct Uncertain {
    report: BTreeMap<TenantId, Usage>,
    mark: HashMap<TenantId, Usage>,
    bytes: Vec<u8>,
}

impl Reporter {
    /// Picks up `node`'s report for `month` where the last process left it. **One GET.**
    ///
    /// ⚠️ **Read back, not started over.** A node that restarted mid-month and wrote its
    /// counts from zero would overwrite everything its previous process reported.
    ///
    /// # Errors
    /// If the store refuses or the report is malformed.
    pub async fn resume<S: BlobStore>(
        store: &S,
        node: &str,
        month: Month,
        meter: &Meter,
    ) -> Result<Self, ReportError> {
        let key = report_key(month, node);
        let (base, tag) = match store.get_with_tag(&key).await {
            Ok((b, tag)) => (decode_report(&b, key.as_str())?, Some(tag)),
            Err(BlobError::NotFound(_)) => (BTreeMap::new(), None),
            Err(e) => return Err(e.into()),
        };
        Ok(Self {
            node: node.to_owned(),
            month,
            base,
            mark: meter.snapshot(),
            tag,
            registered: false,
            uncertain: None,
        })
    }

    /// Writes this node's report for `month`, and registers it in the month's index the first
    /// time.
    ///
    /// A month other than the last one published starts a new report from empty, and what
    /// the meter counted since the old month's last publish goes to the new month.
    ///
    /// # Errors
    /// [`ReportError::Contended`] if another process wrote this node's report;
    /// [`ReportError::IndexContended`] if the registration stayed contended, with the report
    /// already written.
    pub async fn publish<S: BlobStore>(
        &mut self,
        store: &S,
        meter: &Meter,
        month: Month,
    ) -> Result<(), ReportError> {
        self.resolve(store).await?;
        if month != self.month {
            self.month = month;
            self.base.clear();
            self.tag = None;
            self.registered = false;
        }
        let now = meter.snapshot();
        let mut report = self.base.clone();
        for (t, u) in &now {
            let since = *u - self.mark.get(t).copied().unwrap_or_default();
            let entry = report.entry(*t).or_default();
            *entry = *entry + since;
        }
        let pre = match self.tag.clone() {
            Some(t) => Precondition::Match(t),
            None => Precondition::NotExists,
        };
        let key = report_key(self.month, &self.node);
        let bytes = encode_report(&report);
        match store.put_conditional(&key, bytes.clone().into(), pre).await {
            Ok(out) => self.tag = Some(out.tag),
            Err(CasError::Lost | CasError::Contended) => {
                return Err(ReportError::Contended(self.node.clone()));
            }
            Err(CasError::Io(e)) => {
                // ⚠️ **It may have landed** (a timeout after the store committed it). Taken
                // as not landed, the next write's precondition is a tag the store has moved
                // past, and every publish after it is refused for the life of the process.
                self.uncertain = Some(Uncertain {
                    report,
                    mark: now,
                    bytes,
                });
                return Err(BlobError::Other(e).into());
            }
        }
        // Rebased only once the report has landed: a failed write must count again next time.
        self.base = report;
        self.mark = now;
        if !self.registered {
            self.register(store).await?;
        }
        Ok(())
    }

    /// Settles a write that answered `Io`. **One GET, and only after one.** The report holds
    /// that write's bytes if and only if it landed: two writes of one node's report differ
    /// unless the counts did not move, and then adopting either is the same report.
    async fn resolve<S: BlobStore>(&mut self, store: &S) -> Result<(), ReportError> {
        let Some(u) = self.uncertain.take() else {
            return Ok(());
        };
        let key = report_key(self.month, &self.node);
        match store.get_with_tag(&key).await {
            Ok((b, tag)) if b.as_ref() == u.bytes.as_slice() => {
                self.tag = Some(tag);
                self.base = u.report;
                self.mark = u.mark;
            }
            Ok(_) | Err(BlobError::NotFound(_)) => {}
            Err(e) => {
                self.uncertain = Some(u);
                return Err(e.into());
            }
        }
        Ok(())
    }

    async fn register<S: BlobStore>(&mut self, store: &S) -> Result<(), ReportError> {
        let shard = shard_of(&self.node);
        for _ in 0..MAX_CAS_ATTEMPTS {
            let (mut nodes, tag) = read_index(store, self.month, shard).await?;
            if !nodes.insert(self.node.clone()) {
                self.registered = true;
                return Ok(());
            }
            let pre = match tag {
                Some(t) => Precondition::Match(t),
                None => Precondition::NotExists,
            };
            match store
                .put_conditional(
                    &index_key(self.month, shard),
                    encode_index(&nodes).into(),
                    pre,
                )
                .await
            {
                Ok(_) => {
                    self.registered = true;
                    return Ok(());
                }
                // Another node registered first: re-read and add to theirs, never overwrite.
                Err(CasError::Lost | CasError::Contended) => {}
                Err(CasError::Io(e)) => return Err(BlobError::Other(e).into()),
            }
        }
        Err(ReportError::IndexContended(shard))
    }
}

/// A month's usage, summed across every node that reported it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Rollup {
    /// Every node the index named.
    pub nodes: Vec<String>,
    /// Per tenant, the sum of every node's report.
    pub tenants: BTreeMap<TenantId, Usage>,
}

/// A tenant's error budget for a month (M109).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Budget {
    /// Every answer the tenant was given.
    pub answered: u64,
    /// The answers that spent the budget.
    pub failed: u64,
    /// `floor((1 − target) × answered)`: the failures the target allows.
    pub allowed: u64,
    /// `allowed − failed`. Negative once the budget is overspent.
    pub remaining: i64,
    /// More failures than the target allows.
    pub exhausted: bool,
}

impl Rollup {
    /// The tenant's summed usage. Zero for a tenant no node served.
    #[must_use]
    pub fn usage(&self, tenant: TenantId) -> Usage {
        self.tenants.get(&tenant).copied().unwrap_or_default()
    }

    /// The tenant's error budget at `target_ppm`, the answers in a million that must
    /// succeed: `999_000` is three nines. Above a million it is a million.
    ///
    /// ⚠️ **Integers, not a fraction.** `1.0 - 0.9999` is just under `0.0001` in `f64`, so a
    /// float target floored four nines over 10,000 answers to no failure allowed at all.
    #[must_use]
    pub fn budget(&self, tenant: TenantId, target_ppm: u32) -> Budget {
        const MILLION: u128 = 1_000_000;
        let u = self.usage(tenant);
        let spare = MILLION - u128::from(target_ppm).min(MILLION);
        let allowed = u64::try_from(u128::from(u.answered) * spare / MILLION).unwrap_or(u64::MAX);
        let signed = |n: u64| i64::try_from(n).unwrap_or(i64::MAX);
        Budget {
            answered: u.answered,
            failed: u.failed,
            allowed,
            remaining: signed(allowed) - signed(u.failed),
            exhausted: u.failed > allowed,
        }
    }
}

/// Sums every node's report for `month`. **Two rounds:** the [`INDEX_SHARDS`] index shards,
/// then one GET per node they name. No LIST.
///
/// # Errors
/// If the store refuses or an object is malformed.
pub async fn rollup<S: BlobStore>(store: &S, month: Month) -> Result<Rollup, ReportError> {
    let shards = futures_util::future::try_join_all(
        (0..INDEX_SHARDS).map(|s| async move { read_index(store, month, s).await }),
    )
    .await?;
    let nodes: BTreeSet<String> = shards.into_iter().flat_map(|(n, _)| n).collect();
    let reports = futures_util::future::try_join_all(nodes.iter().map(|n| async move {
        let key = report_key(month, n);
        match store.get(&key).await {
            Ok(b) => decode_report(&b, key.as_str()),
            // Registered and never written: a report whose write failed after a registration
            // that landed on an earlier publish of another process. It counted nothing.
            Err(BlobError::NotFound(_)) => Ok(BTreeMap::new()),
            Err(e) => Err(e.into()),
        }
    }))
    .await?;
    let mut tenants: BTreeMap<TenantId, Usage> = BTreeMap::new();
    for report in reports {
        for (t, u) in report {
            let e = tenants.entry(t).or_default();
            *e = *e + u;
        }
    }
    Ok(Rollup {
        nodes: nodes.into_iter().collect(),
        tenants,
    })
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing,
        reason = "assertions in tests are the reporting mechanism"
    )]

    use super::*;

    #[test]
    fn a_report_and_an_index_round_trip_and_a_trailing_byte_is_corrupt() {
        let mut r = BTreeMap::new();
        r.insert(
            TenantId(3),
            Usage {
                requests: [1, 2, 3, 4],
                bytes: [5, 6, 7, 8],
                answered: 9,
                failed: 10,
            },
        );
        let b = encode_report(&r);
        assert_eq!(decode_report(&b, "r").unwrap(), r);
        let mut long = b.clone();
        long.push(0);
        assert!(matches!(
            decode_report(&long, "r"),
            Err(ReportError::Corrupt(_))
        ));
        assert!(matches!(
            decode_report(b.get(..b.len() - 1).unwrap(), "r"),
            Err(ReportError::Corrupt(_))
        ));
        let mut bad = b;
        bad[0] = b'X';
        assert!(matches!(
            decode_report(&bad, "r"),
            Err(ReportError::Corrupt(_))
        ));

        let nodes: BTreeSet<String> = ["n1".to_owned(), "n22".to_owned()].into();
        let i = encode_index(&nodes);
        assert_eq!(decode_index(&i, "i").unwrap(), nodes);
        let mut long = i;
        long.push(0);
        assert!(matches!(
            decode_index(&long, "i"),
            Err(ReportError::Corrupt(_))
        ));
    }

    /// Every day from 1970 through 2500 against a calendar walked one month at a time, so
    /// each century rule and the 400-year era boundary is crossed: 2000 and 2400 are leap
    /// years, 2100, 2200 and 2300 are not.
    #[test]
    fn every_day_to_2500_falls_in_the_month_a_walked_calendar_says() {
        let leap = |y: i64| (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
        let mut day: u64 = 0;
        for year in 1970..=2500_i64 {
            for month in 1..=12_u8 {
                let len = match month {
                    2 if leap(year) => 29,
                    2 => 28,
                    4 | 6 | 9 | 11 => 30,
                    _ => 31,
                };
                for _ in 0..len {
                    let want = Month { year, month };
                    for secs in [day * 86_400, day * 86_400 + 86_399] {
                        assert_eq!(Month::of(Duration::from_secs(secs)), want, "day {day}");
                    }
                    day += 1;
                }
            }
        }
    }

    #[test]
    fn keys_carry_the_month_and_the_identity() {
        let m = Month {
            year: 2026,
            month: 3,
        };
        assert!(
            report_key(m, "n1")
                .as_str()
                .ends_with("/use/2026-03/node/n1")
        );
        assert!(index_key(m, 10).as_str().ends_with("/use/2026-03/idx/0a"));
        assert_ne!(report_key(m, "n1"), report_key(m, "n2"));
        assert!((0..1000).all(|i| shard_of(&format!("n{i}")) < INDEX_SHARDS));
        let used: BTreeSet<u32> = (0..1000).map(|i| shard_of(&format!("n{i}"))).collect();
        assert_eq!(used.len(), INDEX_SHARDS as usize, "a shard is never used");
    }
}
