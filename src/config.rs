use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::target::normalise_base_url;

pub const DEFAULT_CONFIG_PATH: &str = "/etc/trout/trout.toml";
pub const MIN_ACCESS_KEY_LEN: usize = 32;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read config file {path}: {source}")]
    Read { path: PathBuf, source: std::io::Error },
    #[error("cannot parse config file {path}: {source}")]
    Parse {
        path: PathBuf,
        source: Box<toml::de::Error>,
    },
    #[error("invalid value for `{key}`: {reason}")]
    Invalid { key: &'static str, reason: String },
}

fn invalid(key: &'static str, reason: impl Into<String>) -> ConfigError {
    ConfigError::Invalid {
        key,
        reason: reason.into(),
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub listen: SocketAddr,
    pub access_key: String,
    pub targets: Targets,
    pub limits: Limits,
    pub store: Store,
    pub log: Log,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TargetMode {
    AnyPublic,
    Listed,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Targets {
    pub mode: TargetMode,
    pub list: Vec<TargetEntry>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TargetEntry {
    pub base_url: String,
    #[serde(default)]
    pub accepts_gzip: bool,
    #[serde(default)]
    pub allow_private: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Limits {
    pub max_body_bytes: usize,
    pub max_decoded_body_bytes: usize,
    pub connect_timeout_secs: u64,
    pub first_byte_timeout_secs: u64,
    pub stream_idle_timeout_secs: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Store {
    pub path: PathBuf,
    pub ttl_secs: u64,
    pub max_bytes: u64,
    pub sweep_interval_secs: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogFormat {
    Json,
    Text,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Log {
    pub format: LogFormat,
    pub level: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            listen: SocketAddr::from(([127, 0, 0, 1], 8787)),
            access_key: String::new(),
            targets: Targets::default(),
            limits: Limits::default(),
            store: Store::default(),
            log: Log::default(),
        }
    }
}

impl Default for Targets {
    fn default() -> Self {
        Self {
            mode: TargetMode::AnyPublic,
            list: Vec::new(),
        }
    }
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_body_bytes: 16 * 1024 * 1024,
            max_decoded_body_bytes: 64 * 1024 * 1024,
            connect_timeout_secs: 10,
            first_byte_timeout_secs: 120,
            stream_idle_timeout_secs: 300,
        }
    }
}

impl Default for Store {
    fn default() -> Self {
        Self {
            path: PathBuf::from("/var/lib/trout/trout.db"),
            ttl_secs: 86_400,
            max_bytes: 1024 * 1024 * 1024,
            sweep_interval_secs: 60,
        }
    }
}

impl Default for Log {
    fn default() -> Self {
        Self {
            format: LogFormat::Json,
            level: "info".to_string(),
        }
    }
}

impl Config {
    /// Defaults, then the TOML file named by `TROUT_CONFIG`, then environment overrides.
    pub fn load() -> Result<Self, ConfigError> {
        let env: HashMap<String, String> = std::env::vars()
            .filter(|(k, _)| k.starts_with("TROUT_"))
            .collect();

        Self::load_from(&env)
    }

    pub fn load_from(env: &HashMap<String, String>) -> Result<Self, ConfigError> {
        let path = env
            .get("TROUT_CONFIG")
            .map_or_else(|| PathBuf::from(DEFAULT_CONFIG_PATH), PathBuf::from);
        let mut config = Self::read_file(&path)?;

        config.apply_env(env)?;
        config.validate()?;

        Ok(config)
    }

    fn read_file(path: &Path) -> Result<Self, ConfigError> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(source) => {
                return Err(ConfigError::Read {
                    path: path.to_path_buf(),
                    source,
                });
            }
        };

        toml::from_str(&text).map_err(|source| ConfigError::Parse {
            path: path.to_path_buf(),
            source: Box::new(source),
        })
    }

    fn apply_env(&mut self, env: &HashMap<String, String>) -> Result<(), ConfigError> {
        if let Some(v) = env.get("TROUT_ACCESS_KEY") {
            self.access_key = v.clone();
        }

        if let Some(v) = env.get("TROUT_LISTEN") {
            self.listen = v.parse().map_err(|_| invalid("listen", "not a socket address"))?;
        }

        if let Some(v) = env.get("TROUT_STORE_PATH") {
            self.store.path = PathBuf::from(v);
        }

        if let Some(v) = env.get("TROUT_STORE_TTL_SECS") {
            self.store.ttl_secs = v
                .parse()
                .map_err(|_| invalid("store.ttl_secs", "not an integer"))?;
        }

        if let Some(v) = env.get("TROUT_LOG_LEVEL") {
            self.log.level = v.clone();
        }

        Ok(())
    }

    /// Checks every rule of the configuration contract and normalises listed target URLs.
    pub fn validate(&mut self) -> Result<(), ConfigError> {
        if self.access_key.len() < MIN_ACCESS_KEY_LEN {
            return Err(invalid(
                "access_key",
                format!("required, at least {MIN_ACCESS_KEY_LEN} characters (set TROUT_ACCESS_KEY)"),
            ));
        }

        for entry in &mut self.targets.list {
            let url = normalise_base_url(&entry.base_url).map_err(|reason| {
                invalid("targets.list[].base_url", format!("{}: {reason}", entry.base_url))
            })?;

            if url.scheme() == "http" && !entry.allow_private {
                return Err(invalid(
                    "targets.list[].base_url",
                    format!("{}: plain http requires allow_private = true", entry.base_url),
                ));
            }

            entry.base_url = url.as_str().trim_end_matches('/').to_string();
        }

        let l = &self.limits;

        if l.max_body_bytes == 0 {
            return Err(invalid("limits.max_body_bytes", "must be positive"));
        }

        if l.max_decoded_body_bytes < l.max_body_bytes {
            return Err(invalid(
                "limits.max_decoded_body_bytes",
                "must be at least limits.max_body_bytes",
            ));
        }

        if l.connect_timeout_secs == 0 {
            return Err(invalid("limits.connect_timeout_secs", "must be positive"));
        }

        if l.first_byte_timeout_secs == 0 {
            return Err(invalid("limits.first_byte_timeout_secs", "must be positive"));
        }

        if l.stream_idle_timeout_secs == 0 {
            return Err(invalid("limits.stream_idle_timeout_secs", "must be positive"));
        }

        let s = &self.store;

        if s.ttl_secs < 60 {
            return Err(invalid("store.ttl_secs", "must be at least 60"));
        }

        if s.max_bytes < 1024 * 1024 {
            return Err(invalid("store.max_bytes", "must be at least 1 MiB"));
        }

        if !(1..=300).contains(&s.sweep_interval_secs) {
            return Err(invalid("store.sweep_interval_secs", "must be between 1 and 300"));
        }

        if !matches!(
            self.log.level.as_str(),
            "trace" | "debug" | "info" | "warn" | "error"
        ) {
            return Err(invalid(
                "log.level",
                "must be one of trace, debug, info, warn, error",
            ));
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "0123456789abcdef0123456789abcdef";

    fn env(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    fn write_config(body: &str) -> (PathBuf, String) {
        let path = std::env::temp_dir().join(format!("trout-config-{}.toml", uuid::Uuid::new_v4()));

        std::fs::write(&path, body).unwrap();

        let as_str = path.to_string_lossy().into_owned();

        (path, as_str)
    }

    #[test]
    fn missing_file_uses_defaults_and_env_key() {
        let config = Config::load_from(&env(&[
            ("TROUT_CONFIG", "/nonexistent/trout.toml"),
            ("TROUT_ACCESS_KEY", KEY),
        ]))
        .unwrap();

        assert_eq!(config.listen, SocketAddr::from(([127, 0, 0, 1], 8787)));
        assert_eq!(config.targets.mode, TargetMode::AnyPublic);
        assert_eq!(config.limits.max_body_bytes, 16 * 1024 * 1024);
        assert_eq!(config.store.ttl_secs, 86_400);
    }

    #[test]
    fn missing_or_short_access_key_is_refused() {
        let missing = Config::load_from(&env(&[("TROUT_CONFIG", "/nonexistent/trout.toml")]));
        let short = Config::load_from(&env(&[
            ("TROUT_CONFIG", "/nonexistent/trout.toml"),
            ("TROUT_ACCESS_KEY", "short"),
        ]));

        assert!(matches!(
            missing,
            Err(ConfigError::Invalid {
                key: "access_key",
                ..
            })
        ));
        assert!(matches!(
            short,
            Err(ConfigError::Invalid {
                key: "access_key",
                ..
            })
        ));
    }

    #[test]
    fn env_overrides_file() {
        let (path, path_str) = write_config(&format!(
            "listen = \"127.0.0.1:9000\"\naccess_key = \"{KEY}\"\n[store]\nttl_secs = 120\npath = \"/tmp/a.db\"\n"
        ));
        let config = Config::load_from(&env(&[
            ("TROUT_CONFIG", &path_str),
            ("TROUT_LISTEN", "127.0.0.1:9100"),
            ("TROUT_STORE_TTL_SECS", "600"),
            ("TROUT_STORE_PATH", "/tmp/b.db"),
            ("TROUT_LOG_LEVEL", "debug"),
        ]))
        .unwrap();

        std::fs::remove_file(path).unwrap();

        assert_eq!(config.listen.port(), 9100);
        assert_eq!(config.store.ttl_secs, 600);
        assert_eq!(config.store.path, PathBuf::from("/tmp/b.db"));
        assert_eq!(config.log.level, "debug");
        assert_eq!(config.access_key, KEY);
    }

    #[test]
    fn file_targets_are_parsed_and_normalised() {
        let (path, path_str) = write_config(&format!(
            "access_key = \"{KEY}\"\n[targets]\nmode = \"listed\"\n[[targets.list]]\nbase_url = \"HTTPS://API.Example.com/v1/\"\naccepts_gzip = true\n"
        ));
        let config = Config::load_from(&env(&[("TROUT_CONFIG", &path_str)])).unwrap();

        std::fs::remove_file(path).unwrap();

        assert_eq!(config.targets.mode, TargetMode::Listed);
        assert_eq!(config.targets.list[0].base_url, "https://api.example.com/v1");
        assert!(config.targets.list[0].accepts_gzip);
        assert!(!config.targets.list[0].allow_private);
    }

    #[test]
    fn unknown_key_is_a_parse_error() {
        let (path, path_str) = write_config(&format!("access_key = \"{KEY}\"\nlog_bodies = true\n"));
        let result = Config::load_from(&env(&[("TROUT_CONFIG", &path_str)]));

        std::fs::remove_file(path).unwrap();

        assert!(matches!(result, Err(ConfigError::Parse { .. })));
    }

    fn valid() -> Config {
        Config {
            access_key: KEY.to_string(),
            ..Config::default()
        }
    }

    fn rejected_key(mut config: Config) -> &'static str {
        match config.validate() {
            Err(ConfigError::Invalid { key, .. }) => key,
            other => panic!("expected Invalid, got {other:?}"),
        }
    }

    #[test]
    fn validation_rules() {
        let mut c = valid();
        c.limits.max_decoded_body_bytes = c.limits.max_body_bytes - 1;
        assert_eq!(rejected_key(c), "limits.max_decoded_body_bytes");

        let mut c = valid();
        c.limits.first_byte_timeout_secs = 0;
        assert_eq!(rejected_key(c), "limits.first_byte_timeout_secs");

        let mut c = valid();
        c.store.ttl_secs = 59;
        assert_eq!(rejected_key(c), "store.ttl_secs");

        let mut c = valid();
        c.store.max_bytes = 1024;
        assert_eq!(rejected_key(c), "store.max_bytes");

        let mut c = valid();
        c.store.sweep_interval_secs = 301;
        assert_eq!(rejected_key(c), "store.sweep_interval_secs");

        let mut c = valid();
        c.log.level = "loud".to_string();
        assert_eq!(rejected_key(c), "log.level");

        let mut c = valid();
        c.targets.list.push(TargetEntry {
            base_url: "http://api.example.com/v1".to_string(),
            accepts_gzip: false,
            allow_private: false,
        });
        assert_eq!(rejected_key(c), "targets.list[].base_url");

        let mut c = valid();
        c.targets.list.push(TargetEntry {
            base_url: "https://api.example.com/v1?x=1".to_string(),
            accepts_gzip: false,
            allow_private: false,
        });
        assert_eq!(rejected_key(c), "targets.list[].base_url");
    }
}
