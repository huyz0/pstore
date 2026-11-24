//! A TCP listener and a UDP socket on one free port, as a peer serves both (M273).
//!
//! ⚠️ Binding UDP to whatever port TCP was given, and dropping the listener when UDP is
//! refused, failed on Windows: its next `bind(0)` hands out the port just freed, so every try
//! named one port whose UDP half another test's socket held. Each refused socket is held until
//! the search ends, and half the tries bind UDP first, because Windows reserves port ranges per
//! protocol.

use std::io;
use std::net::{SocketAddr, TcpListener, UdpSocket};

/// Tries each way: TCP first, then UDP first.
///
/// ⚠️ A count, not `TRIES / 2`: a mutant of that division is caught by this module's tests in
/// 0.1 s, but `pstore-testkit` is a dev-dependency of every crate, so cargo-mutants runs the
/// whole workspace suite on it, past CI's 600 s limit, and reports a timeout, not a kill.
const HALF: usize = 32;

/// Tries in all.
const TRIES: usize = 2 * HALF;

/// Binds `first` to any port and `second` to the port it was given, up to `tries` times. Each
/// `first` socket whose port `second` refused is pushed to `held`, so later tries are given
/// other ports. The error is the last one seen.
fn half<A, B>(
    tries: usize,
    mut first: impl FnMut() -> io::Result<(A, u16)>,
    mut second: impl FnMut(u16) -> io::Result<B>,
    held: &mut Vec<A>,
) -> Result<(A, B), io::Error> {
    let mut last = io::Error::other("no try was made");
    for _ in 0..tries {
        match first() {
            Ok((a, port)) => match second(port) {
                Ok(b) => return Ok((a, b)),
                Err(e) => {
                    held.push(a);
                    last = e;
                }
            },
            Err(e) => last = e,
        }
    }
    Err(last)
}

/// The search over any binders: `tcp_any`/`udp_any` bind to a port of the system's choosing,
/// `tcp_on`/`udp_on` to a given one.
fn search<T, U>(
    tcp_any: impl FnMut() -> io::Result<(T, u16)>,
    udp_on: impl FnMut(u16) -> io::Result<U>,
    udp_any: impl FnMut() -> io::Result<(U, u16)>,
    tcp_on: impl FnMut(u16) -> io::Result<T>,
) -> Result<(T, U), String> {
    // Held, both halves, until the search returns.
    let mut held_tcp = Vec::new();
    let mut held_udp = Vec::new();
    let tcp_first = match half(HALF, tcp_any, udp_on, &mut held_tcp) {
        Ok(pair) => return Ok(pair),
        Err(e) => e,
    };
    match half(HALF, udp_any, tcp_on, &mut held_udp) {
        Ok((u, t)) => Ok((t, u)),
        Err(udp_first) => Err(format!(
            "no port free for both TCP and UDP in {TRIES} tries: TCP first, {tcp_first}; \
             UDP first, {udp_first}"
        )),
    }
}

fn local(port: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], port))
}

/// A TCP listener and a UDP socket on one free loopback port.
///
/// # Panics
/// When 64 tries find none.
#[must_use]
#[allow(
    clippy::panic,
    reason = "a test helper: a test with no port cannot run"
)]
pub fn same_port() -> (TcpListener, UdpSocket) {
    let found = search(
        || {
            let l = TcpListener::bind(local(0))?;
            let port = l.local_addr()?.port();
            Ok((l, port))
        },
        |port| UdpSocket::bind(local(port)),
        || {
            let u = UdpSocket::bind(local(0))?;
            let port = u.local_addr()?.port();
            Ok((u, port))
        },
        |port| TcpListener::bind(local(port)),
    );
    found.unwrap_or_else(|e| panic!("{e}"))
}

/// [`same_port`], its listener made a tokio one. Call it inside a runtime.
///
/// # Panics
/// As [`same_port`], or when the listener cannot be handed to tokio.
#[must_use]
#[allow(
    clippy::expect_used,
    reason = "a test helper: a test with no listener cannot run"
)]
pub fn same_port_tokio() -> (tokio::net::TcpListener, UdpSocket) {
    let (tcp, udp) = same_port();
    tcp.set_nonblocking(true).expect("a nonblocking listener");
    let tcp = tokio::net::TcpListener::from_std(tcp).expect("a tokio listener");
    (tcp, udp)
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        reason = "assertions in tests"
    )]
    use super::*;
    use std::cell::RefCell;
    use std::collections::BTreeSet;
    use std::rc::Rc;

    /// A fake socket that frees its port when dropped.
    struct Fake {
        port: u16,
        bound: Rc<RefCell<BTreeSet<u16>>>,
    }

    impl Drop for Fake {
        fn drop(&mut self) {
            self.bound.borrow_mut().remove(&self.port);
        }
    }

    /// One protocol's ports, handing out the lowest free one from `from`, as Windows hands out
    /// the port just freed.
    #[derive(Clone)]
    struct Ports {
        bound: Rc<RefCell<BTreeSet<u16>>>,
        offered: Rc<RefCell<Vec<u16>>>,
        from: u16,
        refused: fn(u16) -> bool,
    }

    impl Ports {
        fn new(from: u16, refused: fn(u16) -> bool) -> Self {
            Self {
                bound: Rc::default(),
                offered: Rc::default(),
                from,
                refused,
            }
        }

        fn any(&self) -> io::Result<(Fake, u16)> {
            let port = (self.from..)
                .find(|p| !self.bound.borrow().contains(p) && !(self.refused)(*p))
                .expect("a port");
            self.offered.borrow_mut().push(port);
            Ok((self.on(port)?, port))
        }

        fn on(&self, port: u16) -> io::Result<Fake> {
            if (self.refused)(port) || !self.bound.borrow_mut().insert(port) {
                return Err(io::Error::from(io::ErrorKind::AddrInUse));
            }
            Ok(Fake {
                port,
                bound: Rc::clone(&self.bound),
            })
        }
    }

    #[test]
    fn a_port_just_freed_is_not_offered_again() {
        // Another test holds UDP on 1000, the port TCP hands out first.
        let tcp = Ports::new(1000, |_| false);
        let udp = Ports::new(1000, |p| p == 1000);
        let (t, u) =
            search(|| tcp.any(), |p| udp.on(p), || udp.any(), |p| tcp.on(p)).expect("a pair");
        assert_eq!((t.port, u.port), (1001, 1001));
        assert_eq!(*tcp.offered.borrow(), vec![1000, 1001], "1000 offered once");
    }

    #[test]
    fn udp_first_finds_a_pair_when_tcp_first_cannot() {
        // UDP refuses every port TCP hands out (a range it reserves); its own start at 5000.
        let tcp = Ports::new(1000, |_| false);
        let udp = Ports::new(5000, |p| p < 5000);
        let (t, u) =
            search(|| tcp.any(), |p| udp.on(p), || udp.any(), |p| tcp.on(p)).expect("a pair");
        assert_eq!((t.port, u.port), (5000, 5000));
        assert_eq!(
            tcp.offered.borrow().len(),
            TRIES / 2,
            "every TCP-first try was made"
        );
        let distinct: BTreeSet<_> = tcp.offered.borrow().iter().copied().collect();
        assert_eq!(distinct.len(), TRIES / 2, "no port offered twice");
    }

    #[test]
    fn no_pair_names_both_errors() {
        let tcp = Ports::new(1000, |_| false);
        let udp = Ports::new(1000, |_| true);
        let udp_tries = RefCell::new(0);
        let e = search(
            || tcp.any(),
            |p| udp.on(p),
            || {
                *udp_tries.borrow_mut() += 1;
                Err::<(Fake, u16), _>(io::Error::other("udp any refused"))
            },
            |p| tcp.on(p),
        )
        .err()
        .expect("no pair");
        // 64 tries in all: half each way.
        assert_eq!(tcp.offered.borrow().len(), TRIES / 2);
        assert_eq!(*udp_tries.borrow(), TRIES / 2);
        assert!(e.contains("in 64 tries"), "{e}");
        assert!(e.contains("TCP first"), "{e}");
        assert!(e.contains("udp any refused"), "{e}");
    }

    #[test]
    fn the_real_search_finds_a_pair() {
        let (t, u) = same_port();
        assert_eq!(
            t.local_addr().unwrap().port(),
            u.local_addr().unwrap().port()
        );
    }
}
