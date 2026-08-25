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
            Self::Stdout { enabled, .. } | Self::Http { enabled, .. } => *enabled,
        }
    }
    #[must_use]
    pub fn timeout(&self) -> Duration {
        match self {
            Self::Http { timeout_ms, .. } => Duration::from_millis(*timeout_ms),
            Self::Stdout { .. } => Duration::ZERO,
        }
    }

    #[must_use]
    pub fn fields(&self) -> &[String] {
        match self {
            Self::Stdout { fields, .. } | Self::Http { fields, .. } => fields,
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> io::Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        if fs::symlink_metadata(path)?.file_type().is_symlink() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "configuration file must not be a symlink",
            ));
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
    fn config_load_rejects_symlink_without_reading_target() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("target.toml");
        fs::write(&target, "version=1\nretention_days=99\n").unwrap();
        let link = temp.path().join("config.toml");
        symlink(&target, &link).unwrap();
        assert_eq!(
            Config::load(&link).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        assert_eq!(
            fs::read_to_string(target).unwrap(),
            "version=1\nretention_days=99\n"
        );
    }
}
