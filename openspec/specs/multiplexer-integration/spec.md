# Multiplexer Integration

## Purpose

tmux discovery, stable pane metadata, and validated capture/input control through a backend-neutral multiplexer adapter interface.

## Requirements

### Requirement: Multiplexer support uses a backend-neutral interface
The core SHALL define a multiplexer adapter interface for inspection, pane capture, live pane streaming, and input delivery of named keys, typed characters, and pasted text, without embedding tmux-specific fields in provider adapters. Capture, streaming, and input delivery SHALL select the adapter from the backend recorded in the invocation's multiplexer metadata, and SHALL fail with a typed unsupported-backend error when no adapter exists for that backend. The daemon's request handling SHALL NOT name a specific backend. The recorded backend SHALL serialize as the same lowercase string it does today.

#### Scenario: Future backend implementation
- **WHEN** a Kitty or Zellij adapter is added later
- **THEN** provider launch and normalization code require no provider-specific changes

#### Scenario: Capture dispatches by recorded backend
- **WHEN** a capture request targets an invocation whose multiplexer metadata records backend `tmux`
- **THEN** the tmux adapter performs the capture without the daemon naming tmux in its request handling

#### Scenario: Stream dispatches by recorded backend
- **WHEN** a watch request targets an invocation whose multiplexer metadata records backend `tmux`
- **THEN** the tmux adapter provides the stream without the daemon naming tmux in its request handling

#### Scenario: Recorded backend has no adapter
- **WHEN** an invocation's multiplexer metadata records a backend with no registered adapter
- **THEN** capture, watch, and input requests fail with an unsupported-backend error and no command is executed

### Requirement: Enclosing tmux context is discovered
When launched inside tmux, SessionTap SHALL resolve the exact socket path, server PID, session ID and name, window ID and index, pane ID, pane TTY, and pane process identity when available.

#### Scenario: Custom tmux socket
- **WHEN** the invocation is inside a tmux server started with a non-default socket
- **THEN** SessionTap records the resolved socket path rather than assuming the default server

#### Scenario: No multiplexer
- **WHEN** the invocation is not inside a supported multiplexer
- **THEN** multiplexer metadata is null and multiplexer control capabilities are false

### Requirement: tmux control targets are revalidated
Before capture, before opening a stream, and before each input delivery, the tmux adapter SHALL reconnect through the recorded socket, verify server and pane identity, and verify that the pane still corresponds to the tracked invocation or its process ancestry.

#### Scenario: Pane ID was reused
- **WHEN** a recorded pane ID now refers to an unrelated process after tmux restart or pane replacement
- **THEN** SessionTap refuses capture, streaming, and input rather than controlling the unrelated pane

### Requirement: Input delivery preserves arbitrary text
The tmux adapter SHALL deliver pasted text without shell interpretation, SHALL use a paste-safe mechanism for multiline or special-character content, and SHALL wrap the paste in bracketed-paste markers when the pane's application enabled bracketed paste. Typed characters SHALL be delivered as literal keystrokes, and named keys SHALL be delivered through the multiplexer's key encoding so they match the pane's current keyboard modes.

#### Scenario: Multiline input contains shell syntax
- **WHEN** input contains newlines, quotes, dollar expansion syntax, or terminal key names
- **THEN** the pane receives the literal intended bytes and no intermediate shell evaluates them

#### Scenario: Application requested bracketed paste
- **WHEN** the agent enabled bracketed paste and text is pasted
- **THEN** the agent receives the text between bracketed-paste markers

#### Scenario: Typed key name is not interpreted
- **WHEN** the typed character `E` is sent as a key
- **THEN** the pane receives the single byte `E`, not a key looked up by that name

### Requirement: Remote metadata is not direct authority
Forwarded multiplexer metadata SHALL be descriptive; control requests SHALL identify the invocation and be executed only by the local broker after current validation.

#### Scenario: Stale remote snapshot requests input
- **WHEN** a consumer bases a request on old multiplexer metadata
- **THEN** the local broker revalidates the current invocation and refuses the operation if the target is no longer valid
