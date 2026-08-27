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
SessionTap SHALL read a versioned TOML configuration from `$XDG_CONFIG_HOME/sessiontap/config.toml`, falling back to `$HOME/.config/sessiontap/config.toml` when `XDG_CONFIG_HOME` is unset, and SHALL support named stdout, HTTP, and hub sinks plus a stable source ID and optional source display name.

#### Scenario: Environment-referenced credential
- **WHEN** an HTTP or hub sink specifies `token_env`
- **THEN** the broker reads the credential from that environment variable at runtime and does not copy it into status or event records

#### Scenario: File-referenced hub credential
- **WHEN** a hub sink specifies `token_file`
- **THEN** the broker reads a private non-symlinked credential file at delivery time and does not copy its contents into delivered state

#### Scenario: Hub sink omits a credential
- **WHEN** a hub sink and receiver are intentionally configured without a token for a local deployment
- **THEN** SessionTap permits unauthenticated delivery subject to the configured network safety policy

### Requirement: Forwarded data is normalized, complete, and selectable
The broker SHALL send hub sinks canonical versioned source snapshot and update envelopes. Updates SHALL contain stable source identity, event identity and revision, normalized event kind and timestamps, optional turn identity, optional categorical failure, the complete resulting normalized invocation snapshot including safe provider metadata and available usage, and the current normalized attention object or explicit null. SessionTap SHALL exclude raw hook bodies, transcripts, prompt text, assistant messages, tool inputs and responses, credentials, and other non-normalized provider payloads from every sink.

#### Scenario: Default HTTP archival sink event
- **WHEN** a normalized state change is queued for a non-hub HTTP sink
- **THEN** its payload contains only the configured sanitized snapshot fields and no raw provider payload or transcript content

#### Scenario: Waiting attention is queued for a hub sink
- **WHEN** a waiting-approval or waiting-input event updates local active attention
- **THEN** the hub update contains the normalized event kind, available turn identity, resulting blocked state, and normalized attention kind, summary, and source

#### Scenario: Attention is cleared
- **WHEN** a later normalized transition clears active attention
- **THEN** the resulting hub update contains explicit null attention so a receiver can remove the prior value

#### Scenario: Failure event is queued for a hub sink
- **WHEN** a failed event contains normalized categorical failure context
- **THEN** the hub update contains that category but no raw failure message, tool input, prompt, or transcript

#### Scenario: Provider metadata changes
- **WHEN** model, effort, permission mode, current turn, provider session, or verified usage changes meaningfully
- **THEN** the hub update contains the resulting canonical snapshot and no raw provider field outside the versioned schema

#### Scenario: Usage is unavailable
- **WHEN** a provider exposes no verified token or context-utilization fields
- **THEN** the hub snapshot leaves usage absent or partially populated rather than reporting estimated values

### Requirement: HTTP delivery is durable and deduplicable
The broker SHALL enqueue HTTP and hub sink deliveries in the same transaction as each meaningful committed public state transition, SHALL retry transient failures with bounded exponential backoff, and SHALL include a stable source-scoped event ID that permits receiver idempotency. Registration, child binding, normalized hook changes, lifecycle exit, and reconciliation SHALL be sink-visible when they change public state.

#### Scenario: Receiver is temporarily unavailable
- **WHEN** HTTP or hub delivery fails with a transient network or server error
- **THEN** the outbox retains it and retries without blocking provider execution or local observation

#### Scenario: Receiver accepted an event but acknowledgement was lost
- **WHEN** SessionTap retries an already accepted update with the same source and event ID
- **THEN** the receiver can acknowledge it without applying state or triggering consumers twice

#### Scenario: Invocation exits without a final provider hook
- **WHEN** the wrapper records a lifecycle exit that changes the public invocation state
- **THEN** the broker transactionally queues the resulting stopped state for every enabled hub sink

### Requirement: Hub sinks repair receiver state with source snapshots
An enabled hub sink SHALL deliver a complete source snapshot at a consistent source revision when delivery is established or repair is required, and SHALL order subsequent updates after that revision.

#### Scenario: Hub sink is newly enabled
- **WHEN** the daemon already retains invocations when a hub sink begins delivery
- **THEN** the hub receives a complete snapshot before later incremental revisions

#### Scenario: State changes during snapshot preparation
- **WHEN** an invocation changes while a source snapshot is being established
- **THEN** the receiver obtains either that change in the snapshot or as an ordered later update without a gap

### Requirement: Source identity is stable and explicit
SessionTap SHALL require a stable source ID for hub delivery, SHALL include it and the optional source display name in every hub envelope, and SHALL keep invocation IDs distinct from source identity.

#### Scenario: Host and sandbox publish to one hub
- **WHEN** independently configured daemons use source IDs `host` and `sandbox`
- **THEN** every received snapshot and update is unambiguously attributable to its originating daemon

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
