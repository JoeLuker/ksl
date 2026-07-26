//! Discovery of the site's `search` server-action id.
//!
//! Next.js server actions are addressed by an opaque 40-hex id that rotates
//! with each deployment. The id is statically present in the site's JS
//! chunks as `createServerReference("<id>", ..., "search")`, so we can
//! recover it at runtime: fetch the search page, collect its chunk URLs,
//! and scan them until the reference shows up.

use regex::Regex;

use crate::error::{Error, Result};

pub const BASE_URL: &str = "https://classifieds.ksl.com";

pub fn chunk_urls(page_html: &str) -> Vec<String> {
    let re = Regex::new(r#"src="(https://marketplace-cdn\.ksl\.com/_next/static/chunks/[^"]+\.js)""#)
        .expect("static regex");
    let mut urls: Vec<String> = re
        .captures_iter(page_html)
        .map(|c| c[1].to_string())
        .collect();
    urls.dedup();
    urls
}

pub fn find_search_action_id(chunk_js: &str) -> Option<String> {
    // Observed ids are 42 hex chars; accept a range in case the encoder's
    // output length shifts between Next.js versions.
    let re = Regex::new(r#"createServerReference\)?\("([0-9a-f]{32,64})"[^)]{0,200}?,"search"\)"#)
        .expect("static regex");
    re.captures(chunk_js).map(|c| c[1].to_string())
}

/// Fetch the search page and scan its chunks for the action id.
pub async fn discover(http: &reqwest::Client) -> Result<String> {
    let page_url = format!("{BASE_URL}/v2/search/keyword/a");
    let html = http.get(&page_url).send().await?.text().await?;
    let chunks = chunk_urls(&html);
    if chunks.is_empty() {
        return Err(Error::Discovery(format!(
            "no JS chunk URLs found on {page_url}; page layout may have changed"
        )));
    }
    tracing::debug!(chunks = chunks.len(), "scanning chunks for search action id");
    for url in &chunks {
        let js = http.get(url).send().await?.text().await?;
        if let Some(id) = find_search_action_id(&js) {
            tracing::info!(action_id = %id, chunk = %url, "discovered search action id");
            return Ok(id);
        }
    }
    Err(Error::Discovery(format!(
        "no createServerReference(.., \"search\") in {} chunks",
        chunks.len()
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_action_id_in_real_chunk_snippet() {
        let js = include_str!("../tests/fixtures/chunk_snippet.js");
        assert_eq!(
            find_search_action_id(js).as_deref(),
            Some("7cf8064b59a8ce5f30344922b2db6c8be615d09e96")
        );
    }

    #[test]
    fn extracts_chunk_urls() {
        let html = r#"<script src="https://marketplace-cdn.ksl.com/_next/static/chunks/abc.js" async></script>
<script src="https://marketplace-cdn.ksl.com/_next/static/chunks/def.js"></script>"#;
        assert_eq!(
            chunk_urls(html),
            vec![
                "https://marketplace-cdn.ksl.com/_next/static/chunks/abc.js",
                "https://marketplace-cdn.ksl.com/_next/static/chunks/def.js"
            ]
        );
    }
}
