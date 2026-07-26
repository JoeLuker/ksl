//! Polling watcher: repeatedly runs a search, detects listings not seen
//! before, and appends them as events to a local JSONL file.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::client::KslClient;
use crate::error::{Error, Result};
use crate::models::{ListingSummary, SearchQuery};

/// Consecutive failed polls tolerated before the watcher gives up.
const MAX_CONSECUTIVE_ERRORS: u32 = 5;
/// Fraction of the poll interval used as random spread.
const JITTER_SPREAD: f64 = 0.15;
/// Continuation pages fetched per poll when everything on a page is new.
const MAX_PAGES_PER_POLL: u32 = 5;

#[derive(Debug, Default, Serialize, Deserialize)]
struct WatchState {
    seen: BTreeSet<u64>,
}

#[derive(Debug, Serialize)]
pub struct NewListingEvent<'a> {
    /// Unix seconds when the watcher first saw the listing.
    pub seen_at: u64,
    pub watch: &'a str,
    #[serde(flatten)]
    pub listing: &'a ListingSummary,
}

/// Multiple watchers can share one [`KslClient`] (via `Arc`); the client's
/// min-interval throttle then spaces requests globally across all of them,
/// which is what keeps a multi-watch daemon polite.
pub struct Watcher {
    client: Arc<KslClient>,
    query: SearchQuery,
    interval: Duration,
    name: String,
    state_path: PathBuf,
    events_path: PathBuf,
    state: WatchState,
}

impl Watcher {
    pub fn new(
        client: Arc<KslClient>,
        query: SearchQuery,
        interval: Duration,
        name: String,
        state_dir: &Path,
    ) -> Result<Self> {
        std::fs::create_dir_all(state_dir)?;
        let state_path = state_dir.join(format!("{name}.state.json"));
        let events_path = state_dir.join(format!("{name}.events.jsonl"));
        let state = match std::fs::read(&state_path) {
            Ok(bytes) => serde_json::from_slice(&bytes)?,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => WatchState::default(),
            Err(err) => return Err(err.into()),
        };
        Ok(Watcher {
            client,
            query,
            interval,
            name,
            state_path,
            events_path,
            state,
        })
    }

    /// Poll forever (until ctrl-c). The first poll only baselines: everything
    /// currently matching is marked seen without emitting events, so a fresh
    /// watch doesn't flood the event log with the existing inventory.
    pub async fn run(mut self) -> Result<()> {
        let mut consecutive_errors = 0u32;
        loop {
            // An empty seen-set means this watch has no watermark yet; the
            // poll only baselines the current inventory.
            match self.poll(self.state.seen.is_empty()).await {
                Ok(new_count) => {
                    consecutive_errors = 0;
                    tracing::info!(
                        watch = %self.name,
                        new = new_count,
                        known = self.state.seen.len(),
                        "poll complete"
                    );
                }
                Err(err) => {
                    consecutive_errors += 1;
                    tracing::error!(
                        watch = %self.name,
                        %err,
                        consecutive_errors,
                        "poll failed"
                    );
                    if consecutive_errors >= MAX_CONSECUTIVE_ERRORS {
                        return Err(err);
                    }
                }
            }
            tokio::select! {
                _ = tokio::time::sleep(jittered(self.interval)) => {}
                _ = tokio::signal::ctrl_c() => {
                    tracing::info!(watch = %self.name, "interrupted; stopping");
                    return Ok(());
                }
            }
        }
    }

    /// One poll. Returns how many previously-unseen listings were found.
    async fn poll(&mut self, baseline: bool) -> Result<usize> {
        // A baseline poll only needs a watermark (the newest page); walking
        // deeper is wasted traffic that risks tripping the site's rate limits.
        let max_pages = if baseline { 1 } else { MAX_PAGES_PER_POLL };
        let mut new_listings: Vec<ListingSummary> = Vec::new();
        let mut page = self.client.search(&self.query).await?;
        let mut pages_fetched = 1;
        loop {
            let fresh: Vec<ListingSummary> = page
                .items
                .iter()
                .filter(|item| !self.state.seen.contains(&item.id))
                .cloned()
                .collect();
            let page_fully_new = !page.items.is_empty() && fresh.len() == page.items.len();
            new_listings.extend(fresh);
            // Newest-first ordering: once a page contains anything already
            // seen, everything deeper is older than our watermark.
            if !page_fully_new || !page.page_info.has_next_page || pages_fetched >= max_pages {
                break;
            }
            let cursor = page
                .page_info
                .end_cursor
                .clone()
                .ok_or_else(|| Error::Parse("hasNextPage without endCursor".into()))?;
            page = self.client.search_after(&self.query, &cursor).await?;
            pages_fetched += 1;
        }

        if baseline {
            tracing::info!(
                watch = %self.name,
                count = new_listings.len(),
                "baseline poll; recording current inventory without events"
            );
        } else if !new_listings.is_empty() {
            self.append_events(&new_listings)?;
        }
        for listing in &new_listings {
            self.state.seen.insert(listing.id);
        }
        self.save_state()?;
        Ok(if baseline { 0 } else { new_listings.len() })
    }

    fn append_events(&self, listings: &[ListingSummary]) -> Result<()> {
        use std::io::Write;
        let seen_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before unix epoch")
            .as_secs();
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.events_path)?;
        for listing in listings {
            let event = NewListingEvent {
                seen_at,
                watch: &self.name,
                listing,
            };
            serde_json::to_writer(&mut file, &event)?;
            file.write_all(b"\n")?;
            tracing::info!(
                id = listing.id,
                title = %listing.title,
                price = listing.price,
                url = %listing.url(),
                "new listing"
            );
        }
        Ok(())
    }

    fn save_state(&self) -> Result<()> {
        let tmp = self.state_path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(&self.state)?)?;
        std::fs::rename(&tmp, &self.state_path)?;
        Ok(())
    }
}

/// Spread a poll interval by ±[`JITTER_SPREAD`].
///
/// Two reasons, both about not behaving like a machine: a metronome-exact
/// cadence is itself a bot signal, and several watches started together would
/// otherwise stay synchronized forever, hitting the site in bursts instead of
/// spread out. The sub-second clock is ample entropy for de-synchronizing.
fn jittered(interval: Duration) -> Duration {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    // Nanoseconds within the current second, mapped to [-1.0, 1.0).
    let unit = f64::from(nanos) / 500_000_000.0 - 1.0;
    interval.mul_f64(1.0 + JITTER_SPREAD * unit)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jitter_stays_within_spread() {
        let base = Duration::from_secs(600);
        let low = base.mul_f64(1.0 - JITTER_SPREAD);
        let high = base.mul_f64(1.0 + JITTER_SPREAD);
        for _ in 0..1_000 {
            let actual = jittered(base);
            assert!(
                actual >= low && actual <= high,
                "{actual:?} outside {low:?}..={high:?}"
            );
        }
    }

    #[test]
    fn jitter_actually_varies() {
        let base = Duration::from_secs(600);
        let mut seen = std::collections::BTreeSet::new();
        for _ in 0..50 {
            seen.insert(jittered(base).as_nanos());
            std::thread::yield_now();
        }
        assert!(seen.len() > 1, "jitter produced a constant value");
    }
}
