//! What each relay did the last time this device used it (`relay-health.js`).
//!
//! A relay can accept the connection and then refuse every event, so each
//! one is tested by sending ourselves one sealed envelope through it. A
//! refusal is remembered so the next session skips that relay for a while;
//! a timeout isn't, because it may be this network. The book is kept per
//! device by the caller (serialise it with serde) and never sent anywhere.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

const SKIP_FOR_MS: u64 = 6 * 60 * 60 * 1000;
const FORGET_AFTER_MS: u64 = 7 * 24 * 60 * 60 * 1000;
const MAX_ENTRIES: usize = 16;
const WHY_MAX: usize = 120;

/// A relay's last verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Verdict {
    /// Carried our test envelope.
    Ok,
    /// Refused it.
    Refused,
    /// Accepted it and never delivered it.
    NoEcho,
    /// Could not be reached.
    Unreachable,
}

/// One relay's entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Health {
    /// The verdict.
    pub v: Verdict,
    /// Why, as the relay said it.
    pub why: String,
    /// Round trip of the test, milliseconds.
    pub ms: Option<u64>,
    /// When, epoch milliseconds.
    pub at: u64,
}

/// Every relay's last verdict, keyed by URL.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthBook(BTreeMap<String, Health>);

impl HealthBook {
    /// Whether no relay has a verdict yet.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Records a verdict, keeping the 16 most recent relays.
    pub fn record(&mut self, url: &str, v: Verdict, why: &str, ms: Option<u64>, now_ms: u64) {
        self.forget_old(now_ms);
        self.0.insert(
            url.to_owned(),
            Health {
                v,
                why: why.chars().take(WHY_MAX).collect(),
                ms,
                at: now_ms,
            },
        );
        while self.0.len() > MAX_ENTRIES {
            let Some(oldest) = self
                .0
                .iter()
                .min_by_key(|(_, h)| h.at)
                .map(|(k, _)| k.clone())
            else {
                break;
            };
            self.0.remove(&oldest);
        }
    }

    fn forget_old(&mut self, now_ms: u64) {
        self.0
            .retain(|url, h| url.len() <= 200 && now_ms.saturating_sub(h.at) <= FORGET_AFTER_MS);
    }

    /// The entry for a relay, if fresh.
    #[must_use]
    pub fn get(&self, url: &str, now_ms: u64) -> Option<&Health> {
        self.0
            .get(url)
            .filter(|h| now_ms.saturating_sub(h.at) <= FORGET_AFTER_MS)
    }

    fn skip(&self, url: &str, now_ms: u64) -> bool {
        self.get(url, now_ms).is_some_and(|h| {
            matches!(h.v, Verdict::Refused | Verdict::NoEcho)
                && now_ms.saturating_sub(h.at) < SKIP_FOR_MS
        })
    }

    /// The relays worth dialling, and the ones left out with why. If every
    /// relay is marked bad, all are tried anyway: stale knowledge must never
    /// leave the user with no Nostr at all.
    #[must_use]
    pub fn pick(&self, urls: &[String], now_ms: u64) -> (Vec<String>, Vec<(String, String)>) {
        let use_: Vec<String> = urls
            .iter()
            .filter(|u| !self.skip(u, now_ms))
            .cloned()
            .collect();
        if use_.is_empty() {
            return (urls.to_vec(), Vec::new());
        }
        let skipped = urls
            .iter()
            .filter(|u| self.skip(u, now_ms))
            .map(|u| {
                let why = self
                    .get(u, now_ms)
                    .map(|h| {
                        if h.why.is_empty() {
                            format!("{:?}", h.v)
                        } else {
                            h.why.clone()
                        }
                    })
                    .unwrap_or_default();
                (u.clone(), why)
            })
            .collect();
        (use_, skipped)
    }
}
