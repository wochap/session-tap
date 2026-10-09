use serde::{Deserialize, Serialize};
use sessiontap_core::domain::{PublicField, PublicReasonKind, PublicStatus};
use std::{collections::BTreeMap, io, net::SocketAddr, path::Path};

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
    /// Ingestion tokens keyed by source ID. Each token authorizes writes
    /// only for the sources whose token file yields it. Empty means
    /// unauthenticated ingestion, which validation allows only on loopback.
    #[serde(default)]
    pub sources: BTreeMap<String, SourceAuth>,
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
    /// Remote device access; absent means no remote port is opened.
    #[serde(default)]
    pub remote: Option<RemoteConfig>,
}

/// Private bearer-token file for one ingestion source.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceAuth {
    pub token_file: String,
}

/// Remote listener for paired devices. `listen` holds concrete IP and port
/// entries, or a single wildcard entry (`0.0.0.0:<port>` or `[::]:<port>`)
/// that binds every interface.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteConfig {
    /// Display name shown to devices; defaults to the host name.
    #[serde(default)]
    pub name: Option<String>,
    pub listen: Vec<String>,
    /// Extra `host:port` endpoint hints placed in the pairing QR code.
    #[serde(default)]
    pub advertise: Vec<String>,
    /// Makes the terminal scopes (`watch`, `control`) grantable and effective.
    #[serde(default)]
    pub control: bool,
    /// Announces the wildcard listener over DNS-SD on the local network.
    #[serde(default)]
    pub discovery: bool,
}

/// How the remote listener binds, derived from a validated `remote.listen`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListenMode {
    /// Concrete addresses in configuration order.
    Explicit(Vec<SocketAddr>),
    /// One wildcard address covering every interface of its family.
    Wildcard(SocketAddr),
}

impl ListenMode {
    /// Every address to bind.
    #[must_use]
    pub fn addresses(&self) -> Vec<SocketAddr> {
        match self {
            Self::Explicit(addresses) => addresses.clone(),
            Self::Wildcard(address) => vec![*address],
        }
    }
}

impl RemoteConfig {
    /// Parsed bind mode; call after `validate`.
    #[must_use]
    pub fn listen_mode(&self) -> ListenMode {
        let addresses: Vec<SocketAddr> = self
            .listen
            .iter()
            .filter_map(|entry| entry.parse().ok())
            .collect();
        match addresses.as_slice() {
            [address] if address.ip().is_unspecified() => ListenMode::Wildcard(*address),
            _ => ListenMode::Explicit(addresses),
        }
    }

    #[must_use]
    pub fn display_name(&self) -> String {
        self.name.clone().unwrap_or_else(host_name)
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.listen.is_empty() {
            return Err("remote.listen must name at least one address".into());
        }
        for entry in &self.listen {
            let address: SocketAddr = entry
                .parse()
                .map_err(|_| format!("invalid remote.listen address: {entry}"))?;
            if address.ip().is_unspecified() && self.listen.len() > 1 {
                return Err(format!(
                    "remote.listen wildcard {entry} must be the only remote.listen entry"
                ));
            }
        }
        if self.discovery && !matches!(self.listen_mode(), ListenMode::Wildcard(_)) {
            return Err(
                "remote.discovery requires remote.listen to be a single wildcard entry \
                 (0.0.0.0:<port> or [::]:<port>)"
                    .into(),
            );
        }
        if let Some(entry) = self.advertise.iter().find(|entry| entry.trim().is_empty()) {
            return Err(format!("invalid remote.advertise entry: {entry:?}"));
        }
        if self
            .name
            .as_deref()
            .is_some_and(|name| name.trim().is_empty())
        {
            return Err("remote.name must not be empty".into());
        }
        Ok(())
    }
}

fn host_name() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .or_else(|_| std::fs::read_to_string("/etc/hostname"))
        .map(|name| name.trim().to_owned())
        .ok()
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "sessiontap-hub".into())
}

impl Default for HubConfig {
    fn default() -> Self {
        Self {
            version: 1,
            listen: default_listen(),
            sources: BTreeMap::new(),
            retention_days: 7,
            max_body_bytes: default_max_body(),
            max_concurrent_commands: default_max_concurrent_commands(),
            command_timeout_secs: default_command_timeout_secs(),
            subscriptions: Vec::new(),
            remote: None,
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
        let Ok(listen) = self.listen.parse::<std::net::SocketAddr>() else {
            return Err(format!("invalid listen address: {}", self.listen));
        };
        for (source, auth) in &self.sources {
            if source.trim().is_empty() {
                return Err("sources must not contain an empty source ID".into());
            }
            if auth.token_file.trim().is_empty() {
                return Err(format!("sources.{source}.token_file must not be empty"));
            }
        }
        if !listen.ip().is_loopback() && self.sources.is_empty() {
            return Err(format!(
                "ingestion listen address {} is not loopback; configure sources with token files to accept remote sources",
                self.listen
            ));
        }
        if self.max_concurrent_commands == 0 {
            return Err("max_concurrent_commands must be at least 1".into());
        }
        if self.command_timeout_secs == 0 {
            return Err("command_timeout_secs must be at least 1".into());
        }
        if let Some(remote) = &self.remote {
            remote.validate()?;
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
        assert!(config.sources.is_empty());
        assert!(config.subscriptions.is_empty());
        config.validate().unwrap();
    }

    #[test]
    fn parses_versioned_configuration_with_subscriptions() {
        let config: HubConfig = serde_yaml::from_str(
            r#"
version: 1
listen: "127.0.0.1:8931"
sources:
  sandbox: { token_file: /run/keys/sessiontap-hub-sandbox }
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
        assert_eq!(
            config.sources["sandbox"].token_file,
            "/run/keys/sessiontap-hub-sandbox"
        );
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

    #[test]
    fn remote_section_validates_addresses() {
        let config = HubConfig::parse(
            "version: 1\nremote:\n  name: MacBook\n  listen: [\"100.64.0.7:8932\", \"[fd00::1]:8932\"]\n  advertise: [\"macbook.tailnet.ts.net:8932\"]\n",
        )
        .unwrap();
        config.validate().unwrap();
        let remote = config.remote.unwrap();
        assert_eq!(remote.listen_mode().addresses().len(), 2);
        assert_eq!(remote.display_name(), "MacBook");
    }

    #[test]
    fn remote_control_defaults_off() {
        let parse = |extra: &str| {
            HubConfig::parse(&format!(
                "version: 1\nremote:\n  listen: [\"127.0.0.1:8932\"]\n{extra}"
            ))
            .unwrap()
            .remote
            .unwrap()
            .control
        };
        assert!(!parse(""));
        assert!(parse("  control: true\n"));
        assert!(!parse("  control: false\n"));
    }

    fn remote(listen: &str) -> HubConfig {
        HubConfig::parse(&format!("version: 1\nremote:\n  listen: {listen}\n")).unwrap()
    }

    #[test]
    fn remote_section_accepts_only_a_lone_wildcard() {
        for listen in ["[\"0.0.0.0:8932\"]", "[\"[::]:8932\"]"] {
            remote(listen).validate().unwrap();
        }
        for (listen, wildcard) in [
            ("[\"0.0.0.0:8932\", \"100.64.0.7:8932\"]", "0.0.0.0:8932"),
            ("[\"100.64.0.7:8932\", \"0.0.0.0:8932\"]", "0.0.0.0:8932"),
            ("[\"0.0.0.0:8932\", \"[::]:8932\"]", "0.0.0.0:8932"),
        ] {
            let error = remote(listen).validate().unwrap_err();
            assert!(error.contains(wildcard), "{error}");
        }
        assert!(
            remote("[]")
                .validate()
                .unwrap_err()
                .contains("at least one")
        );
        let bad = remote("[\"laptop:8932\"]").validate().unwrap_err();
        assert!(bad.contains("laptop:8932"), "{bad}");
        assert!(HubConfig::parse("version: 1\nremote:\n  name: x\n").is_err());
    }

    #[test]
    fn discovery_requires_a_lone_wildcard() {
        let parse = |listen: &str, extra: &str| {
            HubConfig::parse(&format!("version: 1\nremote:\n  listen: {listen}\n{extra}")).unwrap()
        };
        assert!(!parse("[\"0.0.0.0:8932\"]", "").remote.unwrap().discovery);
        for listen in ["[\"0.0.0.0:8932\"]", "[\"[::]:8932\"]"] {
            parse(listen, "  discovery: true\n").validate().unwrap();
        }
        for listen in [
            "[\"192.168.1.20:8932\"]",
            "[\"100.64.0.7:8932\", \"192.168.1.20:8932\"]",
        ] {
            let error = parse(listen, "  discovery: true\n").validate().unwrap_err();
            assert!(error.contains("remote.discovery"), "{error}");
        }
        parse("[\"192.168.1.20:8932\"]", "  discovery: false\n")
            .validate()
            .unwrap();
    }

    #[test]
    fn listen_mode_reports_explicit_and_wildcard() {
        let explicit = remote("[\"100.64.0.7:8932\", \"[fd00::1]:8932\"]");
        assert_eq!(
            explicit.remote.unwrap().listen_mode(),
            ListenMode::Explicit(vec![
                "100.64.0.7:8932".parse().unwrap(),
                "[fd00::1]:8932".parse().unwrap(),
            ])
        );
        for wildcard in ["0.0.0.0:8932", "[::]:8932"] {
            let config = remote(&format!("[\"{wildcard}\"]"));
            let mode = config.remote.unwrap().listen_mode();
            let address: SocketAddr = wildcard.parse().unwrap();
            assert_eq!(mode, ListenMode::Wildcard(address));
            assert_eq!(mode.addresses(), vec![address]);
        }
    }

    #[test]
    fn sources_reject_empty_ids_paths_and_top_level_token_file() {
        let empty_key =
            HubConfig::parse("version: 1\nsources:\n  \"\": { token_file: /k }\n").unwrap();
        assert!(
            empty_key
                .validate()
                .unwrap_err()
                .contains("empty source ID")
        );
        let empty_path =
            HubConfig::parse("version: 1\nsources:\n  host: { token_file: \"\" }\n").unwrap();
        assert!(empty_path.validate().unwrap_err().contains("sources.host"));
        assert!(HubConfig::parse("version: 1\nsources:\n  host: { token: x }\n").is_err());
        let error = HubConfig::parse("version: 1\ntoken_file: /k\n").unwrap_err();
        assert!(error.contains("token_file"), "{error}");
    }

    #[test]
    fn non_loopback_listen_requires_sources() {
        for listen in ["0.0.0.0:8931", "[::]:8931", "192.168.1.5:8931"] {
            let config = HubConfig::parse(&format!("version: 1\nlisten: \"{listen}\"\n")).unwrap();
            let error = config.validate().unwrap_err();
            assert!(error.contains(listen), "{error}");
            let config = HubConfig::parse(&format!(
                "version: 1\nlisten: \"{listen}\"\nsources:\n  host: {{ token_file: /k/host }}\n  sandbox: {{ token_file: /k/sandbox }}\n"
            ))
            .unwrap();
            config.validate().unwrap();
        }
        for listen in ["127.0.0.1:8931", "[::1]:8931"] {
            HubConfig::parse(&format!("version: 1\nlisten: \"{listen}\"\n"))
                .unwrap()
                .validate()
                .unwrap();
        }
    }

    #[test]
    fn remote_section_is_optional() {
        let config = HubConfig::parse("version: 1\n").unwrap();
        config.validate().unwrap();
        assert!(config.remote.is_none());
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
            PublicField::Terminal,
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
                | PublicField::Children
                | PublicField::Terminal => {}
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
