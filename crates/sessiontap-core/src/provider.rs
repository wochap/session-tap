use serde::{Deserialize, Serialize};
use std::{fmt, str::FromStr};

/// Identity of a built-in adapter dialect. Configured provider names and
/// aliases stay free-form strings; this type names only the behavior they
/// inherit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderId {
    Claude,
    Codex,
    Qwen,
    Pi,
}

impl ProviderId {
    /// Every built-in identity in the order the CLI documents them.
    pub const ALL: &'static [Self] = &[Self::Claude, Self::Codex, Self::Pi, Self::Qwen];

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Qwen => "qwen",
            Self::Pi => "pi",
        }
    }

    /// Accepted identities joined with `separator`, for diagnostics and usage.
    #[must_use]
    pub fn joined(separator: &str) -> String {
        Self::ALL
            .iter()
            .map(|id| id.as_str())
            .collect::<Vec<_>>()
            .join(separator)
    }
}

impl fmt::Display for ProviderId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownProvider(pub String);

impl fmt::Display for UnknownProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "unknown provider '{}'; expected one of {}",
            self.0,
            ProviderId::joined(", ")
        )
    }
}

impl std::error::Error for UnknownProvider {}

impl FromStr for ProviderId {
    type Err = UnknownProvider;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .iter()
            .copied()
            .find(|id| id.as_str() == value)
            .ok_or_else(|| UnknownProvider(value.to_owned()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identities_round_trip_through_text_and_serde() {
        for id in ProviderId::ALL {
            assert_eq!(id.as_str().parse::<ProviderId>().unwrap(), *id);
            assert_eq!(id.to_string(), id.as_str());
            let encoded = serde_json::to_string(id).unwrap();
            assert_eq!(encoded, format!("\"{}\"", id.as_str()));
            assert_eq!(serde_json::from_str::<ProviderId>(&encoded).unwrap(), *id);
        }
        assert_eq!(ProviderId::ALL.len(), 4);
    }

    #[test]
    fn unknown_names_are_rejected_with_accepted_values() {
        for name in ["", "Claude", "gemini", "claude "] {
            let error = name.parse::<ProviderId>().unwrap_err();
            assert_eq!(error.0, name);
            assert!(error.to_string().contains("claude, codex, pi, qwen"));
        }
        assert!(serde_json::from_str::<ProviderId>("\"gemini\"").is_err());
    }
}
