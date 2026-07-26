//! Extraction of schema.org `Product` JSON-LD from listing pages.
//!
//! JSON-LD blocks are self-contained JSON islands inside
//! `<script type="application/ld+json">` tags, so they can be lifted out with
//! string scanning — no HTML tree needed, and immune to the CSS-class churn
//! that breaks selector-based scrapers.

use serde_json::Value;

use crate::error::{Error, Result};
use crate::models::ListingDetail;

pub fn parse_listing(id: u64, html: &str) -> Result<ListingDetail> {
    for block in jsonld_blocks(html) {
        let Ok(value) = serde_json::from_str::<Value>(&block) else {
            continue;
        };
        // A block may hold a single object or an array of them.
        let candidates: Vec<&Value> = match &value {
            Value::Array(items) => items.iter().collect(),
            other => vec![other],
        };
        for obj in candidates {
            if obj.get("@type").and_then(Value::as_str) == Some("Product") {
                return Ok(product_to_detail(id, obj));
            }
        }
    }
    Err(Error::NoStructuredData(id))
}

fn jsonld_blocks(html: &str) -> impl Iterator<Item = String> + '_ {
    const OPEN: &str = r#"<script type="application/ld+json">"#;
    const CLOSE: &str = "</script>";
    let mut rest = html;
    std::iter::from_fn(move || {
        let start = rest.find(OPEN)?;
        let after = &rest[start + OPEN.len()..];
        let end = after.find(CLOSE)?;
        let block = after[..end].to_string();
        rest = &after[end + CLOSE.len()..];
        Some(block)
    })
}

fn product_to_detail(id: u64, product: &Value) -> ListingDetail {
    let offer = product.get("offers");
    let seller = offer.and_then(|o| o.get("seller"));
    let address = seller.and_then(|s| s.get("address"));
    // `image` appears as a bare URL string, an ImageObject, or an array of
    // either (all three shapes observed live).
    let images = match product.get("image") {
        Some(Value::Array(entries)) => entries.iter().filter_map(image_url).collect(),
        Some(entry) => image_url(entry).into_iter().collect(),
        None => Vec::new(),
    };
    ListingDetail {
        id,
        name: str_field(product, "name").unwrap_or_default(),
        description: str_field(product, "description"),
        url: str_field(product, "url")
            .unwrap_or_else(|| format!("https://classifieds.ksl.com/listing/{id}")),
        price: offer.and_then(|o| o.get("price")).and_then(Value::as_f64),
        price_currency: offer.and_then(|o| str_field(o, "priceCurrency")),
        price_valid_until: offer.and_then(|o| str_field(o, "priceValidUntil")),
        condition: offer.and_then(|o| str_field(o, "itemCondition")),
        availability: offer.and_then(|o| str_field(o, "availability")),
        seller_name: seller.and_then(|s| str_field(s, "name")),
        seller_locality: address.and_then(|a| str_field(a, "addressLocality")),
        seller_region: address.and_then(|a| str_field(a, "addressRegion")),
        seller_postal_code: address.and_then(|a| str_field(a, "postalCode")),
        images,
    }
}

fn image_url(entry: &Value) -> Option<String> {
    match entry {
        Value::String(url) => Some(url.clone()),
        Value::Object(_) => str_field(entry, "url"),
        _ => None,
    }
}

fn str_field(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_product_jsonld() {
        let html = r#"<html><head>
<script type="application/ld+json">{"@context":"https://schema.org","@type":"BreadcrumbList"}</script>
<script type="application/ld+json">{"@context":"https://schema.org","@type":"Product","name":"Native Watercraft Slayer Propel Max 10 fishing kayak","description":"$2,250 fishing kayak in Orem, UT","url":"https://classifieds.ksl.com/listing/81125407","image":["https://image.ksldigital.com/a.jpg"],"offers":{"@type":"Offer","priceCurrency":"USD","price":2250,"priceValidUntil":"2026-08-25","availability":"https://schema.org/InStock","itemCondition":"https://schema.org/UsedCondition","seller":{"@type":"Person","name":"Dan Tingey","address":{"@type":"PostalAddress","addressLocality":"Orem","addressRegion":"UT"}}}}</script>
</head></html>"#;
        let d = parse_listing(81125407, html).unwrap();
        assert_eq!(d.name, "Native Watercraft Slayer Propel Max 10 fishing kayak");
        assert_eq!(d.price, Some(2250.0));
        assert_eq!(d.price_currency.as_deref(), Some("USD"));
        assert_eq!(d.condition.as_deref(), Some("https://schema.org/UsedCondition"));
        assert_eq!(d.seller_name.as_deref(), Some("Dan Tingey"));
        assert_eq!(d.seller_locality.as_deref(), Some("Orem"));
        assert_eq!(d.images, vec!["https://image.ksldigital.com/a.jpg"]);
    }

    #[test]
    fn parses_real_listing_page_fixture() {
        // Structure captured live 2026-07-26 (ImageObject-shaped `image`, and
        // an address carrying postalCode but no locality/region); the
        // identities in it are synthetic.
        let html = include_str!("../tests/fixtures/listing_page.html");
        let d = parse_listing(10000001, html).unwrap();
        assert_eq!(d.name, "Test Kayak Listing");
        assert_eq!(d.price, Some(150.0));
        assert_eq!(d.seller_name.as_deref(), Some("Test Seller"));
        assert_eq!(d.seller_postal_code.as_deref(), Some("00000"));
        assert_eq!(d.seller_locality, None);
        assert_eq!(d.images, vec!["https://example.invalid/image-1.jpg"]);
    }

    #[test]
    fn missing_product_is_an_error() {
        let err = parse_listing(1, "<html></html>").unwrap_err();
        assert!(matches!(err, Error::NoStructuredData(1)));
    }
}
