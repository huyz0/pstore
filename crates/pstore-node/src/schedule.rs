//! When a node heals, reports and counts (M31): the loop's decisions, out of `main`.
//!
//! ⚠️ **Counted in views, and a view is not a poll.** The loop polls the membership every
//! `poll`, and once a second at most it reports a view; healing and ownership are judged on
//! views. `heal_every` used to be `HEAL_PERIOD / poll` -- a count of polls -- compared with a
//! count of views, so at the 200 ms poll a fleet measures at a node healed every 50 s instead
//! of every 10 s, under a comment in `main` saying the period bug had been fixed.

use crate::policy::jitter;
use crate::{DEFAULT_GOSSIP_PERIOD, HEAL_PERIOD};
use std::time::Duration;

/// How often, in polls and views, a node reports and acts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cadence {
    /// Polls per view: a view is reported once a second at most.
    pub view_every: u64,
    /// Views per heal, so that a heal is `HEAL_PERIOD` of wall time.
    pub heal_every: u64,
    /// The view, modulo `heal_every`, this node heals on: spread by its id, so the fleet
    /// does not read the roster at one instant.
    pub heal_slot: u64,
    /// Views per ownership report; 0 never reports.
    pub owns_every: u64,
}

impl Cadence {
    /// The cadence at a poll interval, an ownership period in seconds (0 for none) and a
    /// node id. ⚠️ Each count rounds **down**, at least 1: a report is never later than asked.
    #[must_use]
    pub fn new(poll: Duration, owns_period_s: u64, node_id: &str) -> Self {
        let poll_ms = poll.as_millis().max(1);
        let view_every = (1000 / poll_ms).max(1) as u64;
        let view_ms = poll_ms * u128::from(view_every);
        let heal_every = (HEAL_PERIOD.as_millis() / view_ms).max(1) as u64;
        let owns_every = if owns_period_s == 0 {
            0
        } else {
            (u128::from(owns_period_s) * 1000 / view_ms).max(1) as u64
        };
        Self {
            view_every,
            heal_every,
            heal_slot: jitter(node_id, heal_every),
            owns_every,
        }
    }
}

/// What one poll asks of the loop.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Tick {
    /// The membership size, when it moved since the last poll (and on the first).
    pub changed: Option<usize>,
    /// The view number to report, numbered from 1, on a view poll.
    pub view: Option<u64>,
    /// Re-read the roster: dial what it knows and gossip does not, publish what gossip adds.
    pub heal: bool,
    /// Report what this node would own.
    pub owns: bool,
}

/// The loop's counters.
#[derive(Debug, Clone)]
pub struct Clock {
    cadence: Cadence,
    polls: u64,
    views: u64,
    /// ⚠️ `usize::MAX` to start, as `main`'s was: the first poll is always a change, and a
    /// size of `usize::MAX` never is.
    last_size: usize,
}

impl Clock {
    /// A clock at `cadence`, before its first poll.
    #[must_use]
    pub fn new(cadence: Cadence) -> Self {
        Self {
            cadence,
            polls: 0,
            views: 0,
            last_size: usize::MAX,
        }
    }

    /// One poll, whose membership size is `size`. ⚠️ The view count moves BEFORE the heal and
    /// ownership checks, so the first view is checked at 1, never at 0.
    pub fn tick(&mut self, size: usize) -> Tick {
        self.polls += 1;
        let mut t = Tick::default();
        if size != self.last_size {
            self.last_size = size;
            t.changed = Some(size);
        }
        if !self.polls.is_multiple_of(self.cadence.view_every) {
            return t;
        }
        self.views += 1;
        t.view = Some(self.views);
        t.heal = self.views % self.cadence.heal_every == self.cadence.heal_slot;
        // A zero period never reports: `is_multiple_of(0)` holds only for 0, and the view
        // count is at least 1 here.
        t.owns = self.views.is_multiple_of(self.cadence.owns_every);
        t
    }
}

/// The gossip period and the poll period from their raw settings: a positive count of
/// milliseconds, else the default, and the poll falls back to the gossip period.
#[must_use]
pub fn periods(gossip_ms: Option<&str>, poll_ms: Option<&str>) -> (Duration, Duration) {
    let positive = |raw: Option<&str>| {
        raw.and_then(|v| v.parse::<u64>().ok())
            .filter(|ms| *ms > 0)
            .map(Duration::from_millis)
    };
    let gossip = positive(gossip_ms).unwrap_or(DEFAULT_GOSSIP_PERIOD);
    (gossip, positive(poll_ms).unwrap_or(gossip))
}

/// The injected probe loss (M4b criterion 3): zero unless asked.
#[must_use]
pub fn probe_loss(raw: Option<&str>) -> f64 {
    raw.and_then(|v| v.parse().ok()).unwrap_or(0.0)
}

/// The ownership report's period in seconds: 5 unless set; 0 turns it off.
#[must_use]
pub fn owns_period(raw: Option<&str>) -> u64 {
    raw.and_then(|v| v.parse().ok()).unwrap_or(5)
}

/// Whether to run `chitchat` rather than SWIM: only when asked for by exactly that name.
#[must_use]
pub fn chitchat(raw: Option<&str>) -> bool {
    raw == Some("chitchat")
}

/// The gossip key's text (M53): `PSTORE_GOSSIP_KEY`, or the file `PSTORE_GOSSIP_KEY_FILE`
/// names, its trailing whitespace trimmed.
///
/// # Errors
/// Both set, or a file that cannot be read.
pub fn gossip_key_source(key: Option<&str>, file: Option<&str>) -> Result<Option<String>, String> {
    match (key, file) {
        (Some(_), Some(_)) => {
            Err("set PSTORE_GOSSIP_KEY or PSTORE_GOSSIP_KEY_FILE, not both".to_owned())
        }
        (Some(k), None) => Ok(Some(k.to_owned())),
        (None, Some(path)) => std::fs::read_to_string(path)
            .map(|s| Some(s.trim_end().to_owned()))
            .map_err(|e| format!("PSTORE_GOSSIP_KEY_FILE {path}: {e}")),
        (None, None) => Ok(None),
    }
}

/// The gossip keys (M53): one or two, hex, comma-separated, each at least 32 bytes. The first
/// seals and either opens, so a key rotates through a running fleet in three passes.
///
/// ⚠️ **No key is refused** unless `insecure` is exactly `"1"`: an unauthenticated fleet
/// believes any datagram, and that is a choice to make out loud, not a default to inherit.
///
/// # Errors
/// No key and no `"1"`; a key with chitchat, which cannot honour it; and a malformed key.
pub fn gossip_keys(
    key: Option<&str>,
    insecure: Option<&str>,
    chitchat: bool,
) -> Result<Option<Vec<Vec<u8>>>, String> {
    let Some(text) = key else {
        return if insecure == Some("1") {
            Ok(None)
        } else {
            Err("gossip needs PSTORE_GOSSIP_KEY or PSTORE_GOSSIP_KEY_FILE, \
                 or PSTORE_GOSSIP_INSECURE=1 to run unauthenticated"
                .to_owned())
        };
    };
    if chitchat {
        return Err("PSTORE_GOSSIP_KEY is SWIM's: chitchat cannot seal".to_owned());
    }
    let keys: Vec<Vec<u8>> = text
        .split(',')
        .map(|k| hex(k.trim()))
        .collect::<Option<_>>()
        .ok_or("PSTORE_GOSSIP_KEY: not hex")?;
    if keys.len() > 2 {
        return Err("PSTORE_GOSSIP_KEY: at most two keys".to_owned());
    }
    if keys.iter().any(|k| k.len() < 32) {
        return Err("PSTORE_GOSSIP_KEY: each key at least 32 bytes, 64 hex digits".to_owned());
    }
    Ok(Some(keys))
}

/// `text` as bytes, two hex digits each; `None` for anything else.
fn hex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) || !text.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(text.get(i..i + 2)?, 16).ok())
        .collect()
}
