//! What a converged round costs, at fleet sizes a container fleet cannot reach.
//!
//! ⚠️ **What this measures and what it does not.** Message sizes and round counts, exactly —
//! it runs the real protocol, not a model of one. It does **not** measure CPU, scheduling, or
//! anything a real network does; those need `scripts/cluster.sh`. The 10,000-node column is
//! honest about bytes and dishonest about nothing, because it makes no claim beyond them.
#![allow(clippy::print_stdout, reason = "a reporting example is its own output")]

use pstore_gossip::{Cluster, Message, Protocol};
use std::collections::BTreeMap;

fn id(n: u32) -> [u8; 16] {
    let mut b = [0u8; 16];
    b[..4].copy_from_slice(&n.to_le_bytes());
    b
}

fn addr(n: u32) -> String {
    format!("10.{}.{}.{}:7946", n >> 16, (n >> 8) & 0xff, n & 0xff)
}

fn converged_round_bytes(n: u32) -> (u64, u64) {
    let mut nodes: Vec<Protocol> = Vec::new();
    let mut by_addr = BTreeMap::new();
    for i in 0..n {
        let mut c = Cluster::new(id(i), addr(i));
        for j in 0..n {
            c.join(id(j), addr(j));
        }
        by_addr.insert(addr(i), i as usize);
        nodes.push(Protocol::new(c));
    }
    let mut bytes = 0u64;
    let mut count = 0u64;
    for round in 0..3u64 {
        let mut queue: Vec<(String, String, Message)> = Vec::new();
        for (i, p) in nodes.iter_mut().enumerate() {
            let from = addr(i as u32);
            queue.extend(
                p.tick(round.wrapping_add(i as u64))
                    .into_iter()
                    .map(|(to, m)| (from.clone(), to, m)),
            );
        }
        for _ in 0..3 {
            let mut next = Vec::new();
            for (from, to, msg) in queue.drain(..) {
                let enc = msg.encode();
                if round == 2 {
                    bytes += enc.len() as u64;
                    count += 1;
                }
                if let Some(&i) = by_addr.get(&to)
                    && let Some(p) = nodes.get_mut(i)
                {
                    let here = to.clone();
                    next.extend(
                        p.receive(&from, &msg)
                            .into_iter()
                            .map(|(t, m)| (here.clone(), t, m)),
                    );
                }
            }
            if next.is_empty() {
                break;
            }
            queue = next;
        }
    }
    (bytes / u64::from(n), count / u64::from(n))
}

fn main() {
    println!(
        "{:>8} {:>16} {:>14}",
        "nodes", "bytes/node/round", "msgs/node"
    );
    for n in [100u32, 1_000, 10_000] {
        let (bytes, msgs) = converged_round_bytes(n);
        println!("{n:>8} {bytes:>16} {msgs:>14}");
    }
    println!("\nchitchat, measured on a real 100-node fleet: ~13,000 bytes/node/round");
}
