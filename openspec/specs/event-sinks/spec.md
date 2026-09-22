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
The broker SHALL send hub sinks canonical source snapshot and update envelopes containing stable source and delivery identities, revision, deterministic changed public field paths, and complete resulting `PublicAgentView` values. SessionTap SHALL exclude internal invocation snapshots, lifecycle/activity/event enums, process and multiplexer control metadata, raw hook bodies, transcripts, complete prompt and assistant text, unselected tool inputs and responses, credentials, and arbitrary provider payloads from every sink. Explicitly selected bounded current status summaries SHALL be public fields eligible for configured sink delivery. When a stdout or HTTP sink configures a non-empty `fields` list, the delivered view SHALL contain only those public fields plus the invocation identity; an empty list delivers the complete public view. Hub sinks SHALL ignore field selection.

#### Scenario: Default HTTP archival sink event
- **WHEN** a meaningful projected state change is queued for a non-hub HTTP sink
- **THEN** its payload contains only configured public agent fields and no internal or raw provider data

#### Scenario: Field selection is configured
- **WHEN** a stdout or HTTP sink configures `fields = ["status", "usage"]`
- **THEN** each delivered payload's view contains the invocation identity, `status`, and `usage` only, and the changed-field list is filtered to the same set

#### Scenario: Field selection names an unknown field
- **WHEN** a sink's `fields` list contains a name that is not a public field
- **THEN** configuration validation fails with a diagnostic naming the sink and the unknown field

#### Scenario: Hub sink has field selection
- **WHEN** field selection is present on a hub sink definition
- **THEN** configuration validation rejects it, because hub envelopes are always complete

#### Scenario: Waiting attention is queued for a hub sink
- **WHEN** internal state changes to waiting approval or waiting input
- **THEN** the hub update contains a blocked public view with the corresponding optional bounded public reason

#### Scenario: Completion is queued for a hub sink
- **WHEN** a root provider stop changes a live invocation to stopped
- **THEN** the hub update contains the stopped public view and its optional completed reason

#### Scenario: Failure event is queued for a hub sink
- **WHEN** a documented root failure-stop event leaves the provider process alive
- **THEN** the hub update contains the resulting stopped public view with an optional allowlisted failed reason and no raw failure message, internal event kind, tool input, prompt, or transcript

#### Scenario: Current reason is cleared
- **WHEN** a later normalized transition changes the invocation to running or idle
- **THEN** the resulting complete public view no longer contains the prior blocked, completed, or failed reason

#### Scenario: Provider metadata changes
- **WHEN** model, effort, permission mode, current turn, provider session, repository, or verified usage changes meaningfully
- **THEN** the hub update contains every corresponding changed public field path, the complete resulting public view, and no private state or raw provider field

#### Scenario: Usage is unavailable
- **WHEN** a provider exposes no verified token or context-utilization fields
- **THEN** the public view leaves usage absent or partially populated rather than reporting estimated values

### Requirement: HTTP delivery is durable and deduplicable
The broker SHALL enqueue HTTP and hub sink deliveries in the same transaction as each meaningful committed public-view transition, SHALL retry transient failures with bounded exponential backoff, and SHALL include a stable source-scoped delivery ID that permits receiver idempotency. Registration, normalized hook changes, lifecycle exit, reconciliation, and future normalized enrichment SHALL be sink-visible only when they change projected public state. Each HTTP and hub delivery SHALL use that sink's configured `timeout_ms`. A hub conflict response SHALL be interpreted by its structured error code: `snapshot_required` resets the sink's baseline and retries the update after the snapshot; any other conflict or client error is a permanent rejection subject to the bounded drop policy. The outbox poll interval, outbox batch size, artifact-collection worker limit, stale-working sweep interval, and update broadcast capacity SHALL be configurable in a `[daemon]` configuration section whose defaults are 250 ms, 100 records, 4 workers, 60 seconds, and 1024 updates.

#### Scenario: Receiver is temporarily unavailable
- **WHEN** HTTP or hub delivery fails with a transient network or server error
- **THEN** the outbox retains it and retries without blocking provider execution or local observation

#### Scenario: Receiver accepted an update but acknowledgement was lost
- **WHEN** SessionTap retries an already accepted update with the same source and delivery ID
- **THEN** the receiver can acknowledge it without applying state or triggering consumers twice

#### Scenario: Invocation exits without a final provider hook
- **WHEN** the wrapper records a lifecycle exit that changes the projected public view
- **THEN** the broker transactionally queues the resulting stopped view without a completed or failed reason for every enabled hub sink

#### Scenario: Invocation exits after a reported stop
- **WHEN** lifecycle exit follows an already projected stopped view with the same completed or failed reason and changes no public field
- **THEN** the broker records lifecycle internally without enqueueing a redundant public update

#### Scenario: Internal-only metadata changes
- **WHEN** credentials, reducer bookkeeping, process control data, or multiplexer data changes without changing `PublicAgentView`
- **THEN** the broker does not enqueue a public sink update

#### Scenario: Several public fields change together
- **WHEN** one committed transition changes status, reason, session, and usage
- **THEN** one canonical update contains all four changed field paths and the complete resulting public view

#### Scenario: Sink timeout is configured
- **WHEN** an HTTP or hub sink sets `timeout_ms = 500` and the receiver does not respond within that time
- **THEN** the delivery is treated as transient and retried with backoff

#### Scenario: Hub responds with a structured snapshot-required conflict
- **WHEN** the hub returns HTTP 409 with error code `snapshot_required`
- **THEN** the broker marks the sink's baseline as due, delivers a source snapshot, and retries the held update afterwards

#### Scenario: Hub responds with another conflict
- **WHEN** the hub returns HTTP 409 whose error code is not `snapshot_required`, or a body that does not parse as the error envelope
- **THEN** the broker treats the delivery as permanently rejected and applies the bounded drop policy without resetting the baseline

#### Scenario: Daemon section is absent
- **WHEN** the configuration file has no `[daemon]` section
- **THEN** the daemon runs with the stated defaults and behaves as before

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
