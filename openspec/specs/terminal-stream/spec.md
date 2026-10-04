# Terminal Stream

## Purpose

Lets a trusted local consumer watch a tracked agent's live terminal and send it keys and text through the daemon, as the source-side foundation for remote terminal access.

## Requirements

### Requirement: Terminal is available only for interactive agents in a multiplexer
A live terminal SHALL be available for an invocation only when it was launched interactively (its standard input was a terminal), it runs inside a multiplexer that has a registered adapter, and its status is not `stopped`. A watch or input request for any other invocation SHALL fail with error code `terminal_unavailable`, and a request naming an unknown invocation SHALL fail with `not_found`. Both SHALL fail without running any multiplexer command.

#### Scenario: Headless launch
- **WHEN** a consumer asks to watch an invocation whose standard input was not a terminal
- **THEN** the daemon answers `terminal_unavailable`

#### Scenario: Agent outside tmux
- **WHEN** a consumer asks to watch an invocation with no multiplexer metadata
- **THEN** the daemon answers `terminal_unavailable` and runs no multiplexer command

### Requirement: Watching starts with a complete snapshot
A watch request on the daemon socket SHALL be answered first with a snapshot message. The snapshot SHALL hold the pane's last 500 lines of scrollback and its visible screen as raw terminal bytes with colour and attribute sequences, the cursor column, row, and visibility, whether the alternate screen is active, the pane width and height in cells, and the current input availability. Every following output message SHALL carry only bytes the pane produced after the snapshot was taken, with none dropped and none repeated.

#### Scenario: Agent prints while the snapshot is taken
- **WHEN** the agent writes output continuously while a consumer starts watching
- **THEN** each byte appears either in the snapshot or in exactly one later output message

#### Scenario: Full-screen TUI
- **WHEN** a consumer watches an agent that uses the alternate screen
- **THEN** the snapshot reports the alternate screen as active and holds the current screen contents and cursor position

### Requirement: Output streams raw pane bytes without blocking the agent
After the snapshot, the daemon SHALL forward the pane's output as raw bytes in order. A pane that is not the watched pane, even in the same multiplexer session, SHALL NOT contribute output. A slow or stalled watcher SHALL NOT slow down or block the agent or other watchers: the daemon SHALL keep reading the multiplexer's output, and a watcher that falls behind SHALL be sent a fresh snapshot instead of the missed output. When the multiplexer itself drops output for the pane, every watcher SHALL be sent a fresh snapshot. A change of the pane's size SHALL also be delivered as a fresh snapshot with the new size.

#### Scenario: Watcher stops reading
- **WHEN** one watcher stops reading its connection while the agent keeps printing
- **THEN** the agent keeps running, other watchers keep receiving output, and the stalled watcher receives a fresh snapshot once it reads again

#### Scenario: Other pane in the same session prints
- **WHEN** another pane in the agent's tmux session produces output
- **THEN** no watcher of the agent receives that output

#### Scenario: Desktop resizes the pane
- **WHEN** the pane is resized on the desktop while a consumer watches
- **THEN** the consumer receives a fresh snapshot reporting the new width and height

### Requirement: Watching never changes the desktop view
Watching SHALL NOT resize the pane or its window, SHALL NOT switch the desktop client's session, window, or pane, and SHALL NOT write anything to the pane. The daemon SHALL keep at most one multiplexer connection per watched pane, shared by all of its watchers, and SHALL close it when the last watcher of that pane disconnects.

#### Scenario: Phone-sized consumer watches
- **WHEN** a consumer watches a 160-column pane
- **THEN** the pane stays 160 columns wide and the desktop view is unchanged

#### Scenario: Last watcher leaves
- **WHEN** the last watcher of a pane disconnects
- **THEN** the daemon closes its multiplexer connection for that pane

### Requirement: Input is accepted only while the agent is in front
An input request SHALL be delivered only when, at the time of the request, the foreground process group of the pane's terminal is the agent's own process group, that group still contains the tracked agent process with its recorded start identity, and the pane is not in a scroll or copy mode. Otherwise the daemon SHALL refuse it without writing to the pane, with error code `not_foreground` or `pane_in_mode`. Input for an invocation whose stream has ended SHALL be refused with `terminal_ended`. This guard SHALL be a best-effort protection against accidentally typing into the desktop shell; the daemon SHALL NOT filter individual keys or pasted text.

#### Scenario: Agent suspended on the desktop
- **WHEN** the agent was suspended with Ctrl+Z and the shell is in the foreground of the pane
- **THEN** input requests fail with `not_foreground` and nothing reaches the shell

#### Scenario: Desktop user is scrolling the pane
- **WHEN** the pane is in tmux copy mode
- **THEN** input requests fail with `pane_in_mode` and the copy mode is left untouched

#### Scenario: Agent back in front
- **WHEN** the suspended agent is resumed with `fg`
- **THEN** input requests are delivered again

### Requirement: Input availability changes are pushed to watchers
While a pane is watched, the daemon SHALL push an input-state message whenever input availability changes, carrying whether input is available and, when not, the reason `not_foreground` or `pane_in_mode`. Output SHALL keep streaming while input is unavailable.

#### Scenario: Agent suspended while watched
- **WHEN** the agent is suspended while a consumer watches
- **THEN** the consumer receives an input-state message with reason `not_foreground` and keeps receiving the pane's output

### Requirement: Input supports named keys, typed characters, and pasted text
An input request SHALL carry exactly one of: a sequence of keys, or a text paste. A key SHALL be either one of the named keys `up`, `down`, `left`, `right`, `escape`, `tab`, `back_tab`, `enter`, `space`, `backspace`, `ctrl_c`, or exactly one printable character typed as a keystroke. Named keys SHALL be encoded for the pane's current keyboard modes. A text paste SHALL be delivered literally, as a bracketed paste when the application enabled bracketed paste, and SHALL be followed by an Enter key only when the request asks for it. A request with an unknown key name, an empty key sequence, or empty text SHALL fail with `bad_request` without writing to the pane.

#### Scenario: Answer an approval menu by digit
- **WHEN** a consumer sends the key `1` while the agent shows a numbered approval menu
- **THEN** the agent receives the keystroke `1`

#### Scenario: Arrow keys in application cursor mode
- **WHEN** the agent enabled application cursor keys and a consumer sends `up`
- **THEN** the pane receives the application-mode encoding of the Up key

#### Scenario: Reply with Enter
- **WHEN** a consumer pastes multi-line text and asks for Enter
- **THEN** the agent receives the text literally, followed by one Enter key

### Requirement: Streams end with a reason
A stream SHALL end with exactly one ended message, after which the daemon closes it and forwards no further output. The reason SHALL be `agent_exited` when the tracked agent's process exits, `pane_closed` when the pane no longer exists, `session_closed` when the pane's multiplexer session is destroyed or the connection is moved to another session, `multiplexer_stopped` when the multiplexer server goes away, or `identity_changed` when revalidation finds the pane no longer belongs to the tracked agent. After the agent exits, no output the pane produces later SHALL be forwarded.

#### Scenario: Agent exits to the shell
- **WHEN** the agent exits and the shell prints its prompt in the pane
- **THEN** watchers receive `ended` with reason `agent_exited` and do not receive the shell prompt

#### Scenario: Desktop kills the tmux server
- **WHEN** the tmux server hosting the pane is killed
- **THEN** watchers receive `ended` with reason `multiplexer_stopped`

### Requirement: Terminal CLI exercises the stream
`sessiontap terminal watch <invocation>` SHALL print each stream message as one JSON line, with output bytes base64-encoded, and SHALL exit successfully after printing the ended message. `sessiontap terminal send <invocation>` SHALL send either `--key <key>` (repeatable, in order) or `--text <text>` with optional `--enter`, SHALL exit successfully when delivered, and SHALL exit non-zero printing the daemon's error code when refused. An invocation SHALL be accepted by full ID or unique ID prefix.

#### Scenario: Send refused while suspended
- **WHEN** the user runs `sessiontap terminal send 7f3c --key enter` while the agent is suspended
- **THEN** the command prints `not_foreground` and exits non-zero

#### Scenario: Watch until exit
- **WHEN** the user runs `sessiontap terminal watch 7f3c` and the agent later exits
- **THEN** the command prints a snapshot line, output lines, and a final ended line with reason `agent_exited`, then exits successfully
