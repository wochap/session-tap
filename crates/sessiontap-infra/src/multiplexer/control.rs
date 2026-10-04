//! Pure parser for the tmux control-mode protocol.

/// A notification line received outside a command reply block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Notification {
    Output { pane: String, data: Vec<u8> },
    Pause { pane: String },
    PaneModeChanged { pane: String },
    LayoutChange { window: String },
    SessionChanged { session: String },
    WindowClose { window: String },
    UnlinkedWindowClose { window: String },
    Exit,
    Other,
}

/// One complete protocol unit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    Notification(Notification),
    /// A command reply. `client` is true for commands this client wrote
    /// (flags bit 1); the reply to the initial attach is not.
    Reply {
        client: bool,
        ok: bool,
        lines: Vec<Vec<u8>>,
    },
}

#[derive(Debug, Default)]
pub struct Parser {
    block: Option<(String, bool, Vec<Vec<u8>>)>,
}

impl Parser {
    /// Feeds one line without its trailing newline.
    pub fn feed(&mut self, line: &[u8]) -> Option<Message> {
        if let Some((number, client, lines)) = &mut self.block {
            if let Some((kind, guard)) = block_guard(line) {
                if kind != "%begin" && guard.0 == *number {
                    let ok = kind == "%end";
                    let client = *client;
                    let lines = std::mem::take(lines);
                    self.block = None;
                    return Some(Message::Reply { client, ok, lines });
                }
            }
            lines.push(line.to_vec());
            return None;
        }
        if let Some(("%begin", (number, flags))) = block_guard(line) {
            self.block = Some((number, flags & 1 == 1, Vec::new()));
            return None;
        }
        Some(Message::Notification(parse_notification(line)))
    }
}

/// `%begin|%end|%error <time> <number> <flags>` as `(kind, (number, flags))`.
fn block_guard(line: &[u8]) -> Option<(&str, (String, u32))> {
    let text = std::str::from_utf8(line).ok()?;
    let mut parts = text.split(' ');
    let kind = parts.next()?;
    if !matches!(kind, "%begin" | "%end" | "%error") {
        return None;
    }
    let _time: u64 = parts.next()?.parse().ok()?;
    let number = parts.next()?;
    number.parse::<u64>().ok()?;
    let flags = parts.next()?.parse().ok()?;
    parts
        .next()
        .is_none()
        .then(|| (kind, (number.to_owned(), flags)))
}

fn parse_notification(line: &[u8]) -> Notification {
    let (head, rest) = split_word(line);
    let (arg, tail) = split_word(rest);
    let arg = || String::from_utf8_lossy(arg).into_owned();
    match head {
        b"%output" => Notification::Output {
            pane: arg(),
            data: decode_octal(tail),
        },
        b"%extended-output" => {
            // `%extended-output <pane> <age> ... : <data>`
            let data = tail
                .windows(3)
                .position(|w| w == b" : ")
                .map(|at| &tail[at + 3..])
                .or_else(|| tail.strip_prefix(b": "))
                .unwrap_or_default();
            Notification::Output {
                pane: arg(),
                data: decode_octal(data),
            }
        }
        b"%pause" => Notification::Pause { pane: arg() },
        b"%pane-mode-changed" => Notification::PaneModeChanged { pane: arg() },
        b"%layout-change" => Notification::LayoutChange { window: arg() },
        b"%session-changed" => Notification::SessionChanged { session: arg() },
        b"%window-close" => Notification::WindowClose { window: arg() },
        b"%unlinked-window-close" => Notification::UnlinkedWindowClose { window: arg() },
        b"%exit" => Notification::Exit,
        _ => Notification::Other,
    }
}

fn split_word(line: &[u8]) -> (&[u8], &[u8]) {
    match line.iter().position(|b| *b == b' ') {
        Some(at) => (&line[..at], &line[at + 1..]),
        None => (line, &[]),
    }
}

/// Decodes tmux's `\ooo` octal escapes back to raw bytes.
#[must_use]
pub fn decode_octal(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut i = 0;
    while i < data.len() {
        if data[i] == b'\\'
            && data.len() >= i + 4
            && data[i + 1..i + 4].iter().all(|b| (b'0'..=b'7').contains(b))
        {
            let value = data[i + 1..i + 4]
                .iter()
                .fold(0u32, |acc, b| acc * 8 + u32::from(b - b'0'));
            out.push(u8::try_from(value & 0xff).unwrap_or_default());
            i += 4;
        } else {
            out.push(data[i]);
            i += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed_all(transcript: &str) -> Vec<Message> {
        let mut parser = Parser::default();
        transcript
            .lines()
            .filter_map(|line| parser.feed(line.as_bytes()))
            .collect()
    }

    #[test]
    fn decodes_octal_utf8_backslash_and_control_bytes() {
        assert_eq!(
            decode_octal(br"hi\015\012\033[1m\134x\303\251"),
            b"hi\r\n\x1b[1m\\x\xc3\xa9".to_vec()
        );
        assert_eq!(decode_octal(br"tail\01"), br"tail\01".to_vec());
        assert_eq!(
            String::from_utf8(decode_octal("é".as_bytes())).unwrap(),
            "é"
        );
    }

    #[test]
    fn recorded_transcript_parses() {
        let messages = feed_all(concat!(
            "%begin 1791134021 583 0\n",
            "%end 1791134021 583 0\n",
            "%session-changed $0 t\n",
            "%begin 1791134021 589 1\n",
            "\x1b[1mbold\n",
            "%end 1 2 1\n",
            "%end 1791134021 589 1\n",
            "%begin 1791134021 590 1\n",
            "can't find pane: %9\n",
            "%error 1791134021 590 1\n",
            "%output %0 a\\134b\\015\\012\n",
            "%extended-output %0 0 : \\033[?2004hsh-5.3$ \n",
            "%pause %0\n",
            "%pane-mode-changed %0\n",
            "%layout-change @0 b25d,80x24,0,0,0 b25d,80x24,0,0,0 *\n",
            "%window-close @1\n",
            "%unlinked-window-close @2\n",
            "%window-renamed @0 tmp\n",
            "%exit\n",
        ));
        assert_eq!(
            messages,
            vec![
                Message::Reply {
                    client: false,
                    ok: true,
                    lines: vec![]
                },
                Message::Notification(Notification::SessionChanged {
                    session: "$0".into()
                }),
                Message::Reply {
                    client: true,
                    ok: true,
                    lines: vec![b"\x1b[1mbold".to_vec(), b"%end 1 2 1".to_vec()]
                },
                Message::Reply {
                    client: true,
                    ok: false,
                    lines: vec![b"can't find pane: %9".to_vec()]
                },
                Message::Notification(Notification::Output {
                    pane: "%0".into(),
                    data: b"a\\b\r\n".to_vec()
                }),
                Message::Notification(Notification::Output {
                    pane: "%0".into(),
                    data: b"\x1b[?2004hsh-5.3$ ".to_vec()
                }),
                Message::Notification(Notification::Pause { pane: "%0".into() }),
                Message::Notification(Notification::PaneModeChanged { pane: "%0".into() }),
                Message::Notification(Notification::LayoutChange {
                    window: "@0".into()
                }),
                Message::Notification(Notification::WindowClose {
                    window: "@1".into()
                }),
                Message::Notification(Notification::UnlinkedWindowClose {
                    window: "@2".into()
                }),
                Message::Notification(Notification::Other),
                Message::Notification(Notification::Exit),
            ]
        );
    }
}
