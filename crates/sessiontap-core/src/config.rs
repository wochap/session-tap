use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, fs, io, path::Path, time::Duration};

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
        }
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
    /// Hub sinks always deliver the complete normalized envelope and ignore
    /// field selection.
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

    /// Validates sink configuration invariants that cannot be expressed in the
    /// schema: hub sinks require a stable non-empty source identity, and sink
    /// URLs must satisfy the network safety policy.
    pub fn validate(&self) -> Result<(), String> {
        for (name, sink) in &self.sinks {
            match sink {
                SinkConfig::Http { url, .. } => validate_sink_url(url, &[])?,
                SinkConfig::Hub {
                    enabled,
                    url,
                    trusted_addresses,
                    ..
                } => {
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
    fn hub_sink_rejects_unknown_fields_and_remote_http() {
        assert!(
            toml::from_str::<Config>(
                "version=1\nsource_id='host'\n[sinks.hub]\ntype='hub'\nurl='http://127.0.0.1:9/x'\nfields=['cwd']\n"
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
}
