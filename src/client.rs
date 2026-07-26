use std::path::PathBuf;
use std::time::Duration;

use tokio::sync::Mutex;
use tokio::time::Instant;

use crate::discovery::{self, BASE_URL};
use crate::error::{Error, Result};
use crate::flight;
use crate::jsonld;
use crate::models::{ListingDetail, SearchPage, SearchQuery};

/// We identify ourselves honestly rather than impersonating a browser.
/// Our TLS/HTTP2 fingerprint is plainly not a browser's, so a browser
/// User-Agent would be a transparent inconsistency; a truthful one also lets
/// the site block or contact us deliberately if it objects to this traffic.
const USER_AGENT: &str = concat!(
    env!("CARGO_PKG_NAME"),
    "/",
    env!("CARGO_PKG_VERSION"),
    " (+personal use; low volume; respects Retry-After)"
);

/// How long to stay away after the site blocks us hard enough to exhaust
/// retries. Continued probing during a block appears to extend it.
const DEFAULT_BLOCK_COOLOFF: Duration = Duration::from_secs(30 * 60);

/// Ceiling on a server-supplied `Retry-After`, so a hostile or mistaken
/// value cannot park the client indefinitely.
const MAX_RETRY_AFTER: Duration = Duration::from_secs(15 * 60);

/// Client for KSL Classifieds.
///
/// Reads go through two public surfaces of the site:
/// - search: the Next.js `search` server action (JSON in a flight payload)
/// - listing detail: schema.org `Product` JSON-LD on the listing page
///
/// The client owns all politeness: requests are spaced by `min_interval`,
/// throttling responses are retried with exponential backoff (honoring
/// `Retry-After` when present), and exhausting retries opens a circuit
/// breaker that keeps us off the site entirely for `block_cooloff`.
pub struct KslClient {
    http: reqwest::Client,
    min_interval: Duration,
    max_retries: u32,
    block_cooloff: Duration,
    /// Serializes request pacing: holds the last request time and, when the
    /// circuit breaker is open, the instant we may resume.
    pacing: Mutex<Pacing>,
    action_id: Mutex<Option<String>>,
    action_cache_path: Option<PathBuf>,
}

#[derive(Default)]
struct Pacing {
    last_request: Option<Instant>,
    blocked_until: Option<Instant>,
}

pub struct KslClientBuilder {
    min_interval: Duration,
    max_retries: u32,
    block_cooloff: Duration,
    action_id: Option<String>,
    action_cache_path: Option<PathBuf>,
}

impl Default for KslClientBuilder {
    fn default() -> Self {
        KslClientBuilder {
            min_interval: Duration::from_secs(2),
            max_retries: 3,
            block_cooloff: DEFAULT_BLOCK_COOLOFF,
            action_id: None,
            action_cache_path: None,
        }
    }
}

impl KslClientBuilder {
    /// Minimum spacing between any two requests (default 2s).
    pub fn min_interval(mut self, interval: Duration) -> Self {
        self.min_interval = interval;
        self
    }

    /// Retries after throttling responses (default 3, backoff 2s/8s/32s).
    pub fn max_retries(mut self, retries: u32) -> Self {
        self.max_retries = retries;
        self
    }

    /// How long to stay off the site after retries are exhausted
    /// (default 30 minutes).
    pub fn block_cooloff(mut self, cooloff: Duration) -> Self {
        self.block_cooloff = cooloff;
        self
    }

    /// Pin a known server-action id, skipping discovery.
    pub fn action_id(mut self, id: impl Into<String>) -> Self {
        self.action_id = Some(id.into());
        self
    }

    /// File to persist the discovered action id across runs.
    pub fn action_cache_path(mut self, path: PathBuf) -> Self {
        self.action_cache_path = Some(path);
        self
    }

    pub fn build(self) -> Result<KslClient> {
        let http = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            // Honor Set-Cookie within a session, as any correct HTTP client
            // should. Persisting cookies to disk was tried and removed: the
            // site re-issues its `_pxhd` device cookie on every response to
            // clients that don't run its JS sensor, so a stored copy is never
            // recognized and buys nothing.
            .cookie_store(true)
            .gzip(true)
            .timeout(Duration::from_secs(30))
            .build()?;
        let cached = self.action_id.or_else(|| {
            self.action_cache_path
                .as_deref()
                .and_then(|p| std::fs::read_to_string(p).ok())
                .map(|s| s.trim().to_string())
                .filter(|s| (32..=64).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_hexdigit()))
        });
        Ok(KslClient {
            http,
            min_interval: self.min_interval,
            max_retries: self.max_retries,
            block_cooloff: self.block_cooloff,
            pacing: Mutex::new(Pacing::default()),
            action_id: Mutex::new(cached),
            action_cache_path: self.action_cache_path,
        })
    }
}

/// What to do about a response, decided from its status and headers.
enum Disposition {
    /// 200 with the content we asked for.
    Usable,
    /// Throttle/block/challenge: wait this long, then retry.
    Backoff(Duration),
    /// Anything else: surface it.
    Fatal(u16),
}

impl KslClient {
    pub fn builder() -> KslClientBuilder {
        KslClientBuilder::default()
    }

    pub fn new() -> Result<Self> {
        Self::builder().build()
    }

    /// First page of results for a query.
    pub async fn search(&self, query: &SearchQuery) -> Result<SearchPage> {
        self.search_page(query, None).await
    }

    /// Continuation page: pass the previous page's `end_cursor`.
    pub async fn search_after(&self, query: &SearchQuery, cursor: &str) -> Result<SearchPage> {
        self.search_page(query, Some(cursor)).await
    }

    /// Listing detail from the listing page's Product JSON-LD.
    pub async fn listing(&self, id: u64) -> Result<ListingDetail> {
        let url = format!("{BASE_URL}/listing/{id}");
        let body = self.get_with_retry(&url).await?;
        jsonld::parse_listing(id, &body)
    }

    async fn search_page(&self, query: &SearchQuery, cursor: Option<&str>) -> Result<SearchPage> {
        let action_id = self.ensure_action_id().await?;
        match self.post_search(query, cursor, &action_id).await {
            Err(Error::Parse(reason)) => {
                // A stale action id after a site deploy comes back as a
                // payload without a result row. Re-discover once and retry.
                tracing::warn!(%reason, "search parse failed; re-discovering action id");
                let fresh = self.rediscover_action_id().await?;
                self.post_search(query, cursor, &fresh).await
            }
            other => other,
        }
    }

    async fn post_search(
        &self,
        query: &SearchQuery,
        cursor: Option<&str>,
        action_id: &str,
    ) -> Result<SearchPage> {
        let url = self.search_url(query);
        let body = serde_json::to_string(&serde_json::json!([
            null,
            query.sort.index().to_string(),
            {
                "featuredCount": 0,
                "featuredPosition": 0,
                "spotlightCount": 0,
                "spotlightPosition": 10,
                "listingCount": query.page_size,
            },
            query.filter_object(),
            cursor.unwrap_or(""),
        ]))?;

        let mut attempt = 0;
        loop {
            self.throttle().await;
            tracing::debug!(%url, attempt, "search request");
            let resp = self
                .http
                .post(&url)
                .header("Accept", "text/x-component")
                .header("next-action", action_id)
                .header("Content-Type", "text/plain;charset=UTF-8")
                .body(body.clone())
                .send()
                .await?;
            let status = resp.status().as_u16();
            match self.disposition(&resp, attempt, Some("text/x-component")) {
                Disposition::Usable => {
                    let text = resp.text().await?;
                    let page = flight::parse_search_response(&text)?;
                    tracing::debug!(
                        items = page.items.len(),
                        total = page.page_info.total,
                        "search response"
                    );
                    return Ok(page);
                }
                Disposition::Backoff(wait) => {
                    attempt += 1;
                    tracing::warn!(status, ?wait, attempt, "throttled; backing off");
                    tokio::time::sleep(wait).await;
                }
                Disposition::Fatal(status) if is_throttling(status) => {
                    self.open_circuit_breaker().await;
                    return Err(Error::Throttled { status });
                }
                Disposition::Fatal(status) => return Err(Error::Status { status, url }),
            }
        }
    }

    async fn get_with_retry(&self, url: &str) -> Result<String> {
        let mut attempt = 0;
        loop {
            self.throttle().await;
            let resp = self.http.get(url).send().await?;
            let status = resp.status().as_u16();
            match self.disposition(&resp, attempt, None) {
                Disposition::Usable => return Ok(resp.text().await?),
                Disposition::Backoff(wait) => {
                    attempt += 1;
                    tracing::warn!(status, ?wait, attempt, %url, "throttled; backing off");
                    tokio::time::sleep(wait).await;
                }
                Disposition::Fatal(status) if is_throttling(status) => {
                    self.open_circuit_breaker().await;
                    return Err(Error::Throttled { status });
                }
                Disposition::Fatal(status) => {
                    return Err(Error::Status {
                        status,
                        url: url.to_string(),
                    });
                }
            }
        }
    }

    /// Classify a response. `expect_content_type`, when given, guards against
    /// bot-challenge pages: those are served as HTTP 200 HTML, and treating
    /// one as a real response would misread it as a stale action id.
    fn disposition(
        &self,
        resp: &reqwest::Response,
        attempt: u32,
        expect_content_type: Option<&str>,
    ) -> Disposition {
        let status = resp.status().as_u16();
        let content_type_ok = expect_content_type.is_none_or(|expected| {
            resp.headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .is_some_and(|ct| ct.starts_with(expected))
        });
        if status == 200 && content_type_ok {
            return Disposition::Usable;
        }
        if status != 200 && !is_throttling(status) {
            return Disposition::Fatal(status);
        }
        if attempt >= self.max_retries {
            return Disposition::Fatal(status);
        }
        // Prefer the server's own instruction over our guess.
        let wait = retry_after(resp.headers()).unwrap_or_else(|| backoff_delay(attempt));
        Disposition::Backoff(wait)
    }

    async fn open_circuit_breaker(&self) {
        let resume_at = Instant::now() + self.block_cooloff;
        self.pacing.lock().await.blocked_until = Some(resume_at);
        tracing::warn!(
            cooloff_secs = self.block_cooloff.as_secs(),
            "blocked by the site; staying away until the cool-off expires"
        );
    }

    fn search_url(&self, query: &SearchQuery) -> String {
        let sort = query.sort.index();
        match query.keyword.as_deref().filter(|k| !k.is_empty()) {
            Some(kw) => {
                let encoded: String = url_escape(kw);
                format!("{BASE_URL}/v2/search/keyword/{encoded}/sort/{sort}")
            }
            None => format!("{BASE_URL}/v2/search/sort/{sort}"),
        }
    }

    async fn ensure_action_id(&self) -> Result<String> {
        let mut guard = self.action_id.lock().await;
        if let Some(id) = guard.as_ref() {
            return Ok(id.clone());
        }
        self.throttle().await;
        let id = discovery::discover(&self.http).await?;
        self.persist_action_id(&id);
        *guard = Some(id.clone());
        Ok(id)
    }

    async fn rediscover_action_id(&self) -> Result<String> {
        let mut guard = self.action_id.lock().await;
        self.throttle().await;
        let id = discovery::discover(&self.http).await?;
        self.persist_action_id(&id);
        *guard = Some(id.clone());
        Ok(id)
    }

    fn persist_action_id(&self, id: &str) {
        let Some(path) = &self.action_cache_path else {
            return;
        };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Err(err) = std::fs::write(path, id) {
            tracing::warn!(%err, path = %path.display(), "failed to cache action id");
        }
    }

    /// Enforce request spacing, and wait out an open circuit breaker.
    async fn throttle(&self) {
        let mut pacing = self.pacing.lock().await;
        if let Some(resume_at) = pacing.blocked_until {
            let now = Instant::now();
            if resume_at > now {
                let wait = resume_at - now;
                tracing::info!(
                    wait_secs = wait.as_secs(),
                    "circuit breaker open; waiting out the block"
                );
                tokio::time::sleep(wait).await;
            }
            pacing.blocked_until = None;
        }
        if let Some(prev) = pacing.last_request {
            let elapsed = prev.elapsed();
            if elapsed < self.min_interval {
                tokio::time::sleep(self.min_interval - elapsed).await;
            }
        }
        pacing.last_request = Some(Instant::now());
    }
}

fn is_throttling(status: u16) -> bool {
    matches!(status, 403 | 429 | 500..=599)
}

/// 2s, 8s, 32s, ... — quadrupling, so a persistent block is abandoned fast
/// rather than being probed at a steady rate.
fn backoff_delay(attempt: u32) -> Duration {
    Duration::from_secs(2u64.pow(2 * attempt + 1))
}

/// `Retry-After` as delta-seconds (the form servers actually send), capped.
fn retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    let secs: u64 = headers
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse()
        .ok()?;
    Some(Duration::from_secs(secs).min(MAX_RETRY_AFTER))
}

fn url_escape(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    for b in segment.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::Sort;
    use reqwest::header::{HeaderMap, HeaderValue};

    #[test]
    fn search_url_encodes_keyword() {
        let client = KslClient::new().unwrap();
        let q = SearchQuery::keyword("fishing kayak").sort(Sort::PriceLowToHigh);
        assert_eq!(
            client.search_url(&q),
            "https://classifieds.ksl.com/v2/search/keyword/fishing%20kayak/sort/2"
        );
    }

    #[test]
    fn search_url_without_keyword() {
        let client = KslClient::new().unwrap();
        let q = SearchQuery::default();
        assert_eq!(
            client.search_url(&q),
            "https://classifieds.ksl.com/v2/search/sort/0"
        );
    }

    #[test]
    fn user_agent_is_honest() {
        assert!(USER_AGENT.starts_with("ksl/"));
        assert!(!USER_AGENT.contains("Mozilla"));
    }

    #[test]
    fn backoff_quadruples() {
        assert_eq!(backoff_delay(0), Duration::from_secs(2));
        assert_eq!(backoff_delay(1), Duration::from_secs(8));
        assert_eq!(backoff_delay(2), Duration::from_secs(32));
    }

    #[test]
    fn retry_after_is_parsed_and_capped() {
        let mut headers = HeaderMap::new();
        headers.insert("retry-after", HeaderValue::from_static("120"));
        assert_eq!(retry_after(&headers), Some(Duration::from_secs(120)));

        headers.insert("retry-after", HeaderValue::from_static("999999"));
        assert_eq!(retry_after(&headers), Some(MAX_RETRY_AFTER));

        // HTTP-date form is not parsed; we fall back to our own backoff.
        headers.insert(
            "retry-after",
            HeaderValue::from_static("Wed, 21 Oct 2026 07:28:00 GMT"),
        );
        assert_eq!(retry_after(&headers), None);

        assert_eq!(retry_after(&HeaderMap::new()), None);
    }

    #[test]
    fn throttling_statuses_are_classified() {
        for status in [403, 429, 500, 503, 599] {
            assert!(is_throttling(status), "{status} should be throttling");
        }
        for status in [200, 301, 404, 410] {
            assert!(!is_throttling(status), "{status} should not be throttling");
        }
    }

    #[test]
    fn action_id_cache_roundtrip() {
        let dir = std::env::temp_dir().join("ksl-cache-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("action-id");
        std::fs::write(&path, "7cf8064b59a8ce5f30344922b2db6c8be615d09e96").unwrap();
        let client = KslClient::builder()
            .action_cache_path(path.clone())
            .build()
            .unwrap();
        let cached = client.action_id.try_lock().unwrap().clone();
        assert_eq!(
            cached.as_deref(),
            Some("7cf8064b59a8ce5f30344922b2db6c8be615d09e96")
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

}
