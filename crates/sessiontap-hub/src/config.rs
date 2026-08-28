use serde::{Deserialize, Serialize};
use std::{fs, io, path::Path};

fn default_version() -> u32 {
    1
}
fn default_retention() -> u64 {
    7
}
fn default_listen() -> String {
    "127.0.0.1:8931".into()
}
fn default_max_body() -> usize {
    1024 * 1024
}

/// Versioned hub configuration. Unknown fields are rejected so a rule set is
/// never silently broadened or partially applied.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HubConfig {
    #[serde(default = "default_version")]
    pub version: u32,
    /// HTTP ingestion bind address.
    #[serde(default = "default_listen")]
    pub listen: String,
    /// Optional private bearer-token file. When neither side configures a
    /// token, ingestion is unauthenticated.
    #[serde(default)]
    pub token_file: Option<String>,
    #[serde(default = "default_retention")]
    pub retention_days: u64,
    #[serde(default = "default_max_body")]
    pub max_body_bytes: usize,
    #[serde(default)]
    pub subscriptions: Vec<Subscription>,
}

impl Default for HubConfig {
    fn default() -> Self {
        Self {
            version: 1,
            listen: default_listen(),
            token_file: None,
            retention_days: 7,
            max_body_bytes: default_max_body(),
            subscriptions: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Subscription {
    #[serde(default)]
    pub name: Option<String>,
    /// Normalized match criteria; different fields are ANDed, values within
    /// one field are ORed.
    #[serde(default, rename = "match")]
    pub match_criteria: MatchCriteria,
    /// Canonical fields that must materially change against the previously
    /// persisted state for the subscription to run.
    #[serde(default)]
    pub changes: Vec<String>,
    /// Argument arrays executed directly without shell evaluation.
    pub commands: Vec<Vec<String>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct MatchCriteria {
    pub sources: Vec<String>,
    pub providers: Vec<String>,
    pub events: Vec<String>,
    pub statuses: Vec<String>,
    pub lifecycles: Vec<String>,
    pub repositories: Vec<String>,
}

impl HubConfig {
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
        let config: Self = serde_yaml::from_str(&raw)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        config
            .validate()
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.version != 1 {
            return Err(format!(
                "unsupported hub configuration version {}",
                self.version
            ));
        }
        if self.listen.parse::<std::net::SocketAddr>().is_err() {
            return Err(format!("invalid listen address: {}", self.listen));
        }
        for (index, subscription) in self.subscriptions.iter().enumerate() {
            if subscription.commands.is_empty() {
                return Err(format!("subscription #{index} has no commands"));
            }
            for (command_index, command) in subscription.commands.iter().enumerate() {
                if command.is_empty() {
                    return Err(format!(
                        "subscription #{index} command #{command_index} is empty"
                    ));
                }
            }
            for field in &subscription.changes {
                if !crate::store::CANONICAL_FIELDS.contains(&field.as_str()) {
                    return Err(format!(
                        "subscription #{index} watches unknown field '{field}'"
                    ));
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_bind_loopback_and_local_only() {
        let config = HubConfig::default();
        assert_eq!(config.listen, "127.0.0.1:8931");
        assert!(config.token_file.is_none());
        assert!(config.subscriptions.is_empty());
        config.validate().unwrap();
    }

    #[test]
    fn parses_versioned_configuration_with_subscriptions() {
        let config: HubConfig = serde_yaml::from_str(
            r#"
version: 1
listen: "127.0.0.1:8931"
token_file: /run/keys/sessiontap-hub-token
retention_days: 14
subscriptions:
  - name: waiting-notify
    match:
      sources: [sandbox]
      providers: [codex, claude]
      events: [waiting_input, waiting_approval]
      statuses: [blocked]
    changes: [status, attention]
    commands:
      - ["notify-send", "agent waiting"]
"#,
        )
        .unwrap();
        config.validate().unwrap();
        assert_eq!(config.retention_days, 14);
        assert_eq!(config.subscriptions.len(), 1);
        let sub = &config.subscriptions[0];
        assert_eq!(sub.match_criteria.sources, vec!["sandbox".to_owned()]);
        assert_eq!(sub.commands[0], vec!["notify-send", "agent waiting"]);
    }

    #[test]
    fn unknown_fields_and_versions_are_rejected() {
        // unsupported versions parse but are rejected by validation so load
        // reports the error instead of silently running
        let future: HubConfig = serde_yaml::from_str("version: 2\n").unwrap();
        assert!(future.validate().is_err());
        assert!(HubConfig::default().validate().is_ok());
        assert!(serde_yaml::from_str::<HubConfig>("version: 1\nbogus: true\n").is_err());
        assert!(
            serde_yaml::from_str::<HubConfig>(
                "version: 1\nsubscriptions:\n  - commands: [[\"x\"]]\n    match:\n      nope: []\n"
            )
            .is_err()
        );
    }

    #[test]
    fn invalid_subscriptions_report_errors() {
        let config = HubConfig {
            subscriptions: vec![Subscription {
                name: None,
                match_criteria: MatchCriteria::default(),
                changes: vec![],
                commands: vec![],
            }],
            ..Default::default()
        };
        assert!(config.validate().unwrap_err().contains("no commands"));

        let config = HubConfig {
            subscriptions: vec![Subscription {
                name: None,
                match_criteria: MatchCriteria::default(),
                changes: vec!["bogus".into()],
                commands: vec![vec!["true".into()]],
            }],
            ..Default::default()
        };
        assert!(config.validate().unwrap_err().contains("unknown field"));

        let config = HubConfig {
            listen: "not-an-address".into(),
            ..Default::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn config_load_follows_symlink() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("target.yaml");
        fs::write(&target, "version: 1\nretention_days: 99\n").unwrap();
        let link = temp.path().join("config.yaml");
        symlink(&target, &link).unwrap();
        let config = HubConfig::load(&link).unwrap();
        assert_eq!(config.retention_days, 99);
    }

    #[test]
    fn config_load_rejects_dangling_symlink() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        let link = temp.path().join("config.yaml");
        symlink(temp.path().join("missing.yaml"), &link).unwrap();
        let error = HubConfig::load(&link).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert!(error.to_string().contains("symlink target not found"));
    }
}
