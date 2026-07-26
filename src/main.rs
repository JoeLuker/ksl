use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use clap::{Parser, Subcommand};

use ksl::{Config, KslClient, SearchQuery, Sort, WatchConfig, Watcher};

#[derive(Parser)]
#[command(name = "ksl", about = "KSL Classifieds: search, listing details, watches")]
struct Cli {
    /// Emit machine-readable JSON instead of human-readable lines.
    #[arg(long, global = true)]
    json: bool,

    /// Config file (default: <platform config dir>/config.yaml).
    #[arg(long, global = true, value_name = "PATH")]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Search listings.
    Search {
        keyword: String,
        #[command(flatten)]
        filters: FilterArgs,
        /// Pages to fetch (cursor-chained).
        #[arg(long, default_value_t = 1)]
        pages: u32,
    },
    /// Show one listing's details (from its structured data).
    Listing { id: u64 },
    /// Poll searches and record never-seen-before listings to JSONL files.
    ///
    /// With no NAME, runs every watch in the config file concurrently.
    /// With a NAME matching a configured watch, runs that one (flags
    /// override its settings). Any other NAME starts an ad-hoc keyword
    /// watch built from the flags.
    Watch {
        name: Option<String>,
        #[command(flatten)]
        filters: FilterArgs,
        /// Seconds between polls.
        #[arg(long)]
        interval: Option<u64>,
        /// Directory for watch state and events.
        #[arg(long)]
        state_dir: Option<PathBuf>,
    },
    /// Show the resolved configuration (also validates it).
    Config,
}

#[derive(clap::Args)]
struct FilterArgs {
    #[arg(long)]
    min_price: Option<u64>,
    #[arg(long)]
    max_price: Option<u64>,
    /// Zip code to search near (with --miles).
    #[arg(long)]
    zip: Option<String>,
    /// Radius in miles around the zip (default 25).
    #[arg(long)]
    miles: Option<u32>,
    /// newest|oldest|price-asc|price-desc
    #[arg(long)]
    sort: Option<Sort>,
    /// Extra raw filter as name=value (repeatable); field names per the
    /// site's filter catalog: marketType, category, subCategory, sellerType,
    /// newUsed, postedTime, hasPhotos, hasVideo, expandSearch.
    #[arg(long = "filter", value_parser = parse_key_val)]
    raw: Vec<(String, String)>,
}

fn parse_key_val(s: &str) -> Result<(String, String), String> {
    s.split_once('=')
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .ok_or_else(|| format!("expected name=value, got {s:?}"))
}

impl FilterArgs {
    fn into_query(self, keyword: String) -> SearchQuery {
        let mut query = SearchQuery::keyword(keyword).sort(self.sort.unwrap_or_default());
        match (self.min_price, self.max_price) {
            (None, None) => {}
            (min, max) => {
                query = query.price_range(min.unwrap_or(0), max.unwrap_or(200_000));
            }
        }
        if let Some(zip) = self.zip {
            query = query.near(zip, self.miles.unwrap_or(25));
        }
        for (name, value) in self.raw {
            query = query.raw_filter(name, value);
        }
        query
    }

    /// Overlay any explicitly-set flags onto a configured watch
    /// (CLI > config > defaults).
    fn overlay(self, watch: &mut WatchConfig) {
        if let Some(v) = self.min_price {
            watch.min_price = Some(v);
        }
        if let Some(v) = self.max_price {
            watch.max_price = Some(v);
        }
        if let Some(v) = self.zip {
            watch.zip = Some(v);
        }
        if let Some(v) = self.miles {
            watch.miles = v;
        }
        if let Some(v) = self.sort {
            watch.sort = v;
        }
        for (name, value) in self.raw {
            watch.filters.insert(name, value);
        }
    }
}

fn project_dirs() -> Option<directories::ProjectDirs> {
    directories::ProjectDirs::from("com", "ksl-sdk", "ksl")
}

fn default_config_path() -> PathBuf {
    project_dirs()
        .map(|d| d.config_dir().join("config.yaml"))
        .unwrap_or_else(|| PathBuf::from(".ksl/config.yaml"))
}

fn default_state_dir() -> PathBuf {
    project_dirs()
        .map(|d| d.data_local_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from(".ksl"))
}

/// State-dir precedence: CLI flag > config > platform default.
fn resolve_state_dir(flag: Option<PathBuf>, config: &Config) -> PathBuf {
    flag.or_else(|| config.state_dir())
        .unwrap_or_else(default_state_dir)
}

fn build_client(config: &Config, state_dir: &std::path::Path) -> ksl::Result<KslClient> {
    KslClient::builder()
        .min_interval(Duration::from_secs(config.client.min_interval_secs))
        .max_retries(config.client.max_retries)
        .block_cooloff(Duration::from_secs(config.client.block_cooloff_secs))
        .action_cache_path(state_dir.join("action-id"))
        .build()
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "ksl=info".into()),
        )
        .with_writer(std::io::stderr)
        .init();

    if let Err(err) = run(Cli::parse()).await {
        eprintln!("error: {err}");
        std::process::exit(1);
    }
}

async fn run(cli: Cli) -> ksl::Result<()> {
    let config_path = cli
        .config
        .clone()
        .unwrap_or_else(default_config_path);
    let config = Config::load(cli.config.as_deref(), &default_config_path())?;

    match cli.command {
        Command::Search {
            keyword,
            filters,
            pages,
        } => {
            let state_dir = resolve_state_dir(None, &config);
            let client = build_client(&config, &state_dir)?;
            let query = filters.into_query(keyword);
            let mut page = client.search(&query).await?;
            let total = page.page_info.total;
            let mut shown = 0usize;
            for page_no in 1.. {
                for item in &page.items {
                    shown += 1;
                    if cli.json {
                        println!("{}", serde_json::to_string(item)?);
                    } else {
                        print_summary(item);
                    }
                }
                if page_no >= pages || !page.page_info.has_next_page {
                    break;
                }
                let cursor = page.page_info.end_cursor.as_deref().unwrap_or_default();
                page = client.search_after(&query, cursor).await?;
            }
            if !cli.json {
                eprintln!("({shown} of {total} results)");
            }
        }
        Command::Listing { id } => {
            let state_dir = resolve_state_dir(None, &config);
            let client = build_client(&config, &state_dir)?;
            let detail = client.listing(id).await?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&detail)?);
            } else {
                print_detail(&detail);
            }
        }
        Command::Watch {
            name,
            filters,
            interval,
            state_dir: dir_flag,
        } => {
            let state_dir = resolve_state_dir(dir_flag, &config);
            let client = Arc::new(build_client(&config, &state_dir)?);

            // Resolve which watches to run.
            let watches: Vec<WatchConfig> = match name {
                None => {
                    if config.watches.is_empty() {
                        return Err(ksl::Error::Config(format!(
                            "no watches configured in {} and no keyword given",
                            config_path.display()
                        )));
                    }
                    config.watches.clone()
                }
                Some(name) => {
                    match config.watches.iter().find(|w| w.name == name) {
                        Some(configured) => {
                            let mut watch = configured.clone();
                            filters.overlay(&mut watch);
                            if let Some(secs) = interval {
                                watch.interval_secs = secs;
                            }
                            vec![watch]
                        }
                        // Not a configured name: treat it as an ad-hoc keyword.
                        None => {
                            let query = filters.into_query(name.clone());
                            let mut watch = WatchConfig {
                                name: slugify(&name),
                                keyword: Some(name),
                                min_price: None,
                                max_price: None,
                                zip: None,
                                miles: 25,
                                sort: query.sort,
                                interval_secs: interval.unwrap_or(600),
                                filters: Default::default(),
                            };
                            // Carry the already-built filter map verbatim.
                            watch.filters = query.filters.clone();
                            vec![watch]
                        }
                    }
                }
            };

            let mut set = tokio::task::JoinSet::new();
            for watch in watches {
                let name = watch.name.clone();
                eprintln!(
                    "watching {name:?} every {}s; state in {}",
                    watch.interval_secs,
                    state_dir.display()
                );
                let watcher = Watcher::new(
                    Arc::clone(&client),
                    watch.to_query(),
                    Duration::from_secs(watch.interval_secs),
                    name.clone(),
                    &state_dir,
                )?;
                set.spawn(async move { (name, watcher.run().await) });
            }
            let mut failures = 0u32;
            while let Some(joined) = set.join_next().await {
                match joined {
                    Ok((name, Ok(()))) => tracing::info!(watch = %name, "watch stopped"),
                    Ok((name, Err(err))) => {
                        failures += 1;
                        tracing::error!(watch = %name, %err, "watch gave up");
                    }
                    Err(join_err) => {
                        failures += 1;
                        tracing::error!(%join_err, "watch task panicked");
                    }
                }
            }
            if failures > 0 {
                return Err(ksl::Error::Config(format!("{failures} watch(es) failed")));
            }
        }
        Command::Config => {
            let state_dir = resolve_state_dir(None, &config);
            println!("config file: {}", config_path.display());
            println!(
                "client: min_interval={}s max_retries={} block_cooloff={}s",
                config.client.min_interval_secs,
                config.client.max_retries,
                config.client.block_cooloff_secs
            );
            println!("state dir: {}", state_dir.display());
            if config.watches.is_empty() {
                println!("watches: none");
            } else {
                println!("watches:");
                for w in &config.watches {
                    let query = w.to_query();
                    let filters: Vec<String> = query
                        .filters
                        .iter()
                        .map(|(k, v)| format!("{k}={v}"))
                        .collect();
                    println!(
                        "  {:<20} keyword={:<15} every {:>5}s  {}",
                        w.name,
                        query.keyword.as_deref().unwrap_or("-"),
                        w.interval_secs,
                        filters.join(" ")
                    );
                }
            }
        }
    }
    Ok(())
}

fn print_summary(item: &ksl::ListingSummary) {
    let price = item
        .price
        .map(|p| format!("${p:.0}"))
        .unwrap_or_else(|| "-".into());
    let location = item
        .location
        .as_ref()
        .map(|l| {
            format!(
                "{}, {}",
                l.city.as_deref().unwrap_or("?"),
                l.state.as_deref().unwrap_or("?")
            )
        })
        .unwrap_or_default();
    println!(
        "{:>8}  {:<60}  {:<20}  {}",
        price,
        item.title,
        location,
        item.url()
    );
}

fn print_detail(detail: &ksl::ListingDetail) {
    println!("{}", detail.name);
    if let Some(price) = detail.price {
        println!(
            "  {} {}",
            price,
            detail.price_currency.as_deref().unwrap_or("")
        );
    }
    let place: Vec<&str> = [
        detail.seller_locality.as_deref(),
        detail.seller_region.as_deref(),
        detail.seller_postal_code.as_deref(),
    ]
    .into_iter()
    .flatten()
    .collect();
    if !place.is_empty() {
        println!("  {}", place.join(", "));
    }
    if let Some(seller) = &detail.seller_name {
        println!("  seller: {seller}");
    }
    if let Some(condition) = &detail.condition {
        println!(
            "  condition: {}",
            condition.rsplit('/').next().unwrap_or(condition)
        );
    }
    if let Some(desc) = &detail.description {
        println!("\n{desc}");
    }
}

fn slugify(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>()
        .split('-')
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}
