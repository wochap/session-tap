use crate::{ProviderId, domain::PublicField};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs, io,
    path::Path,
    time::Duration,
};

fn default_version() -> u32 {
    1
}
fn default_retention() -> u64 {
    7
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default = "default_version")]
    pub version: u32,
    #[serde(default = "default_retention")]
    pub retention_days: u64,
    /// Stable source identity required for hub delivery.
    #[serde(default)]
    pub source_id: Option<String>,
    /// Optional human-readable source display name included in hub envelopes.
    #[serde(default)]
    pub source_name: Option<String>,
    #[serde(default)]
    pub adapters: BTreeMap<String, CustomAdapter>,
    #[serde(default)]
    pub sinks: BTreeMap<String, SinkConfig>,
    #[serde(default)]
    pub daemon: DaemonConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            version: 1,
            retention_days: 7,
            source_id: None,
            source_name: None,
            adapters: BTreeMap::new(),
            sinks: BTreeMap::new(),
            daemon: DaemonConfig::default(),
        }
    }
}

/// Daemon tuning knobs. Defaults match the values the daemon has always used.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DaemonConfig {
    /// Concurrent provider artifact collections.
    pub collection_workers: usize,
    /// Outbox poll interval in milliseconds.
    pub sink_poll_ms: u64,
    /// Outbox records delivered per poll.
    pub outbox_batch: usize,
    /// Stale-working sweep interval in seconds.
    pub stale_sweep_secs: u64,
    /// Local update broadcast capacity before listeners lag.
    pub update_buffer: usize,
    /// Attempts after which a permanently rejected delivery is dropped.
    pub max_rejected_attempts: u32,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            collection_workers: 4,
            sink_poll_ms: 250,
            outbox_batch: 100,
            stale_sweep_secs: 60,
            update_buffer: 1024,
            max_rejected_attempts: 16,
        }
    }
}

impl DaemonConfig {
    #[must_use]
    pub const fn sink_poll(&self) -> Duration {
        Duration::from_millis(self.sink_poll_ms)
    }

    #[must_use]
    pub const fn stale_sweep(&self) -> Duration {
        Duration::from_secs(self.stale_sweep_secs)
    }

    /// Rejects zero values and a poll interval above one minute.
    pub fn validate(&self) -> Result<(), String> {
        for (key, value) in [
            ("collection_workers", self.collection_workers as u64),
            ("sink_poll_ms", self.sink_poll_ms),
            ("outbox_batch", self.outbox_batch as u64),
            ("stale_sweep_secs", self.stale_sweep_secs),
            ("update_buffer", self.update_buffer as u64),
            (
                "max_rejected_attempts",
                u64::from(self.max_rejected_attempts),
            ),
        ] {
            if value == 0 {
                return Err(format!("daemon.{key} must be at least 1"));
            }
        }
        if self.sink_poll_ms > 60_000 {
            return Err("daemon.sink_poll_ms must be at most 60000".into());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CustomAdapter {
    pub executable: String,
    pub inherits: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum SinkConfig {
    Stdout {
        #[serde(default)]
        enabled: bool,
        #[serde(default)]
        fields: Vec<String>,
    },
    Http {
        #[serde(default)]
        enabled: bool,
        url: String,
        token_env: Option<String>,
        token_file: Option<String>,
        #[serde(default = "default_timeout_ms")]
        timeout_ms: u64,
        #[serde(default = "default_payload_limit")]
        max_payload_bytes: usize,
        #[serde(default)]
        fields: Vec<String>,
    },
    /// Canonical hub sink delivering versioned source snapshots and updates.
    /// Hub sinks always deliver the complete normalized envelope; a non-empty
    /// `fields` list is rejected by validation.
    Hub {
        #[serde(default)]
        enabled: bool,
        url: String,
        #[serde(default)]
        token_env: Option<String>,
        #[serde(default)]
        token_file: Option<String>,
        #[serde(default = "default_timeout_ms")]
        timeout_ms: u64,
        #[serde(default = "default_payload_limit")]
        max_payload_bytes: usize,
        /// Non-loopback local addresses (for example a sandbox host address)
        /// explicitly trusted for cleartext HTTP delivery.
        #[serde(default)]
        trusted_addresses: Vec<String>,
        /// Accepted only so validation can name the sink; must stay empty.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        fields: Vec<String>,
    },
}
fn default_timeout_ms() -> u64 {
    3_000
}
fn default_payload_limit() -> usize {
    256 * 1024
}

impl SinkConfig {
    #[must_use]
    pub const fn enabled(&self) -> bool {
        match self {
            Self::Stdout { enabled, .. }
            | Self::Http { enabled, .. }
            | Self::Hub { enabled, .. } => *enabled,
        }
    }
    #[must_use]
    pub fn timeout(&self) -> Duration {
        match self {
            Self::Http { timeout_ms, .. } | Self::Hub { timeout_ms, .. } => {
                Duration::from_millis(*timeout_ms)
            }
            Self::Stdout { .. } => Duration::ZERO,
        }
    }

    #[must_use]
    pub fn fields(&self) -> &[String] {
        match self {
            Self::Stdout { fields, .. } | Self::Http { fields, .. } => fields,
            Self::Hub { .. } => &[],
        }
    }

    /// Resolves the configured field selection. An empty set means the
    /// complete public view.
    pub fn public_fields(&self) -> Result<BTreeSet<PublicField>, String> {
        self.fields()
            .iter()
            .map(|name| {
                serde_json::from_value::<PublicField>(serde_json::Value::String(name.clone()))
                    .map_err(|_| format!("unknown public field '{name}'"))
            })
            .collect()
    }

    #[must_use]
    pub const fn is_hub(&self) -> bool {
        matches!(self, Self::Hub { .. })
    }

    #[must_use]
    pub fn max_payload_bytes(&self) -> usize {
        match self {
            Self::Http {
                max_payload_bytes, ..
            }
            | Self::Hub {
                max_payload_bytes, ..
            } => *max_payload_bytes,
            Self::Stdout { .. } => default_payload_limit(),
        }
    }
}

/// Network safety policy for sink URLs: HTTPS is permitted anywhere; cleartext
/// HTTP is limited to loopback or explicitly configured trusted local
/// addresses.
pub fn validate_sink_url(raw: &str, trusted_addresses: &[String]) -> Result<(), String> {
    let url = url::Url::parse(raw).map_err(|e| format!("invalid sink URL: {e}"))?;
    if url.scheme() == "https" {
        return Ok(());
    }
    if url.scheme() != "http" {
        return Err(format!("unsupported sink URL scheme: {}", url.scheme()));
    }
    let ip = match url.host() {
        Some(url::Host::Domain("localhost")) => return Ok(()),
        Some(url::Host::Ipv4(ip)) => std::net::IpAddr::V4(ip),
        Some(url::Host::Ipv6(ip)) => std::net::IpAddr::V6(ip),
        _ => {
            return Err(format!(
                "HTTP sinks require HTTPS except for loopback or explicitly trusted local addresses: {raw}"
            ));
        }
    };
    if ip.is_loopback() {
        return Ok(());
    }
    if trusted_addresses
        .iter()
        .any(|trusted| trusted.parse::<std::net::IpAddr>().is_ok_and(|t| t == ip))
    {
        return Ok(());
    }
    Err(format!(
        "HTTP sinks require HTTPS except for loopback or explicitly trusted local addresses: {raw}"
    ))
}

impl Config {
    pub fn load(path: &Path) -> io::Result<Self> {
        let symlink =
            fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink());
        if !path.exists() {
            if symlink {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    "configuration symlink target not found",
                ));
            }
            return Ok(Self::default());
        }
        let raw = fs::read_to_string(path)?;
        let config: Self =
            toml::from_str(&raw).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        if config.version != 1 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unsupported config version {}", config.version),
            ));
        }
        Ok(config)
    }

    /// Validates configuration invariants that cannot be expressed in the
    /// schema: every adapter alias inherits a built-in provider, hub sinks
    /// require a stable non-empty source identity, and sink URLs must satisfy
    /// the network safety policy.
    pub fn validate(&self) -> Result<(), String> {
        for (name, adapter) in &self.adapters {
            if adapter.inherits.parse::<ProviderId>().is_err() {
                return Err(format!(
                    "adapter '{name}' inherits unknown provider '{}'; expected one of {}",
                    adapter.inherits,
                    ProviderId::joined(", ")
                ));
            }
        }
        self.daemon.validate()?;
        for (name, sink) in &self.sinks {
            sink.public_fields()
                .map_err(|error| format!("sink '{name}': {error}"))?;
            match sink {
                SinkConfig::Http { url, .. } => validate_sink_url(url, &[])?,
                SinkConfig::Hub {
                    enabled,
                    url,
                    trusted_addresses,
                    fields,
                    ..
                } => {
                    if !fields.is_empty() {
                        return Err(format!(
                            "sink '{name}' is a hub sink and does not accept fields; hub envelopes are always complete"
                        ));
                    }
                    validate_sink_url(url, trusted_addresses)?;
                    if *enabled && self.source_id.as_deref().is_none_or(str::is_empty) {
                        return Err(format!(
                            "sink '{name}' is a hub sink and requires a non-empty source_id"
                        ));
                    }
                }
                SinkConfig::Stdout { .. } => {}
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn defaults_are_private_and_local() {
        let c = Config::default();
        assert_eq!(c.retention_days, 7);
        assert!(c.sinks.is_empty());
    }
    #[test]
    fn parses_custom_adapter_and_disabled_sink() {
        let c: Config = toml::from_str("version=1\n[adapters.acme]\nexecutable='company-claude'\ninherits='claude'\n[sinks.debug]\ntype='stdout'\nenabled=false\n").unwrap();
        assert_eq!(c.adapters["acme"].inherits, "claude");
        assert!(!c.sinks["debug"].enabled());
        c.validate().unwrap();
    }

    #[test]
    fn alias_with_unknown_inherits_is_rejected() {
        let c: Config =
            toml::from_str("version=1\n[adapters.acme]\nexecutable='acme'\ninherits='gemini'\n")
                .unwrap();
        let error = c.validate().unwrap_err();
        assert!(error.contains("'acme'"), "{error}");
        assert!(error.contains("'gemini'"), "{error}");
        assert!(error.contains("claude, codex, pi, qwen"), "{error}");
    }

    #[test]
    fn config_load_follows_symlink() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("target.toml");
        fs::write(&target, "version=1\nretention_days=99\n").unwrap();
        let link = temp.path().join("config.toml");
        symlink(&target, &link).unwrap();
        let config = Config::load(&link).unwrap();
        assert_eq!(config.retention_days, 99);
    }

    #[test]
    fn config_load_rejects_dangling_symlink() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        let link = temp.path().join("config.toml");
        symlink(temp.path().join("missing.toml"), &link).unwrap();
        let error = Config::load(&link).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert!(error.to_string().contains("symlink target not found"));
    }

    #[test]
    fn parses_source_identity_and_hub_sink() {
        let c: Config = toml::from_str(
            r#"
version = 1
source_id = "host"
source_name = "Host machine"

[sinks.hub]
type = "hub"
enabled = true
url = "http://127.0.0.1:8931/ingest"
token_file = "/run/keys/sessiontap-hub-token"
timeout_ms = 1500
max_payload_bytes = 65536
trusted_addresses = ["192.168.100.1"]
"#,
        )
        .unwrap();
        assert_eq!(c.source_id.as_deref(), Some("host"));
        assert_eq!(c.source_name.as_deref(), Some("Host machine"));
        let sink = &c.sinks["hub"];
        assert!(sink.enabled());
        assert!(sink.is_hub());
        assert_eq!(sink.timeout(), Duration::from_millis(1500));
        assert_eq!(sink.max_payload_bytes(), 65536);
        assert!(sink.fields().is_empty());
        c.validate().unwrap();
    }

    #[test]
    fn hub_sink_defaults_and_omitted_credential() {
        let c: Config = toml::from_str(
            "version=1\nsource_id='host'\n[sinks.hub]\ntype='hub'\nenabled=true\nurl='http://127.0.0.1:9/ingest'\n",
        )
        .unwrap();
        let sink = &c.sinks["hub"];
        assert_eq!(sink.timeout(), Duration::from_millis(3_000));
        assert_eq!(sink.max_payload_bytes(), 256 * 1024);
        c.validate().unwrap();
    }

    #[test]
    fn hub_sink_requires_source_id() {
        let c: Config = toml::from_str(
            "version=1\n[sinks.hub]\ntype='hub'\nenabled=true\nurl='http://127.0.0.1:9/ingest'\n",
        )
        .unwrap();
        assert!(c.validate().unwrap_err().contains("source_id"));
        let empty: Config = toml::from_str(
            "version=1\nsource_id=''\n[sinks.hub]\ntype='hub'\nenabled=true\nurl='http://127.0.0.1:9/ingest'\n",
        )
        .unwrap();
        assert!(empty.validate().is_err());
    }

    #[test]
    fn disabled_hub_sink_still_requires_source_id() {
        let c: Config = toml::from_str(
            "version=1\n[sinks.hub]\ntype='hub'\nenabled=false\nurl='http://127.0.0.1:9/ingest'\n",
        )
        .unwrap();
        assert!(c.validate().is_ok());
    }

    #[test]
    fn sink_url_policy_limits_cleartext_http() {
        assert!(validate_sink_url("http://127.0.0.1:8931/ingest", &[]).is_ok());
        assert!(validate_sink_url("http://[::1]:8931/ingest", &[]).is_ok());
        assert!(validate_sink_url("http://localhost:8931/ingest", &[]).is_ok());
        assert!(validate_sink_url("https://hub.example.com/ingest", &[]).is_ok());
        assert!(validate_sink_url("http://example.com/ingest", &[]).is_err());
        assert!(validate_sink_url("ftp://127.0.0.1/ingest", &[]).is_err());
        let err = validate_sink_url("http://192.168.100.1:8931/ingest", &[]);
        assert!(err.is_err());
        assert!(
            validate_sink_url(
                "http://192.168.100.1:8931/ingest",
                &["192.168.100.1".to_owned()]
            )
            .is_ok()
        );
        assert!(
            validate_sink_url(
                "http://192.168.100.2:8931/ingest",
                &["192.168.100.1".to_owned()]
            )
            .is_err()
        );
    }

    #[test]
    fn hub_sink_rejects_field_selection_and_remote_http() {
        let with_fields: Config = toml::from_str(
            "version=1\nsource_id='host'\n[sinks.hub]\ntype='hub'\nurl='http://127.0.0.1:9/x'\nfields=['cwd']\n",
        )
        .unwrap();
        let error = with_fields.validate().unwrap_err();
        assert!(
            error.contains("'hub'") && error.contains("fields"),
            "{error}"
        );
        assert!(
            toml::from_str::<Config>(
                "version=1\nsource_id='host'\n[sinks.hub]\ntype='hub'\nurl='http://127.0.0.1:9/x'\nbogus=1\n"
            )
            .is_err()
        );
        let c: Config = toml::from_str(
            "version=1\nsource_id='host'\n[sinks.hub]\ntype='hub'\nenabled=true\nurl='http://example.com/ingest'\n",
        )
        .unwrap();
        assert!(c.validate().is_err());
    }

    #[test]
    fn unsupported_config_version_is_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.toml");
        fs::write(&path, "version=2\n").unwrap();
        assert_eq!(
            Config::load(&path).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn existing_stdout_and_http_sinks_keep_their_shape() {
        let c: Config = toml::from_str(
            r#"
version = 1
[sinks.debug]
type = "stdout"
enabled = true
fields = ["cwd"]
[sinks.archive]
type = "http"
enabled = true
url = "http://127.0.0.1:8787/events"
fields = ["cwd"]
"#,
        )
        .unwrap();
        assert_eq!(c.sinks["debug"].fields(), &["cwd".to_owned()]);
        assert!(!c.sinks["debug"].is_hub());
        assert_eq!(c.sinks["archive"].fields(), &["cwd".to_owned()]);
        c.validate().unwrap();
    }

    #[test]
    fn unknown_sink_field_is_rejected_with_sink_and_field_names() {
        let c: Config = toml::from_str(
            "version=1\n[sinks.archive]\ntype='http'\nurl='http://127.0.0.1:9/x'\nfields=['status','transcript']\n",
        )
        .unwrap();
        let error = c.validate().unwrap_err();
        assert!(error.contains("'archive'"), "{error}");
        assert!(error.contains("'transcript'"), "{error}");
        assert_eq!(
            c.sinks["archive"].public_fields().unwrap_err(),
            "unknown public field 'transcript'"
        );
        let ok: Config =
            toml::from_str("version=1\n[sinks.debug]\ntype='stdout'\nfields=['status','usage']\n")
                .unwrap();
        assert_eq!(
            ok.sinks["debug"].public_fields().unwrap(),
            BTreeSet::from([PublicField::Status, PublicField::Usage])
        );
    }

    #[test]
    fn daemon_section_defaults_to_historical_values() {
        let c: Config = toml::from_str("version=1\n").unwrap();
        assert_eq!(c.daemon, DaemonConfig::default());
        assert_eq!(c.daemon.collection_workers, 4);
        assert_eq!(c.daemon.sink_poll(), Duration::from_millis(250));
        assert_eq!(c.daemon.outbox_batch, 100);
        assert_eq!(c.daemon.stale_sweep(), Duration::from_secs(60));
        assert_eq!(c.daemon.update_buffer, 1024);
        assert_eq!(c.daemon.max_rejected_attempts, 16);
        c.validate().unwrap();
    }

    #[test]
    fn daemon_section_overrides_and_keeps_other_defaults() {
        let c: Config =
            toml::from_str("version=1\n[daemon]\nsink_poll_ms=1000\ncollection_workers=2\n")
                .unwrap();
        assert_eq!(c.daemon.sink_poll_ms, 1000);
        assert_eq!(c.daemon.collection_workers, 2);
        assert_eq!(c.daemon.outbox_batch, 100);
        c.validate().unwrap();
        assert!(toml::from_str::<Config>("version=1\n[daemon]\nbogus=1\n").is_err());
    }

    #[test]
    fn daemon_section_rejects_zero_and_slow_poll() {
        for key in [
            "collection_workers",
            "sink_poll_ms",
            "outbox_batch",
            "stale_sweep_secs",
            "update_buffer",
            "max_rejected_attempts",
        ] {
            let c: Config = toml::from_str(&format!("version=1\n[daemon]\n{key}=0\n")).unwrap();
            let error = c.validate().unwrap_err();
            assert!(error.contains(key), "{error}");
        }
        let c: Config = toml::from_str("version=1\n[daemon]\nsink_poll_ms=60001\n").unwrap();
        assert!(c.validate().unwrap_err().contains("sink_poll_ms"));
    }

    #[test]
    fn cli_doc_config_examples_parse_and_validate() {
        let doc = include_str!("../../../docs/cli.md");
        let examples: Vec<&str> = doc
            .split("```toml\n")
            .skip(1)
            .map(|block| block.split("```").next().unwrap())
            .collect();
        assert!(examples.len() >= 4);
        for example in examples {
            let source = if example.contains("version") {
                example.to_owned()
            } else {
                format!("version = 1\n{example}")
            };
            let config: Config =
                toml::from_str(&source).unwrap_or_else(|error| panic!("{error}\n{example}"));
            config
                .validate()
                .unwrap_or_else(|error| panic!("{error}\n{example}"));
        }
    }
}
