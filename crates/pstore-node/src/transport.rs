//! A gossip transport that **counts bytes and drops datagrams on purpose**.
//!
//! Two M4b criteria need a seam the operating system will not give us:
//!
//! * **Criterion 3** injects 10% probe loss. `tc netem` is the obvious tool and needs
//!   `NET_ADMIN` on every one of a hundred containers — a privileged fleet, to test an
//!   unprivileged one. Dropping in the transport needs no capability and, unlike `netem`,
//!   is **deterministic given a seed**, so a failure is reproducible.
//! * **Criterion 5** wants bytes/s/node at 25, 50 and 100 nodes. Docker's network counters
//!   include the blob-store traffic and the container runtime's own chatter; counting at
//!   the chitchat `Socket` counts gossip and nothing else.
//!
//! ⚠️ The loss is applied to **outbound** datagrams only. Dropping inbound would count the
//! bytes before discarding them and quietly inflate criterion 5's answer by the loss rate.

use chitchat::transport::{RecvOutcome, SendOutcome, Socket, Transport};
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// Gossip traffic, counted at the socket.
#[derive(Debug, Default)]
pub struct Stats {
    sent: AtomicU64,
    recvd: AtomicU64,
    dropped: AtomicU64,
}

impl Stats {
    /// Bytes put on the wire, bytes taken off it, and datagrams deliberately discarded.
    pub fn read(&self) -> (u64, u64, u64) {
        (
            self.sent.load(Ordering::Relaxed),
            self.recvd.load(Ordering::Relaxed),
            self.dropped.load(Ordering::Relaxed),
        )
    }
}

/// A deterministic Bernoulli trial.
///
/// ⚠️ Deliberately not a real RNG. A seeded xorshift makes an injected-loss run
/// **reproducible**: the same seed drops the same datagrams, so a convergence failure under
/// loss can be replayed instead of chased.
#[derive(Debug)]
pub(crate) struct Bernoulli {
    state: u64,
}

impl Bernoulli {
    pub(crate) fn new(seed: u64) -> Self {
        // ⚠️ splitmix64, not `seed | CONST`. OR is **lossy**: it cannot clear a bit, so
        // every seed differing only in bits the constant already sets collapses to the same
        // state — measured, seeds 42 and 43 produced byte-identical drop sequences, and a
        // per-node seed would have given a hundred nodes one loss pattern. splitmix64 is a
        // bijection, so distinct seeds stay distinct.
        let mut z = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        // Zero is a fixed point of xorshift: it would return `false` forever and turn a
        // 10%-loss run into a 0%-loss run that passes.
        Self {
            state: if z == 0 { 0x9E37_79B9_7F4A_7C15 } else { z },
        }
    }

    /// The next draw in `[0, 1)`.
    fn draw(&mut self) -> f64 {
        self.state ^= self.state << 13;
        self.state ^= self.state >> 7;
        self.state ^= self.state << 17;
        // Top 53 bits: the mantissa of an f64, so the quantisation is below any loss rate
        // worth injecting.
        (self.state >> 11) as f64 / (1u64 << 53) as f64
    }

    /// Whether this trial fires, at probability `p`: on a draw in `[0, p)`.
    pub(crate) fn fires(&mut self, p: f64) -> bool {
        self.draw() < p
    }
}

/// Wraps any `Transport`, counting bytes and dropping a fraction of what it sends.
#[derive(Debug)]
pub struct Metered<T> {
    inner: T,
    loss: f64,
    seed: u64,
    stats: Arc<Stats>,
}

impl<T: Transport> Metered<T> {
    /// Wrap `inner`, dropping outbound datagrams at rate `loss` from a `seed`-derived stream.
    #[must_use]
    pub fn new(inner: T, loss: f64, seed: u64) -> Self {
        Self {
            inner,
            loss,
            seed,
            stats: Arc::new(Stats::default()),
        }
    }

    /// A handle to the counters, shared with every socket this transport opens.
    #[must_use]
    pub fn stats(&self) -> Arc<Stats> {
        Arc::clone(&self.stats)
    }
}

#[async_trait::async_trait]
impl<T: Transport> Transport for Metered<T> {
    async fn open(&self, listen_addr: SocketAddr) -> anyhow::Result<Box<dyn Socket>> {
        Ok(Box::new(MeteredSocket {
            inner: self.inner.open(listen_addr).await?,
            loss: self.loss,
            rng: Bernoulli::new(self.seed),
            stats: Arc::clone(&self.stats),
        }))
    }
}

struct MeteredSocket {
    inner: Box<dyn Socket>,
    loss: f64,
    rng: Bernoulli,
    stats: Arc<Stats>,
}

#[async_trait::async_trait]
impl Socket for MeteredSocket {
    fn local_addr(&self) -> anyhow::Result<SocketAddr> {
        self.inner.local_addr()
    }

    async fn send(
        &mut self,
        to: SocketAddr,
        envelope: chitchat::ChitchatEnvelope,
    ) -> anyhow::Result<SendOutcome> {
        if self.rng.fires(self.loss) {
            self.stats.dropped.fetch_add(1, Ordering::Relaxed);
            // ⚠️ Report success with zero bytes, which is what a lost datagram looks like to
            // a sender: UDP never tells it otherwise. Returning `Err` would instead tell
            // chitchat the transport is broken, and it would stop — testing shutdown, not
            // loss.
            return Ok(SendOutcome { num_bytes_sent: 0 });
        }
        let out = self.inner.send(to, envelope).await?;
        self.stats
            .sent
            .fetch_add(out.num_bytes_sent as u64, Ordering::Relaxed);
        Ok(out)
    }

    async fn recv(&mut self) -> anyhow::Result<RecvOutcome> {
        let out = self.inner.recv().await?;
        self.stats
            .recvd
            .fetch_add(out.num_bytes_received as u64, Ordering::Relaxed);
        Ok(out)
    }
}

#[cfg(test)]
#[expect(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "assertions in tests are the reporting mechanism"
)]
mod tests {
    use super::*;

    #[test]
    fn an_injected_loss_rate_is_the_rate_that_is_injected() {
        // ⚠️ The point of the test is the RATE, not that something was dropped. A wrapper
        // that dropped 90% when asked for 10% would still make criterion 3's run "inject
        // loss", and would make it measure a partition instead.
        for p in [0.0, 0.1, 0.5] {
            let mut b = Bernoulli::new(7);
            let n = 100_000;
            let fired = (0..n).filter(|_| b.fires(p)).count();
            let got = fired as f64 / f64::from(n);
            assert!(
                (got - p).abs() < 0.01,
                "asked for {p} loss and got {got:.4}"
            );
        }
    }

    /// ⚠️ **A rate `p` fires on `[0, p)`.** So a draw exactly equal to the rate does not fire
    /// -- which is also what makes a rate of 0.0 never fire. A seeded draw essentially never
    /// lands on a rate, which is why `<` -> `<=` survived every rate test; this puts it there
    /// deliberately, and one float above must fire. A fresh generator per case, so each sees
    /// the same first draw.
    #[test]
    fn a_draw_equal_to_the_rate_does_not_fire_and_one_below_it_does() {
        let u = Bernoulli::new(7).draw();
        assert!(
            u > 0.0,
            "seed 7's first draw is exactly zero; choose another"
        );
        assert!(
            !Bernoulli::new(7).fires(u),
            "a draw equal to the rate fired"
        );
        assert!(
            Bernoulli::new(7).fires(u.next_up()),
            "a draw below the rate did not fire"
        );
    }

    #[test]
    fn a_zero_seed_still_injects_loss() {
        // A zero state is a fixed point of xorshift: `fires` would return false forever and
        // a 10%-loss run would silently be a 0%-loss run that passes.
        let mut b = Bernoulli::new(0);
        let fired = (0..10_000).filter(|_| b.fires(0.1)).count();
        assert!(fired > 800, "a zero seed injected {fired} drops in 10,000");
    }

    /// The first 64 trials at `seed = 7`, `p = 0.25`, as `1` for fires and `0` for not.
    ///
    /// ⚠️ Derived from an **independent implementation** of the documented algorithm
    /// (splitmix64 seeding, xorshift64, top-53-bits-as-a-float), not captured from this code.
    /// A snapshot of the implementation agrees with the implementation by construction and
    /// tests nothing; this disagrees the moment the arithmetic changes. Mutation testing is
    /// what showed the difference — the shift directions inside `fires` could be reversed and
    /// every distributional test stayed green, because a differently-wrong PRNG is still a
    /// PRNG and still produces the right RATE.
    const GOLDEN_TRIALS: &str = "1000101001000010010011110010000010000010000000110110000010010010";

    #[test]
    fn the_drop_stream_is_the_algorithm_that_was_specified() {
        let mut b = Bernoulli::new(7);
        let got: String = (0..GOLDEN_TRIALS.len())
            .map(|_| if b.fires(0.25) { '1' } else { '0' })
            .collect();
        assert_eq!(
            got, GOLDEN_TRIALS,
            "the drop stream changed. A replayable loss run is the reason this is not a real \
             RNG, so the sequence is a contract, not an implementation detail."
        );
    }

    #[test]
    fn the_same_seed_drops_the_same_datagrams() {
        // Reproducibility is the reason this is not a real RNG: a convergence failure under
        // loss has to be replayable.
        let seq = |seed| {
            let mut b = Bernoulli::new(seed);
            (0..500).map(|_| b.fires(0.2)).collect::<Vec<_>>()
        };
        assert_eq!(seq(42), seq(42));
        assert_ne!(seq(42), seq(43), "the seed is not reaching the sequence");
    }

    #[tokio::test]
    async fn bytes_are_counted_in_both_directions_and_drops_are_not() {
        use chitchat::transport::ChannelTransport;

        let t = Metered::new(ChannelTransport::with_mtu(1400), 0.0, 1);
        let stats = t.stats();
        let a: SocketAddr = "127.0.0.1:10001".parse().unwrap();
        let b: SocketAddr = "127.0.0.1:10002".parse().unwrap();
        let (mut sa, mut sb) = (t.open(a).await.unwrap(), t.open(b).await.unwrap());

        let env = chitchat::ChitchatEnvelope {
            version: chitchat::ProtocolVersion::V1,
            message: chitchat::ChitchatMessage::BadCluster,
        };
        let sent = sa.send(b, env).await.unwrap().num_bytes_sent;
        // ⚠️ Bounded. `recv` on a datagram that was never delivered waits forever, so a
        // defect that drops everything makes this test HANG rather than fail — and mutation
        // testing then reports a timeout, which is not evidence the defect was caught. Found
        // exactly that way: `fires -> true` timed out here instead of failing.
        let got = tokio::time::timeout(std::time::Duration::from_secs(5), sb.recv())
            .await
            .expect("the datagram was sent with no loss injected, so it must arrive")
            .unwrap();

        let (s, r, d) = stats.read();
        assert!(sent > 0 && s == sent as u64, "sent {s} counted for {sent}");
        assert_eq!(r, got.num_bytes_received as u64);
        assert_eq!(d, 0, "nothing was asked to be dropped");
        assert_eq!(got.from_addr, a);
    }

    #[tokio::test]
    async fn a_dropped_datagram_costs_no_bytes_and_is_not_an_error() {
        // Total loss, so the assertion cannot pass by luck.
        use chitchat::transport::ChannelTransport;
        let t = Metered::new(ChannelTransport::with_mtu(1400), 1.0, 3);
        let stats = t.stats();
        let b: SocketAddr = "127.0.0.1:10004".parse().unwrap();
        let mut sa = t.open("127.0.0.1:10003".parse().unwrap()).await.unwrap();

        for _ in 0..20 {
            let env = chitchat::ChitchatEnvelope {
                version: chitchat::ProtocolVersion::V1,
                message: chitchat::ChitchatMessage::BadCluster,
            };
            // Not an error: to a UDP sender a lost datagram is indistinguishable from a
            // delivered one.
            assert_eq!(sa.send(b, env).await.unwrap().num_bytes_sent, 0);
        }
        let (s, _, d) = stats.read();
        assert_eq!(d, 20, "20 sends under total loss dropped {d}");
        assert_eq!(
            s, 0,
            "a dropped datagram was counted as {s} bytes on the wire"
        );
    }
}
