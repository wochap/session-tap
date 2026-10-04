//! Device scopes: the fixed set, its order, and its implications.

use std::fmt;

/// One device permission. Declaration order is the canonical order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Scope {
    /// Observe merged hub state (`listen`).
    Read,
    /// Change hub state, such as `forget`.
    Manage,
    /// View an agent's live terminal, read-only.
    Watch,
    /// Send input to an agent's live terminal.
    Control,
}

impl Scope {
    pub const ALL: [Self; 4] = [Self::Read, Self::Manage, Self::Watch, Self::Control];
    /// Scopes granted when a pairing names none.
    pub const DEFAULT: [Self; 2] = [Self::Read, Self::Manage];

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Manage => "manage",
            Self::Watch => "watch",
            Self::Control => "control",
        }
    }

    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|scope| scope.as_str() == name)
    }

    /// Terminal scopes are only effective while `remote.control` is on.
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Watch | Self::Control)
    }

    /// Adds implied scopes (`control` adds `watch`, `watch` adds `read`),
    /// then sorts into canonical order without duplicates.
    #[must_use]
    pub fn expand(scopes: &[Self]) -> Vec<Self> {
        let mut out = scopes.to_vec();
        if out.contains(&Self::Control) {
            out.push(Self::Watch);
        }
        if out.contains(&Self::Watch) {
            out.push(Self::Read);
        }
        out.sort_unstable();
        out.dedup();
        out
    }

    /// Validates a pairing request. An empty request means the default set.
    pub fn parse_request(requested: &[String]) -> Result<Vec<Self>, String> {
        if requested.is_empty() {
            return Ok(Self::DEFAULT.to_vec());
        }
        let mut scopes = Vec::with_capacity(requested.len());
        for name in requested {
            let Some(scope) = Self::parse(name) else {
                return Err(format!(
                    "unknown scope '{name}' (expected {})",
                    Self::valid_names()
                ));
            };
            scopes.push(scope);
        }
        Ok(Self::expand(&scopes))
    }

    /// Parses stored comma-joined text, dropping unknown names.
    #[must_use]
    pub fn parse_stored(text: &str) -> Vec<Self> {
        let mut scopes: Vec<Self> = text.split(',').filter_map(Self::parse).collect();
        scopes.sort_unstable();
        scopes.dedup();
        scopes
    }

    #[must_use]
    pub fn names(scopes: &[Self]) -> Vec<String> {
        scopes
            .iter()
            .map(|scope| scope.as_str().to_owned())
            .collect()
    }

    /// `read, manage, watch, or control`
    #[must_use]
    pub fn valid_names() -> String {
        let names: Vec<&str> = Self::ALL.iter().map(|scope| scope.as_str()).collect();
        let (last, rest) = names.split_last().expect("scope set is not empty");
        format!("{}, or {last}", rest.join(", "))
    }
}

impl fmt::Display for Scope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(names: &[&str]) -> Result<Vec<String>, String> {
        let names: Vec<String> = names.iter().map(|name| (*name).to_owned()).collect();
        Scope::parse_request(&names).map(|scopes| Scope::names(&scopes))
    }

    #[test]
    fn default_is_read_and_manage() {
        assert_eq!(request(&[]).unwrap(), ["read", "manage"]);
    }

    #[test]
    fn control_implies_watch_and_read() {
        assert_eq!(request(&["control"]).unwrap(), ["read", "watch", "control"]);
        assert_eq!(request(&["watch"]).unwrap(), ["read", "watch"]);
        assert_eq!(
            request(&["control", "manage", "control"]).unwrap(),
            ["read", "manage", "watch", "control"]
        );
    }

    #[test]
    fn read_alone_stays_read() {
        assert_eq!(request(&["read"]).unwrap(), ["read"]);
    }

    #[test]
    fn unknown_name_lists_all_four() {
        let error = request(&["write"]).unwrap_err();
        assert!(error.contains("'write'"), "{error}");
        for name in ["read", "manage", "watch", "control"] {
            assert!(error.contains(name), "{error}");
        }
    }

    #[test]
    fn stored_text_drops_unknown_names() {
        assert_eq!(
            Scope::parse_stored("control,write,read,watch"),
            [Scope::Read, Scope::Watch, Scope::Control]
        );
        assert!(Scope::parse_stored("").is_empty());
    }
}
