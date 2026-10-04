//! Transport-neutral live terminal contract: watch frames, input requests,
//! error codes, and the provider terminal policy.

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

/// Error codes answered by watch and input requests.
pub mod error_code {
    pub const NOT_FOUND: &str = "not_found";
    pub const TERMINAL_UNAVAILABLE: &str = "terminal_unavailable";
    pub const NOT_FOREGROUND: &str = "not_foreground";
    pub const PANE_IN_MODE: &str = "pane_in_mode";
    pub const TERMINAL_ENDED: &str = "terminal_ended";
    pub const UNSUPPORTED_BACKEND: &str = "unsupported_backend";
    pub const BAD_REQUEST: &str = "bad_request";
    /// Relay only: the source's control channel is gone or did not answer.
    pub const SOURCE_UNAVAILABLE: &str = "source_unavailable";
    /// Relay only: the source has not opted in to terminal control.
    pub const SOURCE_DISALLOWS_CONTROL: &str = "source_disallows_control";
}

/// Number of scrollback lines a snapshot carries above the visible screen.
pub const SNAPSHOT_SCROLLBACK_LINES: u32 = 500;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cursor {
    pub x: u16,
    pub y: u16,
    pub visible: bool,
}

/// Why input is currently refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputUnavailable {
    NotForeground,
    PaneInMode,
}

impl InputUnavailable {
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::NotForeground => error_code::NOT_FOREGROUND,
            Self::PaneInMode => error_code::PANE_IN_MODE,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputState {
    pub available: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<InputUnavailable>,
}

impl InputState {
    #[must_use]
    pub const fn from_guard(blocked: Option<InputUnavailable>) -> Self {
        Self {
            available: blocked.is_none(),
            reason: blocked,
        }
    }
}

/// Why a stream ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EndReason {
    AgentExited,
    PaneClosed,
    SessionClosed,
    MultiplexerStopped,
    IdentityChanged,
    /// Relay only: the source's control channel closed or was replaced.
    SourceUnavailable,
    /// Relay only: the source turned `control` off.
    SourceDisallowsControl,
    /// Relay only: the device closed the stream.
    Closed,
}

/// One message of a terminal watch stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TerminalFrame {
    Snapshot {
        seq: u64,
        cols: u16,
        rows: u16,
        cursor: Cursor,
        alternate_screen: bool,
        #[serde(with = "base64_bytes")]
        data: Vec<u8>,
        input: InputState,
    },
    Output {
        seq: u64,
        #[serde(with = "base64_bytes")]
        data: Vec<u8>,
    },
    Input(InputState),
    Ended {
        reason: EndReason,
    },
}

/// Named keys accepted by terminal input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NamedKey {
    Up,
    Down,
    Left,
    Right,
    Escape,
    Tab,
    BackTab,
    Enter,
    Space,
    Backspace,
    CtrlC,
}

impl NamedKey {
    pub const ALL: &'static [Self] = &[
        Self::Up,
        Self::Down,
        Self::Left,
        Self::Right,
        Self::Escape,
        Self::Tab,
        Self::BackTab,
        Self::Enter,
        Self::Space,
        Self::Backspace,
        Self::CtrlC,
    ];

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Up => "up",
            Self::Down => "down",
            Self::Left => "left",
            Self::Right => "right",
            Self::Escape => "escape",
            Self::Tab => "tab",
            Self::BackTab => "back_tab",
            Self::Enter => "enter",
            Self::Space => "space",
            Self::Backspace => "backspace",
            Self::CtrlC => "ctrl_c",
        }
    }
}

/// A named key or exactly one printable character typed as a keystroke.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Named(NamedKey),
    Char(char),
}

impl std::str::FromStr for Key {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if let Some(named) = NamedKey::ALL.iter().find(|key| key.as_str() == value) {
            return Ok(Self::Named(*named));
        }
        let mut chars = value.chars();
        match (chars.next(), chars.next()) {
            (Some(c), None) if !c.is_control() => Ok(Self::Char(c)),
            _ => Err(format!("unknown key '{value}'")),
        }
    }
}

impl std::fmt::Display for Key {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Named(key) => f.write_str(key.as_str()),
            Self::Char(c) => write!(f, "{c}"),
        }
    }
}

impl Serialize for Key {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Key {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(de::Error::custom)
    }
}

/// One input request: a non-empty key sequence, or a non-empty paste.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", try_from = "RawTerminalInput")]
pub enum TerminalInput {
    Keys(Vec<Key>),
    Paste {
        text: String,
        #[serde(default)]
        enter: bool,
    },
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum RawTerminalInput {
    Keys(Vec<Key>),
    Paste {
        text: String,
        #[serde(default)]
        enter: bool,
    },
}

impl TryFrom<RawTerminalInput> for TerminalInput {
    type Error = String;

    fn try_from(raw: RawTerminalInput) -> Result<Self, Self::Error> {
        let input = match raw {
            RawTerminalInput::Keys(keys) => Self::Keys(keys),
            RawTerminalInput::Paste { text, enter } => Self::Paste { text, enter },
        };
        input.validate()?;
        Ok(input)
    }
}

impl TerminalInput {
    /// Rejects an empty key sequence or empty paste text.
    pub fn validate(&self) -> Result<(), String> {
        match self {
            Self::Keys(keys) if keys.is_empty() => Err("empty key sequence".into()),
            Self::Paste { text, .. } if text.is_empty() => Err("empty paste text".into()),
            _ => Ok(()),
        }
    }
}

/// Whether a single digit answers the provider's numbered menus.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuickPick {
    Digits,
    #[default]
    None,
}

/// Provider-specific behaviour of a live terminal, declared by adapters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct TerminalPolicy {
    pub quick_pick: QuickPick,
}

impl TerminalPolicy {
    #[must_use]
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

/// Public descriptor present while a live terminal is available.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalDescriptor {
    pub quick_pick: QuickPick,
}

mod base64_bytes {
    use base64::{Engine, engine::general_purpose::STANDARD};
    use serde::{Deserialize, Deserializer, Serializer, de};

    pub fn serialize<S: Serializer>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&STANDARD.encode(bytes))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<u8>, D::Error> {
        STANDARD
            .decode(String::deserialize(deserializer)?)
            .map_err(de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn round_trip<T>(value: &T, expected: serde_json::Value)
    where
        T: Serialize + for<'de> Deserialize<'de> + PartialEq + std::fmt::Debug,
    {
        assert_eq!(serde_json::to_value(value).unwrap(), expected);
        assert_eq!(&serde_json::from_value::<T>(expected).unwrap(), value);
    }

    #[test]
    fn every_frame_round_trips() {
        round_trip(
            &TerminalFrame::Snapshot {
                seq: 0,
                cols: 160,
                rows: 40,
                cursor: Cursor {
                    x: 3,
                    y: 7,
                    visible: true,
                },
                alternate_screen: true,
                data: b"\x1b[1mhi\xff".to_vec(),
                input: InputState::from_guard(None),
            },
            json!({
                "type": "snapshot", "seq": 0, "cols": 160, "rows": 40,
                "cursor": {"x": 3, "y": 7, "visible": true},
                "alternate_screen": true, "data": "G1sxbWhp/w==",
                "input": {"available": true},
            }),
        );
        round_trip(
            &TerminalFrame::Output {
                seq: 4,
                data: b"ok\n".to_vec(),
            },
            json!({"type": "output", "seq": 4, "data": "b2sK"}),
        );
        round_trip(
            &TerminalFrame::Input(InputState::from_guard(Some(InputUnavailable::PaneInMode))),
            json!({"type": "input", "available": false, "reason": "pane_in_mode"}),
        );
        for (reason, name) in [
            (EndReason::AgentExited, "agent_exited"),
            (EndReason::PaneClosed, "pane_closed"),
            (EndReason::SessionClosed, "session_closed"),
            (EndReason::MultiplexerStopped, "multiplexer_stopped"),
            (EndReason::IdentityChanged, "identity_changed"),
            (EndReason::SourceUnavailable, "source_unavailable"),
            (
                EndReason::SourceDisallowsControl,
                "source_disallows_control",
            ),
            (EndReason::Closed, "closed"),
        ] {
            round_trip(
                &TerminalFrame::Ended { reason },
                json!({"type": "ended", "reason": name}),
            );
        }
    }

    #[test]
    fn keys_round_trip_by_name_and_character() {
        for key in NamedKey::ALL {
            round_trip(&Key::Named(*key), json!(key.as_str()));
        }
        round_trip(&Key::Char('1'), json!("1"));
        round_trip(&Key::Char('E'), json!("E"));
        round_trip(&Key::Char('é'), json!("é"));
        for bad in ["pageup", "", "ab", "\u{7}", "Up"] {
            assert!(serde_json::from_value::<Key>(json!(bad)).is_err(), "{bad}");
        }
    }

    #[test]
    fn input_rejects_empty_and_unknown() {
        round_trip(
            &TerminalInput::Keys(vec![Key::Named(NamedKey::Down), Key::Char('2')]),
            json!({"keys": ["down", "2"]}),
        );
        round_trip(
            &TerminalInput::Paste {
                text: "a\nb".into(),
                enter: true,
            },
            json!({"paste": {"text": "a\nb", "enter": true}}),
        );
        for bad in [
            json!({"keys": []}),
            json!({"keys": ["nope"]}),
            json!({"paste": {"text": ""}}),
            json!({}),
        ] {
            assert!(
                serde_json::from_value::<TerminalInput>(bad.clone()).is_err(),
                "{bad}"
            );
        }
    }

    #[test]
    fn policy_defaults_to_none() {
        assert_eq!(TerminalPolicy::default().quick_pick, QuickPick::None);
        round_trip(
            &TerminalDescriptor {
                quick_pick: QuickPick::Digits,
            },
            json!({"quick_pick": "digits"}),
        );
    }
}
