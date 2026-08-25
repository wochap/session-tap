# Event Sinks

## Purpose

Sanitized local event fan-out and durable remote delivery — configurable stdout and HTTP sinks with safe transport defaults, plus a minimal example receiver demonstrating the protocol.

## Requirements

### Requirement: SessionTap is local-only by default
The broker SHALL send no session event over the network unless the user explicitly enables a sink in SessionTap configuration.

#### Scenario: Fresh installation
- **WHEN** no sink is configured
- **THEN** all snapshots and events remain on the local machine

### Requirement: Sink configuration follows XDG conventions
SessionTap SHALL read a versioned TOML configuration from `$XDG_CONFIG_HOME/sessiontap/config.toml`, falling back to `$HOME/.config/sessiontap/config.toml` when `XDG_CONFIG_HOME` is unset, and SHALL support named stdout and HTTP sinks.

#### Scenario: Environment-referenced credential
- **WHEN** an HTTP sink specifies `token_env`
- **THEN** the broker reads the credential from that environment variable at runtime and does not copy it into status or event records

### Requirement: Forwarded data is sanitized and selectable
The broker SHALL forward only normalized fields allowed by the sink configuration and SHALL exclude raw hook bodies, transcripts, prompt text, tool inputs, and secrets by default.

#### Scenario: Default HTTP sink event
- **WHEN** a normalized state change is queued for a default HTTP sink
- **THEN** its payload contains sanitized session metadata and state but no raw provider payload or transcript content

### Requirement: HTTP delivery is durable and deduplicable
The broker SHALL enqueue sink deliveries in the same transaction as their normalized event, retry transient failures with bounded exponential backoff, and include a stable event ID that permits receiver deduplication.

#### Scenario: Receiver is temporarily unavailable
- **WHEN** an HTTP delivery fails with a transient network or server error
- **THEN** the outbox retains it and retries without blocking provider execution or local observation

#### Scenario: Receiver accepted an event but acknowledgement was lost
- **WHEN** SessionTap retries an already accepted event
- **THEN** the receiver can identify the duplicate by event ID

### Requirement: Network transport defaults are safe
HTTP sinks SHALL require HTTPS except for explicit loopback destinations, SHALL use bounded connection and response timeouts, and SHALL cap queued payload size.

#### Scenario: Insecure remote URL
- **WHEN** configuration specifies plain HTTP to a non-loopback host
- **THEN** SessionTap rejects or disables that sink with a diagnostic

### Requirement: Example receiver demonstrates the protocol
The repository SHALL include a minimal non-production receiver that accepts versioned SessionTap events, deduplicates event IDs for its process lifetime, and prints each newly accepted event as JSON.

#### Scenario: Event is posted twice
- **WHEN** the example receiver receives the same event ID twice
- **THEN** it prints the event once and acknowledges both deliveries safely
