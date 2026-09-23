use serde::{Deserialize, Serialize};
use sessiontap_core::domain::{PublicField, PublicReasonKind, PublicStatus};
use std::{io, path::Path};

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
fn default_max_concurrent_commands() -> usize {
    4
}
fn default_command_timeout_secs() -> u64 {
    30
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
    /// Subscription commands running at once across all deliveries.
    #[serde(default = "default_max_concurrent_commands")]
    pub max_concurrent_commands: usize,
    /// A subscription command running longer than this is killed.
    #[serde(default = "default_command_timeout_secs")]
    pub command_timeout_secs: u64,
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
            max_concurrent_commands: default_max_concurrent_commands(),
            command_timeout_secs: default_command_timeout_secs(),
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
    pub changes: Vec<PublicField>,
    /// Argument arrays executed directly without shell evaluation.
    pub commands: Vec<Vec<String>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct MatchCriteria {
    pub sources: Vec<String>,
    pub providers: Vec<String>,
    pub statuses: Vec<PublicStatus>,
    pub reasons: Vec<PublicReasonKind>,
    pub repositories: Vec<String>,
}

impl HubConfig {
    pub fn load(path: &Path) -> io::Result<Self> {
        let Some(raw) = sessiontap_infra::fs::read_private_config(path)? else {
            return Ok(Self::default());
        };
        let config =
            Self::parse(&raw).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        config
            .validate()
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        Ok(config)
    }

    /// Parses YAML configuration. Field, status, and reason names are
    /// deserialized straight into the canonical public enums, so the accepted
    /// names are exactly those the public view can carry. A subscription
    /// error names the subscription index.
    pub fn parse(raw: &str) -> Result<Self, String> {
        let mut document: serde_yaml::Value =
            serde_yaml::from_str(raw).map_err(|error| error.to_string())?;
        let subscriptions = match &mut document {
            serde_yaml::Value::Mapping(mapping) => mapping.remove("subscriptions"),
            _ => None,
        };
        let mut config: Self =
            serde_yaml::from_value(document).map_err(|error| error.to_string())?;
        if let Some(subscriptions) = subscriptions {
            let serde_yaml::Value::Sequence(items) = subscriptions else {
                return Err("subscriptions must be a list".into());
            };
            config.subscriptions = items
                .into_iter()
                .enumerate()
                .map(|(index, item)| {
                    serde_yaml::from_value(item)
                        .map_err(|error| format!("subscription #{index}: {error}"))
                })
                .collect::<Result<_, _>>()?;
        }
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
        if self.max_concurrent_commands == 0 {
            return Err("max_concurrent_commands must be at least 1".into());
        }
        if self.command_timeout_secs == 0 {
            return Err("command_timeout_secs must be at least 1".into());
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
      statuses: [blocked]
      reasons: [input, approval]
    changes: [status, reason]
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
        for private in [
            "events",
            "lifecycles",
            "activities",
            "processes",
            "multiplexers",
        ] {
            let yaml = format!(
                "version: 1\nsubscriptions:\n  - commands: [[\"x\"]]\n    match:\n      {private}: [value]\n"
            );
            assert!(
                serde_yaml::from_str::<HubConfig>(&yaml).is_err(),
                "accepted private routing field {private}"
            );
        }
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
            listen: "not-an-address".into(),
            ..Default::default()
        };
        assert!(config.validate().is_err());
    }

    fn subscription_error(body: &str) -> String {
        let yaml = format!(
            "version: 1\nsubscriptions:\n  - commands: [[\"x\"]]\n  - name: second\n{body}"
        );
        HubConfig::parse(&yaml).unwrap_err()
    }

    #[test]
    fn unknown_changed_field_is_rejected_with_index() {
        let error = subscription_error("    changes: [bogus]\n    commands: [[\"x\"]]\n");
        assert!(error.contains("subscription #1"), "{error}");
        assert!(error.contains("bogus"), "{error}");
        assert!(error.contains("repository"), "{error}");
    }

    #[test]
    fn wrong_case_status_is_rejected_with_accepted_values() {
        let error =
            subscription_error("    match:\n      statuses: [Blocked]\n    commands: [[\"x\"]]\n");
        assert!(error.contains("subscription #1"), "{error}");
        assert!(error.contains("Blocked"), "{error}");
        for accepted in ["running", "idle", "blocked", "stopped"] {
            assert!(error.contains(accepted), "{error}");
        }
    }

    #[test]
    fn unknown_reason_is_rejected() {
        let error =
            subscription_error("    match:\n      reasons: [unknown]\n    commands: [[\"x\"]]\n");
        assert!(error.contains("subscription #1"), "{error}");
        assert!(error.contains("approval"), "{error}");
    }

    #[test]
    fn every_public_field_is_accepted() {
        let fields = [
            PublicField::InvocationId,
            PublicField::Provider,
            PublicField::Status,
            PublicField::Reason,
            PublicField::Cwd,
            PublicField::CreatedAt,
            PublicField::UpdatedAt,
            PublicField::Session,
            PublicField::Metadata,
            PublicField::Usage,
            PublicField::Repository,
            PublicField::Children,
        ];
        // Exhaustive: adding a variant fails to compile here until listed.
        for field in fields {
            match field {
                PublicField::InvocationId
                | PublicField::Provider
                | PublicField::Status
                | PublicField::Reason
                | PublicField::Cwd
                | PublicField::CreatedAt
                | PublicField::UpdatedAt
                | PublicField::Session
                | PublicField::Metadata
                | PublicField::Usage
                | PublicField::Repository
                | PublicField::Children => {}
            }
        }
        let names: Vec<String> = fields
            .iter()
            .map(|field| {
                serde_json::to_value(field)
                    .unwrap()
                    .as_str()
                    .unwrap()
                    .to_owned()
            })
            .collect();
        let yaml = format!(
            "version: 1\nsubscriptions:\n  - changes: [{}]\n    commands: [[\"x\"]]\n",
            names.join(", ")
        );
        let config = HubConfig::parse(&yaml).unwrap();
        config.validate().unwrap();
        assert_eq!(config.subscriptions[0].changes, fields);
    }

    #[test]
    fn command_limits_default_and_parse() {
        let config = HubConfig::parse("version: 1\n").unwrap();
        assert_eq!(config.max_concurrent_commands, 4);
        assert_eq!(config.command_timeout_secs, 30);
        let config =
            HubConfig::parse("version: 1\nmax_concurrent_commands: 2\ncommand_timeout_secs: 5\n")
                .unwrap();
        assert_eq!(config.max_concurrent_commands, 2);
        assert_eq!(config.command_timeout_secs, 5);
        let zero = HubConfig::parse("version: 1\nmax_concurrent_commands: 0\n").unwrap();
        assert!(zero.validate().is_err());
    }

    /// Every complete configuration example in the hub guide must load.
    #[test]
    fn documented_examples_load() {
        let guide = include_str!("../../../docs/hub.md");
        let examples: Vec<&str> = guide
            .split("```yaml\n")
            .skip(1)
            .filter_map(|block| block.split_once("```").map(|(yaml, _)| yaml))
            .filter(|yaml| yaml.starts_with("version: 1"))
            .collect();
        assert!(examples.len() >= 2, "hub guide lost its examples");
        for yaml in examples {
            let config = HubConfig::parse(yaml).unwrap_or_else(|error| panic!("{error}\n{yaml}"));
            config.validate().unwrap();
        }
    }

    #[test]
    fn config_load_follows_symlink() {
        use std::{fs, os::unix::fs::symlink};
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
