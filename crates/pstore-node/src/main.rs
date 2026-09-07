//! A cluster member.
//!
//! Joins from the roster (**one GET**, no LIST, no DNS), gossips, and reports what it would
//! own. ⚠️ **Not a query server.** M4b measures membership and placement; a server would make
//! every number here about something else, and the milestone would silently become a
//! benchmark of a query path that does not exist yet.
//!
//! ## What this node is, in one sentence
//!
//! It owns nothing. Placement tells it which shards it would *cache*; the blob store holds
//! everything. That is why adding or removing nodes copies no bytes.

use pstore_blob::{BlobStore, Capabilities, ObjectStoreBackend};
use pstore_cluster::Roster;
use pstore_node::policy::{self, fresh_node_id, jitter, read_roster_patiently};
use pstore_node::{ATTEMPT_TIMEOUT, DEFAULT_GOSSIP_PERIOD, HEAL_PERIOD, gossip, swim};
use std::sync::Arc;
use std::time::Duration;

fn env(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_owned())
}

// ⚠️ The doc comment that stood here described `fresh_node_id`, which moved to
// `pstore_node::policy`. Left as a note rather than deleted silently, because the rule it
// carried still binds and the SWIM path currently breaks it: `swim::derive_id` derives an
// identity from the advertised address, so a restarted node does NOT look cold. That is a
// cache-placement problem, and `swim::derive_id` says where it has to be fixed.

/// Whichever membership protocol this process was asked to run.
///
/// ⚠️ An enum rather than a trait object: there are exactly two, one of them is on its way
/// out, and a trait would invite a third.
enum Membership {
    /// Scuttlebutt, via `chitchat` — what M4b measured.
    Chitchat(gossip::Member),
    /// Probe-based membership — what M4c replaces it with.
    Swim(swim::Member),
}

impl Membership {
    fn self_addr(&self) -> String {
        match self {
            Self::Chitchat(m) => m.self_addr(),
            Self::Swim(m) => m.self_addr(),
        }
    }

    async fn members(&self) -> Vec<String> {
        match self {
            Self::Chitchat(m) => m.members().await,
            Self::Swim(m) => m.members().await,
        }
    }

    /// The view SIZE, without materialising the view.
    ///
    /// ⚠️ The loop asks every period and only ever compares the number. At 1,000 members that
    /// is a thousand string allocations a second per node — measured as free at 100 nodes and
    /// emphatically not at 1,000, which is the whole lesson: a cost that is invisible at the
    /// size you test at is not therefore absent.
    async fn member_count(&self) -> usize {
        match self {
            Self::Chitchat(m) => m.members().await.len(),
            Self::Swim(m) => m.member_count().await,
        }
    }

    fn traffic(&self) -> (u64, u64, u64) {
        match self {
            Self::Chitchat(m) => m.traffic(),
            Self::Swim(m) => m.traffic(),
        }
    }

    async fn dial(&self, addr: &str) -> bool {
        match self {
            Self::Chitchat(m) => m.dial(addr),
            Self::Swim(m) => m.dial(addr).await,
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cluster = env("PSTORE_CLUSTER", "c1");
    let node_id = env("PSTORE_NODE_ID", &fresh_node_id());
    let listen = env("PSTORE_GOSSIP_ADDR", "0.0.0.0:7946");
    let advertise = env("PSTORE_ADVERTISE", &listen);
    let endpoint = env("PSTORE_S3_ENDPOINT", "http://minio:9000");
    let bucket = env("PSTORE_BUCKET", "pstore");

    // Concrete, not `dyn`: `Roster`'s methods are generic over `BlobStore`, which keeps the
    // trait object-safety question out of the trait itself.
    let store = blob_store(&endpoint, &bucket)?;

    // ⚠️ One GET. This is the whole of discovery: no DNS, no service registry, no LIST. The
    // bucket we already depend on is how a node finds the fleet, which is what keeps "one
    // stateful dependency" true.
    let (roster, tag) = read_roster_patiently(&store, &cluster, &node_id).await?;
    let seeds: Vec<String> = roster
        .nodes()
        .iter()
        .filter(|n| **n != advertise)
        .cloned()
        .collect();
    println!(
        "JOIN node={node_id} advertise={advertise} seeds={}",
        seeds.len()
    );

    // Injected probe loss (M4b criterion 3). Zero unless asked: a fleet that always drops
    // datagrams cannot tell a loss result from a baseline one.
    let loss: f64 = std::env::var("PSTORE_PROBE_LOSS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0.0);
    // ⚠️ One value, used twice: chitchat's gossip interval and this loop's sampling
    // interval are the same number, so a timing reported in periods means the same thing to
    // the node and to the harness that reads its log.
    let period = std::env::var("PSTORE_GOSSIP_PERIOD_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|ms| *ms > 0)
        .map_or(DEFAULT_GOSSIP_PERIOD, Duration::from_millis);
    // ⚠️ Separable from the gossip period ON PURPOSE. This loop asks chitchat for its member
    // list every tick, which locks its state and clones a `String` per member — 500
    // allocations a second per node at 100 members and a 200ms period, all of it ours rather
    // than the protocol's. Whether that matters is a measurement, and it cannot be taken
    // while the two intervals are the same number.
    let poll = std::env::var("PSTORE_POLL_PERIOD_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|ms| *ms > 0)
        .map_or(period, Duration::from_millis);
    // ⚠️ Both protocols stay runnable, on one harness, deliberately. Replacing a membership
    // layer and measuring only the replacement compares two runs rather than two protocols —
    // and every difference in the host, the harness or the day lands in the result.
    let handle = if env("PSTORE_GOSSIP", "swim") == "chitchat" {
        Membership::Chitchat(
            gossip::start(&node_id, &listen, &advertise, &seeds, loss, period).await?,
        )
    } else {
        Membership::Swim(swim::start(&listen, &advertise, &seeds, loss, period).await?)
    };
    // ⚠️ The address gossip reports for THIS node, not the name it was configured with.
    // chitchat advertises a resolved `SocketAddr`, so peers see `10.0.0.7:7946` while the
    // config says `pstore-n7:7946` — and a node comparing the configured name against its
    // own gossip view never finds itself. Measured: every node reported owning 0 of 1000
    // shards while holding a 44-member view, which reads as a placement bug and is a naming
    // bug.
    let me = handle.self_addr();
    println!("SELF advertise={advertise} gossip={me}");

    // ⚠️ Announce on join, not on the next refold tick. A cold cluster's roster is empty, so
    // the first node has no seeds — and if it waits a full period before publishing itself,
    // every node that starts meanwhile also finds an empty roster and forms its own
    // one-member cluster. Measured: ten nodes, ten singleton views. The roster is a cache of
    // gossip, and a cache nobody writes until later is one nobody can join through.
    let mine = roster.merged(&Roster::from_nodes([me.clone()]));
    match Roster::refold(&store, &cluster, &mine, tag).await {
        Ok(()) => println!("ANNOUNCE ok members={}", mine.nodes().len()),
        Err(_) => {
            // Lost the race: rebase and try once. Losing repeatedly is fine — gossip will
            // carry us in as soon as one peer knows us.
            if let Ok((cur, t)) = Roster::read(&store, &cluster).await {
                let merged = cur.merged(&Roster::from_nodes([me.clone()]));
                let _ = Roster::refold(&store, &cluster, &merged, t).await;
            }
        }
    }

    // Report the view, and refold the roster from it. The roster is a *cache of gossip*, so
    // a lost refold is never a lost membership — it is one fewer cache update.
    // ⚠️ Everything below is counted in PERIODS, not seconds, because the period is no
    // longer fixed — it scales with the fleet. `ticks % HEAL_PERIOD.as_secs()` was correct
    // only while a period happened to be 200ms and five of them made a second; at a 2s
    // period the same expression heals every 100 seconds.
    let view_every = (Duration::from_secs(1).as_millis() / poll.as_millis().max(1)).max(1) as u64;
    let heal_every = (HEAL_PERIOD.as_millis() / poll.as_millis().max(1)).max(1) as u64;
    // How often to report what this node would own, in seconds; 0 disables it.
    let owns_every: u64 = std::env::var("PSTORE_OWNS_PERIOD_S")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(5);
    let mut ticks = 0u64;
    let mut sub = 0u64;
    let mut last_size = usize::MAX;
    loop {
        // ⚠️ Sampled at the GOSSIP period, not once a second. Criterion 2 is stated in
        // periods of 200ms and its bound is eight of them — an instrument that samples once
        // a second resolves to ±5 periods, so a one-second sampler cannot tell 8 from 9 and
        // any pass or fail it reports is unfalsifiable. Measured before this change: "9
        // periods" against a bound of 8, from a sampler that could not have said otherwise.
        tokio::time::sleep(poll).await;
        sub += 1;
        let size = handle.member_count().await;

        // The view size changes rarely, so printing on CHANGE costs almost nothing and is
        // what actually carries the timing: `docker logs -t` timestamps it to the
        // millisecond, and convergence is the last of these across the fleet.
        if size != last_size {
            last_size = size;
            println!("VIEWCHANGE members={last_size} self={me}");
        }

        if !sub.is_multiple_of(view_every) {
            continue;
        }
        // Only now, once a second at most, is the list itself worth building.
        let members = handle.members().await;
        ticks += 1;
        // Scraped by `scripts/cluster.sh`; a line per second per node is cheap and needs no
        // endpoint, which would be a server by another name.
        println!("VIEW t={ticks} members={size} self={me}");

        if ticks % heal_every == jitter(&node_id, heal_every) {
            // Read first. The roster is two things at once here: the directory this node
            // publishes into, and the seed list it heals a partition from.
            // Bounded for the same reason the join read is: an unbounded blob request on
            // this loop stops the node reporting anything at all, and a node that has
            // stopped reporting is one the harness cannot tell from a dead one.
            if let Ok(Ok((cur, tag))) =
                tokio::time::timeout(ATTEMPT_TIMEOUT, Roster::read(&store, &cluster)).await
            {
                let mut healed = 0usize;
                for n in policy::to_dial(&cur, &members) {
                    if handle.dial(n).await {
                        healed += 1;
                    }
                }

                match policy::union_to_publish(&cur, &members) {
                    None => {
                        if healed > 0 {
                            println!("HEAL dialled={healed} known={}", cur.nodes().len());
                        }
                    }
                    Some(union) => {
                        match tokio::time::timeout(
                            ATTEMPT_TIMEOUT,
                            Roster::refold(&store, &cluster, &union, tag),
                        )
                        .await
                        {
                            Ok(Ok(())) => {
                                println!(
                                    "REFOLD ok members={} healed={healed}",
                                    union.nodes().len()
                                );
                            }
                            // A lost CAS is not a lost membership: the winner wrote a union
                            // too, and the next period rebases on it.
                            Ok(Err(_)) => println!("REFOLD lost, {} known", cur.nodes().len()),
                            Err(_) => println!("REFOLD timed out, {} known", cur.nodes().len()),
                        }
                    }
                }
            }
        }
        // ⚠️ **Diagnostics, not protocol**, and separable because it turned out to be
        // neither free nor obviously not-free. This block builds a ring and does 1,000
        // placements — 32,000 rendezvous hashes at 100 nodes, ~93,000 at 1,000 — and it
        // exists only so a fleet run can report what a node WOULD own. It was inside the
        // number M4b attributed to gossip. `PSTORE_OWNS_PERIOD_S=0` turns it off, which is
        // what makes the attribution measurable instead of assumed.
        if owns_every > 0 && ticks.is_multiple_of(owns_every) {
            // Reuse the view already fetched this tick. Fetching again locks chitchat's
            // state and clones a String per member, for a list that cannot have changed.
            let r = Roster::from_nodes(members.iter().cloned());
            let mine = policy::owned_shards(&r, &me, 1000, 3);
            let (sent, recvd, dropped) = handle.traffic();
            println!(
                "OWNS shards={mine} of=1000 members={} sent={sent} recvd={recvd} dropped={dropped}",
                r.nodes().len()
            );
        }
    }
}

fn blob_store(endpoint: &str, bucket: &str) -> Result<impl BlobStore, Box<dyn std::error::Error>> {
    let s3 = object_store::aws::AmazonS3Builder::new()
        .with_endpoint(endpoint)
        .with_bucket_name(bucket)
        .with_access_key_id(env("PSTORE_ACCESS_KEY", "pstore"))
        .with_secret_access_key(env("PSTORE_SECRET_KEY", "pstore-dev-secret"))
        .with_allow_http(true)
        .with_region("us-east-1")
        .build()?;
    // ⚠️ `unprobed`: this backend's capabilities have NOT been measured here. MinIO is known
    // to ignore `If-None-Match: *`, which is why the roster uses CAS on an observed tag and
    // never create-if-absent.
    Ok(ObjectStoreBackend::new(
        Arc::new(s3),
        Capabilities {
            backend: format!("s3({endpoint})"),
            ..ObjectStoreBackend::unprobed("s3")
        },
    ))
}
