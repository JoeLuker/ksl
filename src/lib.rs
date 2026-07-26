//! SDK for KSL Classifieds (classifieds.ksl.com).
//!
//! The site is a Next.js app; this crate speaks to its two public read
//! surfaces, both verified empirically (2026-07-26):
//!
//! - **Search** — the `search` Next.js server action: a POST to the search
//!   route with a `next-action` header, returning structured listing JSON
//!   inside a React flight payload. Pagination is cursor-based
//!   (`pageInfo.endCursor`). The action id rotates with site deployments and
//!   is re-discovered automatically from the site's JS chunks.
//! - **Listing detail** — schema.org `Product` JSON-LD embedded in each
//!   listing page.
//!
//! ```no_run
//! use ksl::{KslClient, SearchQuery, Sort};
//!
//! # async fn demo() -> ksl::Result<()> {
//! let client = KslClient::new()?;
//! let query = SearchQuery::keyword("kayak")
//!     .price_range(100, 500)
//!     .sort(Sort::NewestFirst);
//! let page = client.search(&query).await?;
//! println!("{} total matches", page.page_info.total);
//! for item in &page.items {
//!     println!("{:>10} {} {}", item.price.unwrap_or(0.0), item.title, item.url());
//! }
//! # Ok(())
//! # }
//! ```

mod client;
mod config;
mod discovery;
mod error;
mod flight;
mod geo;
mod haul;
mod jsonld;
mod models;
mod watch;

pub use client::{KslClient, KslClientBuilder};
pub use config::{ClientConfig, Config, WatchConfig};
pub use error::{Error, Result};
pub use geo::miles_between;
pub use haul::{HaulEstimate, Rates, SelfHaul, SizeClass, landed_cost};
pub use models::{
    Image, ListingDetail, ListingSummary, Location, PageInfo, SearchPage, SearchQuery, Sort,
};
pub use watch::Watcher;
