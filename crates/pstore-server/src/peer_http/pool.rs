//! Bounded process-wide peer connections and deadline-limited transport choice.

use super::{Client, Driver, Error, Limits, PeerTransport, Protocol};
use crate::{
    PeerTls,
    peer_wire::{self, Reply, Request},
};
use pstore_types::TenantId;
use serde::{Serialize, de::DeserializeOwned};
use std::collections::BTreeMap;
use std::sync::{
    Arc, Mutex, OnceLock,
    atomic::{AtomicU64, Ordering},
};
use std::time::Duration;
use tokio::sync::Semaphore;
use tokio::time::Instant;

const IDLE: Duration = Duration::from_secs(30);
const BACKOFF: Duration = Duration::from_secs(30);
const RETRY: Duration = Duration::from_millis(100);
const PROBE: Duration = Duration::from_millis(250);

struct Address {
    key: String,
    host: String,
    port: u16,
}

impl Address {
    fn parse(input: &str) -> Result<Self, Error> {
        if input.len() > 1024 || input.contains('#') {
            return Err(Error::Protocol);
        }
        let uri: axum::http::Uri = input.parse().map_err(|_| Error::Protocol)?;
        if uri.scheme_str() != Some("https") || uri.path() != "/" || uri.query().is_some() {
            return Err(Error::Protocol);
        }
        let authority = uri.authority().ok_or(Error::Protocol)?;
        if authority.as_str().contains('@') {
            return Err(Error::Protocol);
        }
        let raw_host = authority.host();
        let port = match authority.port_u16() {
            Some(port) => port,
            None if authority.as_str() == raw_host
                || authority.as_str() == format!("[{raw_host}]") =>
            {
                443
            }
            _ => return Err(Error::Protocol),
        };
        let host = raw_host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .to_ascii_lowercase();
        if host.is_empty() || port == 0 {
            return Err(Error::Protocol);
        }
        let display = if host.contains(':') {
            format!("[{host}]")
        } else {
            host.clone()
        };
        Ok(Self {
            key: format!("https://{display}:{port}"),
            host,
            port,
        })
    }
}

#[derive(Default)]
struct State {
    client: Option<Arc<Client>>,
    retry: Option<Instant>,
    h3_retry: Option<Instant>,
}
#[derive(Default)]
struct Slot {
    state: tokio::sync::Mutex<State>,
}
struct Entry {
    touched: Instant,
    slot: Arc<Slot>,
}

/// All tables and timers scale with this node's bounded peer set.
pub(crate) struct Pool {
    tls: PeerTls,
    mode: PeerTransport,
    #[cfg(test)]
    timeout: Duration,
    capacity: usize,
    entries: Mutex<BTreeMap<String, Entry>>,
    connections: Arc<Semaphore>,
    requests: Semaphore,
    budget: Arc<Semaphore>,
    ids: AtomicU64,
    h2: AtomicU64,
    h3: AtomicU64,
    probes: AtomicU64,
    fallbacks: AtomicU64,
    sweeper: OnceLock<Driver>,
}

impl std::fmt::Debug for Pool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PeerPool")
            .field("stats", &self.stats())
            .finish_non_exhaustive()
    }
}

#[derive(Debug)]
pub(crate) struct Stats {
    pub(crate) entries: usize,
    pub(crate) connections: usize,
    pub(crate) h2: u64,
    pub(crate) h3: u64,
    pub(crate) probes: u64,
    pub(crate) fallbacks: u64,
}

impl Pool {
    pub(crate) fn canonical(uri: &str) -> Result<String, Error> {
        Address::parse(uri).map(|address| address.key)
    }
    #[cfg(test)]
    pub(crate) fn new(
        tls: PeerTls,
        mode: PeerTransport,
        timeout: Duration,
    ) -> Result<Arc<Self>, Error> {
        Self::with_limits(tls, mode, timeout, Arc::new(Limits::default()))
    }

    #[cfg(test)]
    fn with_capacity(
        tls: PeerTls,
        mode: PeerTransport,
        timeout: Duration,
        capacity: usize,
    ) -> Result<Arc<Self>, Error> {
        Self::with_limits(tls, mode, timeout, Limits::new(capacity)?)
    }

    pub(crate) fn with_limits(
        tls: PeerTls,
        mode: PeerTransport,
        timeout: Duration,
        limits: Arc<Limits>,
    ) -> Result<Arc<Self>, Error> {
        if timeout.is_zero() {
            return Err(Error::Protocol);
        }
        Ok(Arc::new(Self {
            tls,
            mode,
            #[cfg(test)]
            timeout,
            capacity: limits.capacity,
            entries: Mutex::new(BTreeMap::new()),
            connections: limits.connections.clone(),
            requests: Semaphore::new(256),
            budget: limits.budget.clone(),
            ids: AtomicU64::new(1),
            h2: AtomicU64::new(0),
            h3: AtomicU64::new(0),
            probes: AtomicU64::new(0),
            fallbacks: AtomicU64::new(0),
            sweeper: OnceLock::new(),
        }))
    }

    fn start(self: &Arc<Self>) -> Result<(), Error> {
        let runtime = tokio::runtime::Handle::try_current().map_err(|_| Error::Unavailable)?;
        self.sweeper.get_or_init(|| {
            // Start only from a fully constructed Arc. Spawning in new_cyclic
            // lets another runtime thread observe a Weak that cannot upgrade yet.
            let weak = Arc::downgrade(self);
            Driver(runtime.spawn(async move {
                let mut timer = tokio::time::interval(Duration::from_secs(5));
                timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                loop {
                    timer.tick().await;
                    let Some(pool) = weak.upgrade() else {
                        break;
                    };
                    pool.reap(Instant::now());
                }
            }))
        });
        Ok(())
    }

    fn slot(&self, key: &str) -> Result<Arc<Slot>, Error> {
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(entry) = entries.get_mut(key) {
            entry.touched = Instant::now();
            return Ok(entry.slot.clone());
        }
        if entries.len() >= self.capacity {
            let victim = entries
                .iter()
                .filter(|(_, e)| Arc::strong_count(&e.slot) == 1)
                .min_by_key(|(_, e)| e.touched)
                .map(|(key, _)| key.clone())
                .ok_or(Error::Unavailable)?;
            entries.remove(&victim);
        }
        let slot = Arc::new(Slot::default());
        entries.insert(
            key.to_owned(),
            Entry {
                touched: Instant::now(),
                slot: slot.clone(),
            },
        );
        Ok(slot)
    }

    fn reap(&self, now: Instant) {
        self.entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|_, e| now.duration_since(e.touched) < IDLE || Arc::strong_count(&e.slot) > 1);
    }

    pub(crate) fn retain(&self, members: &[String]) {
        let members: std::collections::BTreeSet<String> = members
            .iter()
            .filter_map(|m| Address::parse(m).ok().map(|a| a.key))
            .collect();
        self.entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|key, _| members.contains(key));
    }

    pub(crate) fn stats(&self) -> Stats {
        Stats {
            entries: self
                .entries
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .len(),
            connections: self.capacity - self.connections.available_permits(),
            h2: self.h2.load(Ordering::Relaxed),
            h3: self.h3.load(Ordering::Relaxed),
            probes: self.probes.load(Ordering::Relaxed),
            fallbacks: self.fallbacks.load(Ordering::Relaxed),
        }
    }

    /// One timeout includes queueing, DNS, negotiation, sending and receiving.
    #[cfg(test)]
    pub(crate) async fn call(
        self: &Arc<Self>,
        uri: &str,
        tenant: TenantId,
        body: serde_json::Value,
    ) -> Result<Reply, Error> {
        self.call_typed(uri, tenant, body).await
    }

    #[cfg(test)]
    pub(crate) async fn call_typed<Q: Serialize + Send, R: DeserializeOwned + Send>(
        self: &Arc<Self>,
        uri: &str,
        tenant: TenantId,
        body: Q,
    ) -> Result<Reply<R>, Error> {
        self.call_with_timeout(uri, tenant, body, self.timeout)
            .await
    }

    pub(crate) async fn call_with_timeout<Q: Serialize + Send, R: DeserializeOwned + Send>(
        self: &Arc<Self>,
        uri: &str,
        tenant: TenantId,
        body: Q,
        timeout: Duration,
    ) -> Result<Reply<R>, Error> {
        let address = Address::parse(uri)?;
        if timeout.is_zero() {
            return Err(Error::Deadline);
        }
        let deadline = Instant::now().checked_add(timeout).ok_or(Error::Deadline)?;
        self.start()?;
        tokio::time::timeout_at(deadline, async {
            let _request = self
                .requests
                .acquire()
                .await
                .map_err(|_| Error::Unavailable)?;
            let slot = self.slot(&address.key)?;
            let client = self.client_before(&slot, &address, deadline).await?;
            let id = self
                .ids
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
                .map_err(|_| Error::Protocol)?;
            let result = client
                .call_typed(Request {
                    version: peer_wire::VERSION,
                    id,
                    tenant: tenant.0.to_string(),
                    body,
                })
                .await;
            if result.is_err() {
                let mut state = slot.state.lock().await;
                if state
                    .client
                    .as_ref()
                    .is_some_and(|current| Arc::ptr_eq(current, &client))
                {
                    state.client = None;
                    state.retry = Some(Instant::now() + RETRY);
                }
            }
            result
        })
        .await
        .map_err(|_| Error::Deadline)?
    }

    #[cfg(test)]
    async fn client(&self, slot: &Slot, address: &Address) -> Result<Arc<Client>, Error> {
        let deadline = Instant::now()
            .checked_add(self.timeout)
            .ok_or(Error::Deadline)?;
        self.client_before(slot, address, deadline).await
    }

    async fn client_before(
        &self,
        slot: &Slot,
        address: &Address,
        deadline: Instant,
    ) -> Result<Arc<Client>, Error> {
        let mut state = slot.state.lock().await;
        if let Some(client) = &state.client {
            return Ok(client.clone());
        }
        let now = Instant::now();
        if state.retry.is_some_and(|retry| now < retry) {
            return Err(Error::Unavailable);
        }
        // DNS is resolved once per new connection, never once per record/index.
        let resolved = tokio::net::lookup_host((address.host.as_str(), address.port))
            .await
            .map_err(|_| Error::Unavailable)?
            .take(8)
            .collect::<Vec<_>>();
        let mut result = Err(Error::Unavailable);
        for socket in resolved {
            let next = match self.mode {
                PeerTransport::H2 => {
                    self.connect(socket, &address.host, Protocol::H2, None)
                        .await
                }
                PeerTransport::H3 => {
                    self.connect(socket, &address.host, Protocol::H3, None)
                        .await
                }
                PeerTransport::Auto => {
                    if state.h3_retry.is_some_and(|retry| Instant::now() < retry) {
                        self.connect(socket, &address.host, Protocol::H2, None)
                            .await
                    } else {
                        let h3 = self
                            .connect(socket, &address.host, Protocol::H3, Some(deadline))
                            .await;
                        match h3 {
                            Err(Error::Unavailable) => {
                                state.h3_retry = Some(Instant::now() + BACKOFF);
                                self.fallbacks.fetch_add(1, Ordering::Relaxed);
                                self.connect(socket, &address.host, Protocol::H2, None)
                                    .await
                            }
                            other => other,
                        }
                    }
                }
            };
            match next {
                Ok(client) => {
                    state.client = Some(client.clone());
                    state.retry = None;
                    return Ok(client);
                }
                Err(Error::Unavailable) => {
                    result = Err(Error::Unavailable);
                }
                Err(Error::Deadline) => return Err(Error::Deadline),
                Err(error) => {
                    state.retry = Some(Instant::now() + RETRY);
                    return Err(error);
                }
            }
        }
        state.retry = Some(Instant::now() + RETRY);
        result
    }

    async fn connect(
        &self,
        address: std::net::SocketAddr,
        name: &str,
        protocol: Protocol,
        probe: Option<Instant>,
    ) -> Result<Arc<Client>, Error> {
        let admission = self
            .connections
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| Error::Unavailable)?;
        // Queueing is still inside the caller's overall deadline, but cannot
        // consume the network probe budget or suggest UDP is unavailable.
        let probe = match probe {
            Some(deadline) => {
                let remaining = deadline
                    .checked_duration_since(Instant::now())
                    .filter(|duration| !duration.is_zero())
                    .ok_or(Error::Deadline)?;
                self.probes.fetch_add(1, Ordering::Relaxed);
                Some(PROBE.min(remaining / 4))
            }
            None => None,
        };
        let connecting = Client::connect_admitted(
            address,
            name,
            &self.tls,
            protocol,
            self.budget.clone(),
            Some(admission),
        );
        let client = match probe {
            Some(limit) => tokio::time::timeout(limit, connecting)
                .await
                .unwrap_or(Err(Error::Unavailable))?,
            None => connecting.await?,
        };
        match protocol {
            Protocol::H2 => self.h2.fetch_add(1, Ordering::Relaxed),
            Protocol::H3 => self.h3.fetch_add(1, Ordering::Relaxed),
        };
        Ok(Arc::new(client))
    }
}

#[cfg(test)]
mod tests;
