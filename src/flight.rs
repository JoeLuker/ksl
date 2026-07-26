//! Parsing for React Server Component "flight" payloads
//! (`Content-Type: text/x-component`).
//!
//! A flight payload is a sequence of newline-delimited rows of the form
//! `<row-id>:<data>`. The search server action returns its JSON result as one
//! such row; we identify it structurally (the object carrying `pageInfo` and
//! `items`) rather than by row id, which is an implementation detail of the
//! React serializer.

use crate::error::{Error, Result};
use crate::models::SearchPage;

pub fn parse_search_response(body: &str) -> Result<SearchPage> {
    for line in body.lines() {
        let Some((row_id, data)) = line.split_once(':') else {
            continue;
        };
        if row_id.is_empty() || !row_id.bytes().all(|b| b.is_ascii_hexdigit()) {
            continue;
        }
        if !data.starts_with('{') {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(data) else {
            continue;
        };
        if value.get("pageInfo").is_some() && value.get("items").is_some() {
            let page: SearchPage = serde_json::from_value(value)?;
            debug_assert!(
                page.items.len() as u64 <= page.page_info.total,
                "page has more items than the reported total"
            );
            return Ok(page);
        }
    }
    Err(Error::Parse(format!(
        "no search-result row in flight payload ({} bytes); \
         the server action id may be stale",
        body.len()
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("../tests/fixtures/search_response.flight");

    #[test]
    fn parses_real_flight_payload() {
        let page = parse_search_response(FIXTURE).unwrap();
        assert_eq!(page.page_info.total, 423);
        assert_eq!(page.items.len(), 19);
        assert!(page.page_info.has_next_page);
        assert!(page.page_info.end_cursor.is_some());

        // Identities in the fixture are synthetic; everything asserted here is
        // shape and parsing behavior, which is what this test is for.
        let first = &page.items[0];
        assert_eq!(first.id, 10000001);
        assert_eq!(first.title, "Test Listing 1");
        assert_eq!(first.price, Some(1000.0));
        let loc = first.location.as_ref().unwrap();
        assert_eq!(loc.city.as_deref(), Some("Testville"));
        assert_eq!(loc.state.as_deref(), Some("UT"));
        assert_eq!(first.sub_category.as_deref(), Some("Kayaks"));
        assert_eq!(first.created_at, Some(1782335195));
        assert_eq!(first.favorite_count, Some(5));
    }

    #[test]
    fn rejects_payload_without_result_row() {
        let err = parse_search_response("0:{\"a\":\"$@1\"}\n").unwrap_err();
        assert!(matches!(err, Error::Parse(_)));
    }
}
