//! YAML configuration: client politeness settings and named watches.
//!
//! One canonical home: the config file (default
//! `<platform config dir>/config.yaml`, overridable with `--config`).
//! CLI flags override config values per-invocation; built-in defaults apply
//! when neither is given. Unknown YAML keys are rejected so typos fail loudly
//! instead of silently deconfiguring something.
//!
//! ```yaml
//! client:
//!   min_interval_secs: 2
//!   max_retries: 3
//!   block_cooloff_secs: 1800   # stay away this long after a hard block
//! state_dir: ~/ksl-data          # optional; default: platform data dir
//! watches:
//!   - name: cheap-kayaks
//!     keyword: kayak
//!     min_price: 100
//!     max_price: 500
//!     zip: "84604"
//!     miles: 25
//!     sort: newest               # newest|oldest|price-asc|price-desc
//!     interval_secs: 600
//!     filters:                   # raw pass-through to the filter catalog
//!       sellerType: Private
//! ```

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::error::{Error, Result};
use crate::models::{SearchQuery, Sort};

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub client: ClientConfig,
    /// Directory for watch state, events, and the action-id cache.
    #[serde(default)]
    pub state_dir: Option<PathBuf>,
    #[serde(default)]
    pub watches: Vec<WatchConfig>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientConfig {
    /// Minimum spacing between any two requests.
    #[serde(default = "defaults::min_interval_secs")]
    pub min_interval_secs: u64,
    /// Retries after throttling responses.
    #[serde(default = "defaults::max_retries")]
    pub max_retries: u32,
    /// How long to stay off the site entirely once retries are exhausted.
    /// Continued probing during a block appears to lengthen it.
    #[serde(default = "defaults::block_cooloff_secs")]
    pub block_cooloff_secs: u64,
}

impl Default for ClientConfig {
    fn default() -> Self {
        ClientConfig {
            min_interval_secs: defaults::min_interval_secs(),
            max_retries: defaults::max_retries(),
            block_cooloff_secs: defaults::block_cooloff_secs(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WatchConfig {
    /// Names the watch and its state/event files: `[a-z0-9-_]+`.
    pub name: String,
    #[serde(default)]
    pub keyword: Option<String>,
    #[serde(default)]
    pub min_price: Option<u64>,
    #[serde(default)]
    pub max_price: Option<u64>,
    #[serde(default)]
    pub zip: Option<String>,
    #[serde(default = "defaults::miles")]
    pub miles: u32,
    #[serde(default)]
    pub sort: Sort,
    #[serde(default = "defaults::interval_secs")]
    pub interval_secs: u64,
    /// Raw filter-catalog pass-through (sellerType, category, newUsed, ...).
    #[serde(default)]
    pub filters: BTreeMap<String, String>,
}

mod defaults {
    pub fn min_interval_secs() -> u64 {
        2
    }
    pub fn max_retries() -> u32 {
        3
    }
    pub fn block_cooloff_secs() -> u64 {
        30 * 60
    }
    pub fn miles() -> u32 {
        25
    }
    pub fn interval_secs() -> u64 {
        600
    }
}

impl Config {
    /// Load configuration. An explicitly given path must exist; a missing
    /// file at the default path just means built-in defaults.
    pub fn load(explicit: Option<&Path>, default_path: &Path) -> Result<Self> {
        let (path, required) = match explicit {
            Some(p) => (p, true),
            None => (default_path, false),
        };
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound && !required => {
                tracing::debug!(path = %path.display(), "no config file; using defaults");
                return Ok(Config::default());
            }
            Err(err) => {
                return Err(Error::Config(format!("{}: {err}", path.display())));
            }
        };
        let config: Config = serde_yaml_ng::from_str(&text)
            .map_err(|err| Error::Config(format!("{}: {err}", path.display())))?;
        config.validate()?;
        tracing::debug!(path = %path.display(), watches = config.watches.len(), "loaded config");
        Ok(config)
    }

    fn validate(&self) -> Result<()> {
        if self.client.min_interval_secs == 0 {
            return Err(Error::Config(
                "client.min_interval_secs must be at least 1; \
                 the site rate-limits bursts hard"
                    .into(),
            ));
        }
        let mut names = BTreeSet::new();
        for watch in &self.watches {
            if watch.name.is_empty()
                || !watch
                    .name
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
            {
                return Err(Error::Config(format!(
                    "watch name {:?} must be non-empty [a-z0-9-_] (it names state files)",
                    watch.name
                )));
            }
            if !names.insert(&watch.name) {
                return Err(Error::Config(format!("duplicate watch name {:?}", watch.name)));
            }
            if watch.keyword.as_deref().unwrap_or_default().is_empty() && watch.filters.is_empty() {
                return Err(Error::Config(format!(
                    "watch {:?} matches everything: give it a keyword or filters",
                    watch.name
                )));
            }
            if watch.interval_secs < 60 {
                tracing::warn!(
                    watch = %watch.name,
                    interval_secs = watch.interval_secs,
                    "aggressive poll interval; the site blocks bursty clients"
                );
            }
        }
        Ok(())
    }

    /// `state_dir` with `~/` expanded, when configured.
    pub fn state_dir(&self) -> Option<PathBuf> {
        self.state_dir.as_ref().map(|dir| expand_tilde(dir))
    }
}

impl WatchConfig {
    pub fn to_query(&self) -> SearchQuery {
        let mut query = SearchQuery {
            keyword: self.keyword.clone().filter(|k| !k.is_empty()),
            sort: self.sort,
            page_size: 19,
            ..Default::default()
        };
        match (self.min_price, self.max_price) {
            (None, None) => {}
            (min, max) => {
                query = query.price_range(min.unwrap_or(0), max.unwrap_or(200_000));
            }
        }
        if let Some(zip) = &self.zip {
            query = query.near(zip.clone(), self.miles);
        }
        for (name, value) in &self.filters {
            query = query.raw_filter(name.clone(), value.clone());
        }
        query
    }
}

fn expand_tilde(path: &Path) -> PathBuf {
    match path.strip_prefix("~") {
        Ok(rest) => match std::env::var_os("HOME") {
            Some(home) => PathBuf::from(home).join(rest),
            None => path.to_path_buf(),
        },
        Err(_) => path.to_path_buf(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULL: &str = r#"
client:
  min_interval_secs: 3
  max_retries: 5
state_dir: ~/ksl-data
watches:
  - name: cheap-kayaks
    keyword: kayak
    min_price: 100
    max_price: 500
    zip: "84604"
    miles: 30
    sort: price-asc
    interval_secs: 300
    filters:
      sellerType: Private
  - name: canoes
    keyword: canoe
"#;

    fn parse(yaml: &str) -> Result<Config> {
        let config: Config =
            serde_yaml_ng::from_str(yaml).map_err(|e| Error::Config(e.to_string()))?;
        config.validate()?;
        Ok(config)
    }

    #[test]
    fn parses_full_config() {
        let config = parse(FULL).unwrap();
        assert_eq!(config.client.min_interval_secs, 3);
        assert_eq!(config.client.max_retries, 5);
        assert_eq!(config.watches.len(), 2);

        let w = &config.watches[0];
        assert_eq!(w.name, "cheap-kayaks");
        assert_eq!(w.sort, Sort::PriceLowToHigh);
        assert_eq!(w.interval_secs, 300);
        let q = w.to_query();
        assert_eq!(q.keyword.as_deref(), Some("kayak"));
        assert_eq!(q.filters.get("price").map(String::as_str), Some("100-500"));
        assert_eq!(q.filters.get("zip").map(String::as_str), Some("84604"));
        assert_eq!(q.filters.get("miles").map(String::as_str), Some("30"));
        assert_eq!(
            q.filters.get("sellerType").map(String::as_str),
            Some("Private")
        );

        let defaults = &config.watches[1];
        assert_eq!(defaults.interval_secs, 600);
        assert_eq!(defaults.miles, 25);
        assert_eq!(defaults.sort, Sort::NewestFirst);
    }

    #[test]
    fn state_dir_tilde_expands_to_home() {
        let config = parse(FULL).unwrap();
        let dir = config.state_dir().unwrap();
        assert!(dir.is_absolute());
        assert!(dir.ends_with("ksl-data"));
        assert!(!dir.to_string_lossy().contains('~'));
    }

    #[test]
    fn unknown_keys_are_rejected() {
        let err = parse("watches:\n  - name: a\n    keyword: x\n    pricee: 5\n").unwrap_err();
        assert!(err.to_string().contains("pricee"));
    }

    #[test]
    fn duplicate_names_are_rejected() {
        let yaml = "watches:\n  - name: a\n    keyword: x\n  - name: a\n    keyword: y\n";
        assert!(parse(yaml).unwrap_err().to_string().contains("duplicate"));
    }

    #[test]
    fn unnamed_or_bad_names_are_rejected() {
        let yaml = "watches:\n  - name: \"Bad Name\"\n    keyword: x\n";
        assert!(parse(yaml).unwrap_err().to_string().contains("Bad Name"));
    }

    #[test]
    fn unconstrained_watch_is_rejected() {
        let yaml = "watches:\n  - name: everything\n";
        assert!(
            parse(yaml)
                .unwrap_err()
                .to_string()
                .contains("matches everything")
        );
    }

    #[test]
    fn missing_default_config_is_fine() {
        let config = Config::load(None, Path::new("/nonexistent/config.yaml")).unwrap();
        assert!(config.watches.is_empty());
    }

    #[test]
    fn missing_explicit_config_is_an_error() {
        let err =
            Config::load(Some(Path::new("/nonexistent/config.yaml")), Path::new("x")).unwrap_err();
        assert!(matches!(err, Error::Config(_)));
    }
}
