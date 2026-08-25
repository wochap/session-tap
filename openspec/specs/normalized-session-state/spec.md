# Normalized Session State

## Purpose

Provider-neutral lifecycle/activity reduction, SQLite persistence through a single-writer broker, versioned status snapshots, and race-free live JSONL observation.

## Requirements

### Requirement: Lifecycle and activity are independent
The broker SHALL store process lifecycle as `starting`, `alive`, `exited`, or `lost` and activity as `unknown`, `idle`, `working`, `waiting_input`, or `waiting_approval`.

#### Scenario: Completed turn in a live TUI
- **WHEN** a provider reports that a turn stopped while its process remains alive
- **THEN** lifecycle remains alive, activity becomes idle, and public status becomes idle

#### Scenario: Approval is required
- **WHEN** a provider reports an unresolved permission request for a live invocation
- **THEN** activity becomes waiting_approval and public status becomes blocked

### Requirement: Public status is derived consistently
The broker SHALL derive exactly one public status using the precedence stopped for exited or lost lifecycle, blocked for either waiting activity, running for working activity, and idle for alive idle activity.

#### Scenario: Process exits while blocked
- **WHEN** a blocked provider process exits
- **THEN** its public status becomes stopped even if its last activity was waiting_approval

### Requirement: Provider events reduce deterministically
The broker SHALL assign a monotonically increasing broker revision transactionally and SHALL apply adapter-specific ordering and transition guards so delayed hook events cannot resurrect a completed turn incorrectly.

#### Scenario: Late tool event follows turn completion
- **WHEN** a delayed tool activity event arrives after the same turn's completion event
- **THEN** the reducer retains idle unless the event proves a new turn began

#### Scenario: Duplicate event delivery
- **WHEN** the broker receives an event with an event ID it already committed
- **THEN** it does not apply or publish the event twice

### Requirement: State is durably persisted in SQLite
The broker SHALL be the only SQLite writer and SHALL transactionally persist invocation snapshots, normalized events, schema migrations, and sink outbox entries using WAL mode.

#### Scenario: Broker restart
- **WHEN** the broker restarts after committed events
- **THEN** it restores retained invocation history while treating previously alive process state as unverified until liveness is reconciled

#### Scenario: Concurrent hook burst
- **WHEN** multiple hooks for one or more invocations arrive concurrently
- **THEN** the broker serializes their commits without corrupting snapshots or revisions

### Requirement: Status command returns a versioned snapshot
`sessiontap --status` and `sessiontap status` SHALL print a JSON array of retained sanitized invocation snapshots using the public schema version.

#### Scenario: No tracked invocations
- **WHEN** no invocation exists within retention
- **THEN** the command prints an empty JSON array and exits successfully

### Requirement: Listen command is race-free JSONL
`sessiontap --listen` and `sessiontap listen` SHALL first emit a snapshot envelope with its revision and then emit one JSON object per committed change after that revision without a subscription gap.

#### Scenario: Event races listener startup
- **WHEN** a state event commits while a listener is obtaining its initial snapshot
- **THEN** the event appears either in that snapshot or as a following update, but is not omitted

#### Scenario: Listener reconnects
- **WHEN** a listener reconnects after disconnection
- **THEN** it receives a new complete snapshot before later updates

### Requirement: Status payload uses typed structured fields
Each snapshot SHALL contain the invocation ID, provider, executable, sanitized argument array, cwd, process metadata, timestamps, lifecycle, activity, derived status, optional provider session, optional usage, optional repository metadata, optional multiplexer metadata, and advertised capabilities.

#### Scenario: Enrichment is unavailable
- **WHEN** token usage, repository information, session name, or multiplexer information cannot be determined
- **THEN** the snapshot remains valid with the corresponding optional field absent or null
