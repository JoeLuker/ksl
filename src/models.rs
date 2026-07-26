use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Sort orders, indexed as the site's `sortOptions` list orders them.
/// Index 0 (newest first) is verified against live traffic; the others map
/// positionally onto the option list the server returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Sort {
    #[default]
    NewestFirst,
    OldestFirst,
    PriceLowToHigh,
    PriceHighToLow,
}

impl Sort {
    pub fn index(self) -> u8 {
        match self {
            Sort::NewestFirst => 0,
            Sort::OldestFirst => 1,
            Sort::PriceLowToHigh => 2,
            Sort::PriceHighToLow => 3,
        }
    }
}

impl<'de> serde::Deserialize<'de> for Sort {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

impl std::str::FromStr for Sort {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "newest" => Ok(Sort::NewestFirst),
            "oldest" => Ok(Sort::OldestFirst),
            "price-asc" => Ok(Sort::PriceLowToHigh),
            "price-desc" => Ok(Sort::PriceHighToLow),
            other => Err(format!(
                "unknown sort {other:?}; expected newest|oldest|price-asc|price-desc"
            )),
        }
    }
}

/// A search request. Filter values ride in the server-action body as a flat
/// string map; the typed setters cover the encodings verified against live
/// traffic, and `raw_filter` leaves the door open for the rest of the
/// server's filter catalog (`sellerType`, `newUsed`, `postedTime`, ...).
#[derive(Debug, Clone, Default)]
pub struct SearchQuery {
    pub keyword: Option<String>,
    pub sort: Sort,
    /// Listings per page; the site itself asks for 19.
    pub page_size: u32,
    pub filters: BTreeMap<String, String>,
}

impl SearchQuery {
    pub fn keyword(keyword: impl Into<String>) -> Self {
        SearchQuery {
            keyword: Some(keyword.into()),
            page_size: 19,
            ..Default::default()
        }
    }

    pub fn sort(mut self, sort: Sort) -> Self {
        self.sort = sort;
        self
    }

    /// Price range filter. Encodes as `"min-max"` (verified: narrows totals
    /// and every returned price falls inside the range).
    pub fn price_range(mut self, min: u64, max: u64) -> Self {
        self.filters.insert("price".into(), format!("{min}-{max}"));
        self
    }

    /// Restrict to listings within `miles` of a zip code (verified: totals
    /// narrow and returned cities cluster around the zip).
    pub fn near(mut self, zip: impl Into<String>, miles: u32) -> Self {
        self.filters.insert("zip".into(), zip.into());
        self.filters.insert("miles".into(), miles.to_string());
        self
    }

    /// Escape hatch for filters without a typed setter. Field names come from
    /// the `filters` catalog in [`SearchPage`]: `marketType`, `category`,
    /// `subCategory`, `sellerType`, `newUsed`, `postedTime`, `hasPhotos`,
    /// `hasVideo`, `expandSearch`.
    pub fn raw_filter(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.filters.insert(name.into(), value.into());
        self
    }

    /// The flat filter object sent in the action body.
    pub(crate) fn filter_object(&self) -> serde_json::Map<String, serde_json::Value> {
        let mut map = serde_json::Map::new();
        if let Some(kw) = &self.keyword {
            map.insert("keyword".into(), serde_json::Value::String(kw.clone()));
        }
        for (k, v) in &self.filters {
            map.insert(k.clone(), serde_json::Value::String(v.clone()));
        }
        map
    }
}

/// One result card from a search response.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListingSummary {
    pub id: u64,
    #[serde(default)]
    pub listing_type: Option<String>,
    pub title: String,
    #[serde(default)]
    pub price: Option<f64>,
    #[serde(default)]
    pub price_modifier: Option<String>,
    #[serde(default)]
    pub location: Option<Location>,
    #[serde(default)]
    pub category: Option<String>,
    #[serde(default)]
    pub sub_category: Option<String>,
    #[serde(default)]
    pub seller_type: Option<String>,
    #[serde(default)]
    pub market_type: Option<String>,
    #[serde(default)]
    pub primary_image: Option<Image>,
    #[serde(default)]
    pub favorite_count: Option<u64>,
    #[serde(default)]
    pub member_is_verified: Option<bool>,
    /// Unix seconds.
    #[serde(default)]
    pub created_at: Option<i64>,
    /// Unix seconds; ordering key for newest-first search.
    #[serde(default)]
    pub display_at: Option<i64>,
    /// Unix seconds.
    #[serde(default)]
    pub expires_at: Option<i64>,
}

impl ListingSummary {
    pub fn url(&self) -> String {
        format!("https://classifieds.ksl.com/listing/{}", self.id)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Location {
    #[serde(default)]
    pub city: Option<String>,
    #[serde(default)]
    pub state: Option<String>,
    #[serde(default)]
    pub zip: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Image {
    pub url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PageInfo {
    pub has_next_page: bool,
    /// Opaque continuation token (base64 of an Elasticsearch `search_after`
    /// pair). Feed back via `KslClient::search_after`.
    #[serde(default)]
    pub end_cursor: Option<String>,
    pub total: u64,
}

/// One page of search results, as returned by the site's `search` server
/// action. The raw response also carries the full filter catalog; it is kept
/// as untyped JSON for callers that want to enumerate available filters.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchPage {
    pub items: Vec<ListingSummary>,
    pub page_info: PageInfo,
    #[serde(default)]
    pub filters: serde_json::Value,
}

/// Listing detail, parsed from the schema.org `Product` JSON-LD embedded in
/// the listing page — the most change-resistant surface the site offers.
#[derive(Debug, Clone, Serialize)]
pub struct ListingDetail {
    pub id: u64,
    pub name: String,
    pub description: Option<String>,
    pub url: String,
    pub price: Option<f64>,
    pub price_currency: Option<String>,
    pub price_valid_until: Option<String>,
    /// schema.org URI, e.g. `https://schema.org/UsedCondition`.
    pub condition: Option<String>,
    /// schema.org URI, e.g. `https://schema.org/InStock`.
    pub availability: Option<String>,
    pub seller_name: Option<String>,
    pub seller_locality: Option<String>,
    pub seller_region: Option<String>,
    pub seller_postal_code: Option<String>,
    pub images: Vec<String>,
}
