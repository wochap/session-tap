# Multiplexer Integration

## Purpose

tmux discovery, stable pane metadata, and validated capture/input control through a backend-neutral multiplexer adapter interface.

## Requirements

### Requirement: Multiplexer support uses a backend-neutral interface
The core SHALL define a multiplexer adapter interface for inspection, pane capture, and input delivery without embedding tmux-specific fields in provider adapters.

#### Scenario: Future backend implementation
- **WHEN** a Kitty or Zellij adapter is added later
- **THEN** provider launch and normalization code require no provider-specific changes

### Requirement: Enclosing tmux context is discovered
When launched inside tmux, SessionTap SHALL resolve the exact socket path, server PID, session ID and name, window ID and index, pane ID, pane TTY, and pane process identity when available.

#### Scenario: Custom tmux socket
- **WHEN** the invocation is inside a tmux server started with a non-default socket
- **THEN** SessionTap records the resolved socket path rather than assuming the default server

#### Scenario: No multiplexer
- **WHEN** the invocation is not inside a supported multiplexer
- **THEN** multiplexer metadata is null and multiplexer control capabilities are false

### Requirement: tmux control targets are revalidated
Before capture or input, the tmux adapter SHALL reconnect through the recorded socket, verify server and pane identity, and verify that the pane still corresponds to the tracked invocation or its process ancestry.

#### Scenario: Pane ID was reused
- **WHEN** a recorded pane ID now refers to an unrelated process after tmux restart or pane replacement
- **THEN** SessionTap refuses capture and input rather than controlling the unrelated pane

### Requirement: Input delivery preserves arbitrary text
The tmux adapter SHALL deliver text without shell interpretation and SHALL use a paste-safe mechanism for multiline or special-character content.

#### Scenario: Multiline input contains shell syntax
- **WHEN** input contains newlines, quotes, dollar expansion syntax, or terminal key names
- **THEN** the pane receives the literal intended bytes and no intermediate shell evaluates them

### Requirement: Remote metadata is not direct authority
Forwarded multiplexer metadata SHALL be descriptive; control requests SHALL identify the invocation and be executed only by the local broker after current validation.

#### Scenario: Stale remote snapshot requests input
- **WHEN** a consumer bases a request on old multiplexer metadata
- **THEN** the local broker revalidates the current invocation and refuses the operation if the target is no longer valid
