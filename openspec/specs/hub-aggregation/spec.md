# Hub Aggregation

## Purpose

Define canonical multi-source ingestion, persistence, repair, and live observation for the SessionTap hub.

## Requirements

### Requirement: Hub ingests only canonical SessionTap envelopes
The hub SHALL accept versioned source snapshot and update envelopes produced by SessionTap sinks, SHALL validate their canonical schema, and SHALL NOT perform provider-specific normalization or reinterpret provider hook payloads.

#### Scenario: Canonical update arrives
- **WHEN** a source sends a valid normalized update containing source identity, event metadata, resulting agent state, and attention state
- **THEN** the hub stores that canonical state without applying provider-specific transformations

#### Scenario: Unknown or malformed envelope arrives
- **WHEN** an ingestion request has an unsupported schema version or violates the canonical envelope schema
- **THEN** the hub rejects it without changing persisted state or invoking subscriptions

### Requirement: Hub merges stable source identities
Each source SHALL have a configured stable ID and optional display name, and the hub SHALL identify an agent by the pair of source ID and invocation ID.

#### Scenario: Same invocation identifier appears from two sources
- **WHEN** host and sandbox sources publish the same invocation ID
- **THEN** the hub retains two distinct agents keyed by their respective source IDs

### Requirement: Hub persists the merged current state
The hub SHALL persist source metadata, current source revision, canonical invocation snapshots, accepted update identities, and a monotonically increasing hub revision in SQLite.

#### Scenario: Hub restarts
- **WHEN** the hub restarts after accepting source state
- **THEN** it restores the merged agent view before accepting consumers or subsequent updates

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
The hub SHALL provide a local command that emits one persisted merged snapshot followed by one JSON object per accepted update after the snapshot revision without a subscription gap.

#### Scenario: Quickshell listener starts
- **WHEN** a consumer runs `sessiontap-hub listen`
- **THEN** it receives the current agents from all sources and then receives live normalized updates without periodic polling

#### Scenario: Listener reconnects
- **WHEN** a live consumer reconnects after the hub or consumer restarts
- **THEN** it receives a new complete persisted baseline before subsequent updates

### Requirement: Hub retains explicit current attention state
The hub SHALL persist the complete normalized attention object supplied by SessionTap, including its kind, summary, and source, and SHALL clear it when an accepted agent state contains `attention: null`.

#### Scenario: Agent stops waiting for input
- **WHEN** an update changes an agent from blocked to running and contains `attention: null`
- **THEN** the merged state and live update no longer expose the prior attention information

### Requirement: Hub control remains unavailable
The initial hub SHALL NOT expose agent screen inspection, capture, input, or command-control operations, while source envelopes SHALL remain versioned and MAY advertise capabilities for a future separately specified bidirectional transport.

#### Scenario: Consumer requests agent control
- **WHEN** a client attempts to send input or inspect an agent through the initial hub
- **THEN** the hub exposes no such operation
