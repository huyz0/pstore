//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! Reconcile's argument, checked over **every** interleaving (M22 criterion 8).
//!
//! Each actor's requests to the store, and each step it takes on the truth (a HEAD CAS, a
//! HEAD read), wait at a gate. A scheduler opens one gate at a time and enumerates every
//! order, depth first. The property: when everyone is done, the shard has an entry iff the
//! truth says there is work, carrying the truth's generation.

mod common;

use common::{Counting, Gate};
use pstore_blob::TagStyle;
use pstore_jobs::{Claim, Register};
use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;

#[derive(Debug, Default)]
struct State {
    armed: bool,
    waiting: BTreeSet<usize>,
    done: BTreeSet<usize>,
    go: Option<usize>,
}

#[derive(Debug, Default)]
struct Sched {
    state: Mutex<State>,
    changed: Notify,
}

impl Sched {
    fn with<T>(&self, f: impl FnOnce(&mut State) -> T) -> T {
        let out = f(&mut self.state.lock().unwrap());
        self.changed.notify_waiters();
        out
    }

    /// Waits until `ready` holds of the state.
    async fn until(&self, mut ready: impl FnMut(&mut State) -> bool) {
        loop {
            let n = self.changed.notified();
            tokio::pin!(n);
            n.as_mut().enable();
            if ready(&mut self.state.lock().unwrap()) {
                return;
            }
            n.await;
        }
    }

    fn finish(&self, actor: usize) {
        self.with(|s| s.done.insert(actor));
    }
}

#[async_trait::async_trait]
impl Gate for Sched {
    async fn pass(&self, actor: usize) {
        if !self.state.lock().unwrap().armed {
            return;
        }
        self.with(|s| s.waiting.insert(actor));
        self.until(|s| {
            if s.go == Some(actor) {
                s.go = None;
                s.waiting.remove(&actor);
                true
            } else {
                false
            }
        })
        .await;
    }
}

/// The truth a reconcile reads: `Some(generation)` when there is work.
type Truth = Arc<Mutex<Option<u64>>>;

/// What an actor does.
#[derive(Debug, Clone, Copy)]
enum Act {
    /// Sets the truth, then reconciles: a control call.
    Control(Option<u64>),
    /// Reconciles alone: a worker's cleanup, or a status repair.
    Reconcile,
    /// Renews its claims: a worker's heartbeat.
    Renew,
    /// Claims what it can: a worker's scan.
    Claim,
}

const ID: &str = "t1";

/// Switches away from a runnable actor that one order may make.
const PREEMPTIONS: usize = 3;

/// Runs `acts` under `schedule` (indexes into the waiting set, in order; beyond it, the
/// first). Returns each decision as (taken, of how many), and the end state.
async fn run(
    style: TagStyle,
    initial: Option<u64>,
    claimed: bool,
    acts: &[Act],
    schedule: &[usize],
) -> (Vec<(usize, usize)>, Option<u64>, Option<u64>) {
    let store = Counting::with_tags(style);
    let truth: Truth = Arc::new(Mutex::new(initial));
    let sched = Arc::new(Sched::default());
    let setup = Register::open(Arc::new(store.clone()), "rep", 4)
        .await
        .unwrap();
    if let Some(g) = initial {
        let claim = claimed.then_some(Claim {
            owner: 99,
            expires_ms: 0,
        });
        setup
            .reconcile(ID, || async move { Ok(Some(g)) }, claim, false)
            .await
            .unwrap();
    }
    let mut regs = Vec::new();
    for a in 0..acts.len() {
        let gated = store.gated(Arc::clone(&sched) as Arc<dyn Gate>, a);
        regs.push(Arc::new(
            Register::open(Arc::new(gated), "rep", 4).await.unwrap(),
        ));
    }
    sched.with(|s| s.armed = true);
    let mut tasks = Vec::new();
    for (a, act) in acts.iter().copied().enumerate() {
        let reg = Arc::clone(&regs[a]);
        let truth = Arc::clone(&truth);
        let sched = Arc::clone(&sched);
        tasks.push(tokio::spawn(async move {
            let want = || {
                let truth = Arc::clone(&truth);
                let sched = Arc::clone(&sched);
                async move {
                    sched.pass(a).await;
                    Ok(*truth.lock().unwrap())
                }
            };
            let shard = reg.shard_of(ID);
            match act {
                Act::Control(v) => {
                    sched.pass(a).await;
                    *truth.lock().unwrap() = v;
                    reg.reconcile(ID, want, None, true).await.unwrap();
                }
                Act::Reconcile => {
                    reg.reconcile(ID, want, None, false).await.unwrap();
                }
                Act::Renew => {
                    reg.renew(shard, 99, 0, 1_000).await.unwrap();
                }
                Act::Claim => {
                    reg.claim(shard, 98, 10, 1_000, 5).await.unwrap();
                }
            }
            sched.finish(a);
        }));
    }
    let n = acts.len();
    let mut decisions = Vec::new();
    let (mut last, mut preempted) = (None, 0);
    loop {
        sched
            .until(|s| s.go.is_none() && s.waiting.len() + s.done.len() == n)
            .await;
        let waiting: Vec<usize> = sched.with(|s| s.waiting.iter().copied().collect());
        if waiting.is_empty() {
            break;
        }
        // ⚠️ **Preemption-bounded** (as CHESS bounds it): once `PREEMPTIONS` switches away
        // from an actor that could have gone on are spent, it goes on. Unbounded, every lost
        // write adds a retry and the orders multiply past any test budget; the bugs this
        // protocol can have need two or three switches, and the one the search found needed
        // two.
        let allowed: Vec<usize> = match last {
            Some(l) if preempted >= PREEMPTIONS && waiting.contains(&l) => vec![l],
            _ => waiting,
        };
        let i = schedule.get(decisions.len()).copied().unwrap_or(0);
        decisions.push((i, allowed.len()));
        let next = allowed[i];
        if last.is_some_and(|l| l != next && allowed.contains(&l)) {
            preempted += 1;
        }
        last = Some(next);
        sched.with(|s| s.go = Some(next));
    }
    for t in tasks {
        t.await.unwrap();
    }
    let shard = setup.read(setup.shard_of(ID)).await.unwrap();
    let entry = shard.entries.get(ID).copied();
    (
        decisions,
        *truth.lock().unwrap(),
        entry.map(|e| e.generation),
    )
}

/// Every interleaving of `acts`, under both tag styles: asserts the property on each,
/// returns how many ran.
async fn every_order(initial: Option<u64>, claimed: bool, acts: &[Act]) -> usize {
    let mut runs = 0;
    for style in [TagStyle::Monotonic, TagStyle::ContentHash] {
        runs += orders(style, initial, claimed, acts).await;
    }
    runs
}

async fn orders(style: TagStyle, initial: Option<u64>, claimed: bool, acts: &[Act]) -> usize {
    let mut schedule: Vec<usize> = Vec::new();
    let mut runs = 0;
    loop {
        let (decisions, truth, entry) = run(style, initial, claimed, acts, &schedule).await;
        runs += 1;
        assert_eq!(
            entry, truth,
            "the shard disagrees with the truth after {acts:?} from {initial:?} \
             in order {decisions:?} with {style:?} tags"
        );
        // The next order, depth first: the last decision with an untried alternative.
        let Some(at) = decisions.iter().rposition(|(i, of)| i + 1 < *of) else {
            return runs;
        };
        schedule = decisions[..at].iter().map(|(i, _)| *i).collect();
        schedule.push(decisions[at].0 + 1);
    }
}

#[tokio::test(start_paused = true)]
async fn two_controllers_and_a_worker_from_nothing() {
    let acts = [Act::Control(Some(7)), Act::Control(None), Act::Reconcile];
    assert!(every_order(None, false, &acts).await > 1_000);
}

#[tokio::test(start_paused = true)]
async fn two_controllers_and_a_worker_from_work() {
    let acts = [Act::Control(Some(7)), Act::Control(None), Act::Reconcile];
    assert!(every_order(Some(3), true, &acts).await > 1_000);
}

#[tokio::test(start_paused = true)]
async fn a_resume_racing_a_pause_and_a_new_generation() {
    let acts = [
        Act::Control(None),
        Act::Control(Some(8)),
        Act::Control(Some(9)),
    ];
    assert!(every_order(Some(3), false, &acts).await > 1_000);
}

#[tokio::test(start_paused = true)]
async fn renew_and_claim_never_bring_back_a_removed_entry() {
    let acts = [Act::Control(None), Act::Renew, Act::Claim];
    assert!(every_order(Some(3), true, &acts).await > 50);
}

#[tokio::test(start_paused = true)]
async fn renew_and_claim_never_lose_a_new_generation() {
    let acts = [Act::Control(Some(4)), Act::Renew, Act::Claim];
    assert!(every_order(Some(3), true, &acts).await > 50);
}

#[tokio::test(start_paused = true)]
async fn the_enumeration_reaches_a_losing_write() {
    // The search is only worth something if it reaches the orders that matter: one in
    // which a reconcile's write loses to a racing one and has to re-read.
    let acts = [Act::Control(Some(7)), Act::Control(None)];
    let mut schedule = Vec::new();
    let mut most = 0;
    loop {
        let (decisions, ..) = run(TagStyle::Monotonic, None, false, &acts, &schedule).await;
        most = most.max(decisions.len());
        let Some(at) = decisions.iter().rposition(|(i, of)| i + 1 < *of) else {
            break;
        };
        schedule = decisions[..at].iter().map(|(i, _)| *i).collect();
        schedule.push(decisions[at].0 + 1);
    }
    // Two clean controls take 4 steps each; a lost write and its retry add 3.
    assert!(most > 8, "no order lost a write: at most {most} steps");
}
