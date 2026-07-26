# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

A Rust SDK + CLI (`ksl`) for **KSL Classifieds** (classifieds.ksl.com): typed search, listing details, and a polling watcher that appends never-seen listings to JSONL.

## Commands

- Build: `cargo build` — Test: `cargo test` — Lint: `cargo clippy --all-targets`
- Single test: `cargo test <name>` (e.g. `cargo test parses_real_flight_payload`)
- CLI: `./target/debug/ksl search kayak --min-price 100 --max-price 500 [--pages N] [--json]`, `ksl listing <id>`, `ksl watch <keyword> --interval 600`
- Logs to stderr via `RUST_LOG` (default `ksl=info`)

## Architecture

`src/lib.rs` re-exports the public surface. One module per concern:

- `client.rs` — `KslClient`: reqwest wrapper owning politeness (min-interval throttle, exponential backoff on 403/429/5xx and on challenge pages) and the action-id lifecycle (memory → disk cache → discovery → re-discovery once when a search response stops parsing, which is what a post-deploy stale id looks like).
- `config.rs` — YAML config (`--config` or `<platform config dir>/config.yaml`): client politeness settings + named watches. `deny_unknown_fields` so typos fail loudly; CLI flags override config per-invocation. `ksl config` prints/validates the resolved picture; bare `ksl watch` runs all configured watches concurrently over one shared client (global throttle).
- `discovery.rs` — recovers the `search` server-action id from the site's JS chunks (`createServerReference("<hex id>",…,"search")`). IDs rotate with each site deploy; observed length 42 hex.
- `flight.rs` — parses React flight payloads (`text/x-component`); finds the result row structurally (object with `pageInfo`+`items`), not by row id.
- `jsonld.rs` — lifts schema.org `Product` JSON-LD out of listing pages by string-scanning script tags (deliberately no HTML parser: JSON-LD is the change-resistant surface).
- `models.rs` — `SearchQuery` (builder; flat string filter map), `ListingSummary`, `SearchPage`/`PageInfo`, `ListingDetail`, `Sort`.
- `watch.rs` — `Watcher`: polls newest-first, walks continuation pages only while a page is entirely unseen (watermark logic), first poll baselines without emitting, atomic state save, gives up after 5 consecutive failed polls.
- `main.rs` — clap CLI (`search`/`listing`/`watch`).

Tests: unit tests live with their modules and run against fixtures in `tests/fixtures/` captured from live responses (a flight payload, a listing page, a chunk snippet with the action reference). If the site changes shape, re-capture rather than hand-editing to make a test pass.

**Fixtures must never contain real users' data.** The captured payloads carry seller names, member ids, zips, and listing ids belonging to real people; scrub those to synthetic values (`Test Seller`, `Testville`, `84000`, ids from 10000001) as part of re-capturing, before the file is ever committed. Structure, field names, types, and site metadata are what the tests assert on — identities are not, and this repo is public.

## Current site behavior (verified empirically 2026-07-26)

- The classifieds live at **`https://classifieds.ksl.com`** (`www.ksl.com/classifieds/` redirects there). Next.js App Router (Turbopack chunks off `marketplace-cdn.ksl.com`); no `__NEXT_DATA__` blob.
- **The real search API is the `search` Next.js server action**: `POST https://classifieds.ksl.com/v2/search/keyword/<kw>/sort/<n>` with headers `Accept: text/x-component`, `next-action: <id>`, `Content-Type: text/plain;charset=UTF-8` and body `[null,"<sortIdx>",{"featuredCount":0,"featuredPosition":0,"spotlightCount":0,"spotlightPosition":10,"listingCount":19},{<flat filters>},"<cursor|empty>"]`. Works from bare curl — no cookies or auth. Response: flight payload whose result row is JSON with `items[]` (id, title, price, location{city,state,zip}, category/subCategory, sellerType, marketType, createdAt/displayAt/expiresAt, favoriteCount, primaryImage), `pageInfo{hasNextPage,endCursor,total}`, and the full filter catalog.
  - Filters verified: `{"keyword":"kayak"}`; `"price":"100-500"` (min-max string; totals narrow and returned prices stay in range); `"zip":"84604","miles":"25"` (totals narrow, cities cluster around the zip). Wrong shapes (objects, arrays) → HTTP 500. Catalog names: marketType, category, subCategory, price, expandSearch, hasPhotos, hasVideo, sellerType, newUsed, postedTime. Sort path/body index: 0=newest (verified), 2=price-asc (verified); 1=oldest, 3=price-desc positional from the option list.
  - First page: empty-string cursor (or omit the 5th element). Continuation: `pageInfo.endCursor` (base64 of an Elasticsearch `search_after` pair). SSR HTML `page/N` and `?page=` variants are ignored — cursor is the only pagination.
  - The action id rotates per deploy; recover it from the JS chunks (`createServerReference("<42-hex>",…,"search")`).
- **SSR search HTML** renders only the first ~11 cards (`a.search-result`, `data-item-id`) plus total count — a teaser, not the result set.
- **Listing detail pages** (`/listing/<id>`) embed **schema.org `Product` JSON-LD** with name, description, numeric price, `priceValidUntil`, availability, `itemCondition`, seller name + address city/state. Parse that, not CSS selectors.
- **Rate limiting is real**: the site fronts with PerimeterX (HUMAN Security), app id `PX2sZ8xyop` — browser traffic posts a sensor blob to `collector-PX2sZ8xyop.px-cloud.net/api/v2/collector`. A burst of ~10 rapid requests earned a multi-minute window of 403/503 and challenge pages; repeat offenses lengthened the block (~20-30 min → 50+ min). Space requests (≥2s) and back off on 403/429/5xx.

### What PerimeterX can see about us (measured 2026-07-26)

Our client is *unmistakably* not the browser its User-Agent claims, yet passes at low volume — so on these endpoints enforcement is evidently volume/behavior-driven, not fingerprint-driven. Measured against browserleaks:

| Signal | Our client | Real Chrome |
| --- | --- | --- |
| JA4 TLS | `t13d1011h2_61a7ad8aa9b6_3fcd1a44f3e3` (10 ciphers, 11 exts, no GREASE) | `t13d1516h2_…` (15/16, GREASE) |
| HTTP/2 Akamai fp | `2:0;4:2097152;5:16384;6:16384\|5177345\|0\|m,s,a,p` | different settings/priority set |
| PX sensor payload | never sent (no JS execution) | posted to `/api/v2/collector` |
| `_px3` token | never obtained (requires passing sensor) | present |
| `_pxhd` device cookie | **rotated on every response** for us | stable across requests |
| Behavioral telemetry | none (no mouse/scroll/dwell) | continuous |

Consequences:

1. **Rate discipline is the only lever that matters** for us, not disguise. We are trivially identifiable and still served; what gets us blocked is burst volume.
2. **Cookie persistence is worthless here** — measured: the site issues a fresh `_pxhd` on every response to sensor-less clients, even when the previous one is sent back, so there is no device identity to accumulate. The client keeps an in-session cookie store (correct HTTP behavior) but deliberately does *not* persist to disk. Don't re-add it without re-measuring.
3. **If KSL tightens policy on these endpoints, pacing won't save us** — the sensor/`_px3` chain would be required, which is out of scope for this SDK. That is the accepted failure mode.
4. `reqwest` with `default-features = false` silently drops HTTP/2 — a real defect that had us speaking HTTP/1.1; the `http2` feature is now explicit. Note feature renames in reqwest 0.13: `rustls-tls`→`rustls`, `macos-system-configuration`→`system-proxy`.

**We identify honestly**: the User-Agent is `ksl/<version> (+personal use; low volume; respects Retry-After)`, not a spoofed browser string — verified still served normally. A browser UA would contradict our own TLS fingerprint anyway.
- Old `api.php` is **404**. `api3.ksl.com` responds (400 to bare GETs) but its contract is unmapped.

## Historical access paths (prior art, all dated)

Two access paths were reverse-engineered previously, neither official:

### 1. The mobile-app JSON API
Discovered via Fiddler against KSL's Android app (noxad.com writeup):
- Original endpoint `http://www.ksl.com/classifieds/api.php` — **superseded**: KSL moved it to `api3.ksl.com` with HTTPS and **auth tokens**. The unauthenticated v1 described below is documentation of shape, not a working endpoint. Verify current behavior (proxy the current app) before building on it.
- Commands via `cmd=`: `categories` (category tree), `list` (search/category results), `ad` (single listing).
- Params: `c`/`o` (count/offset paging), `nid` (category id), `id` (ad id), `s` (search string), `min`/`max` (price), `z` + `d` (zip + distance miles), `srt` (sort), `slr` (seller type: all/private/business).
- JSON response fields: `sid`, `nid`, `title`, `price`, `image`, `display_time` (Unix ts), `city`, `state`.
- Jobs and cars are separate systems not covered by this API.

### 2. HTML scraping
What the three reference projects do. All have broken at least once when KSL changed markup (noted 2017), so treat any selector knowledge as stale:
- Old query-string shape: `ksl.com/?nid=231&search=...&min_price=&max_price=&zip=&distance=`.
- Old selectors: listing = `div.adBox`, title = `a.listlink`, price = `div.priceBox`, city/state/age = `div.adTime`.

## Reference implementations

- https://noxad.com/discovering-documenting-ksl-classifieds-api/ — the API reverse-engineering writeup (source of the param table above).
- https://gist.github.com/blakev/a6bbe3b5a861d64c6e36 — Python CLI scraper; BeautifulSoup + ThreadPoolExecutor, source of the selector list above.
- https://github.com/jdanders/ksl-classifieds-notifier — Python 3 polling daemon (derived from that gist); emails new matches, 10-min default interval, error-accumulation cutoff for resilience.
- https://github.com/jdcargile/ksl-classified-scraper — ASP.NET Core screen-scraper exposing results as a JSON HTTP API.

## Constraints to respect when building

- **Verify before building**: the historical sources predate KSL's current site/API. The 2026-07-26 probe above is the current baseline; re-verify if behavior diverges from it.
- **Polling etiquette**: the notifier's pattern is the sane default — modest intervals, back off on errors, stop after repeated failures.
