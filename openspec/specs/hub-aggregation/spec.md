# Hub Aggregation

## Purpose

Define canonical multi-source ingestion, persistence, repair, and live observation for the SessionTap hub.

## Requirements

### Requirement: Hub ingests only canonical SessionTap envelopes
The hub SHALL accept canonical source snapshot and update envelopes produced by SessionTap sinks. Each envelope SHALL contain complete `PublicAgentView` values and no internal invocation snapshot, internal normalized event, or raw provider payload. The hub SHALL validate the canonical schema and SHALL NOT perform provider-specific normalization or reinterpret provider hooks.

#### Scenario: Canonical update arrives
- **WHEN** a source sends a valid public update containing source identity, delivery identity, changed public field paths, and complete resulting public agent view
- **THEN** the hub stores that public state without applying provider-specific transformations

#### Scenario: Private source state arrives
- **WHEN** an otherwise valid ingestion request contains multiplexer details, credentials, raw hooks, or another unrecognized field outside the public schema
- **THEN** the hub discards the unrecognized field and persists and forwards only its recognized canonical public projection

#### Scenario: Unknown or malformed envelope arrives
- **WHEN** an ingestion request omits or invalidates a required canonical public-envelope field
- **THEN** the hub rejects it without changing persisted state or invoking subscriptions

#### Scenario: Future source adds optional public metadata
- **WHEN** a newer source includes an unrecognized optional public field while all required canonical fields remain valid
- **THEN** the hub accepts the envelope, ignores the unknown field, and does not echo it to listeners or commands

### Requirement: Hub merges stable source identities
Each source SHALL have a configured stable ID and optional display name, and the hub SHALL identify an agent by the pair of source ID and invocation ID.

#### Scenario: Same invocation identifier appears from two sources
- **WHEN** host and sandbox sources publish the same invocation ID
- **THEN** the hub retains two distinct agents keyed by their respective source IDs

### Requirement: Hub persists the merged current state
The hub SHALL persist source metadata, current source revision, canonical `PublicAgentView` values, accepted delivery identities, and a monotonically increasing hub revision in SQLite. It SHALL NOT persist source-internal invocation snapshots or provider hook payloads.

#### Scenario: Hub restarts
- **WHEN** the hub restarts after accepting source state
- **THEN** it restores the merged public agent view before accepting consumers or subsequent updates

### Requirement: Hub applies delivery idempotently
The hub SHALL accept sink delivery with at-least-once semantics and SHALL apply each `(source_id, event_id)` update at most once to state, live output, and subscription matching.

#### Scenario: Acknowledgement is lost
- **WHEN** a daemon retries an update that the hub already committed
- **THEN** the hub acknowledges the retry without changing state, incrementing its revision, or invoking scripts again

#### Scenario: Stale source revision arrives
- **WHEN** an otherwise valid snapshot or update is older than the source revision already materialized by the hub
- **THEN** the hub does not replace newer state

### Requirement: Source snapshots repair hub state
The hub SHALL transactionally replace the materialized invocation set for one source from a complete snapshot while preserving agents belonging to other sources.

#### Scenario: Newly deployed hub receives a snapshot
- **WHEN** a source with already-running agents establishes delivery to an empty hub
- **THEN** the hub materializes every invocation in the source snapshot before applying later revisions

#### Scenario: Snapshot omits a previously retained active agent
- **WHEN** a newer complete snapshot for a source no longer contains an invocation previously retained as active for that source
- **THEN** the hub removes or marks that stale materialized invocation according to the snapshot replacement semantics

### Requirement: Hub provides gap-free merged live observation
The hub SHALL provide a local command that emits one persisted merged public snapshot followed by one JSON object per accepted public update after the snapshot revision without a subscription gap. Agents SHALL be identified by source ID and invocation ID.

#### Scenario: Status-bar listener starts
- **WHEN** a consumer runs `sessiontap-hub listen`
- **THEN** it receives complete public views for current agents from all sources and then receives live normalized public updates without polling

#### Scenario: Listener reconnects
- **WHEN** a live consumer reconnects after the hub or consumer restarts
- **THEN** it receives a new complete persisted public baseline before subsequent updates

#### Scenario: Multiple fields change in one source update
- **WHEN** one accepted update changes multiple projected fields
- **THEN** the hub listener receives the complete view and the full deterministic changed-field set from that update

### Requirement: Hub retains explicit current attention state
The hub SHALL retain only the optional bounded public status reason carried inside each complete `PublicAgentView`. It SHALL replace or clear the prior reason whenever an accepted complete view replaces the materialized agent state.

#### Scenario: Agent stops waiting for input
- **WHEN** an update changes an agent from blocked to running and its complete public view has no reason
- **THEN** the merged state and live update no longer expose the prior blocked reason

#### Scenario: Approval changes to ordinary input
- **WHEN** a blocked public view with an approval reason is replaced by a blocked view with an input reason
- **THEN** the hub retains only the input reason from the latest complete view

### Requirement: Hub control remains unavailable
The initial hub SHALL NOT expose agent screen inspection, capture, input, or command-control operations, while source envelopes SHALL remain versioned and MAY advertise capabilities for a future separately specified bidirectional transport.

#### Scenario: Consumer requests agent control
- **WHEN** a client attempts to send input or inspect an agent through the initial hub
- **THEN** the hub exposes no such operation
