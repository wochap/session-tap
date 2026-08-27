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
The broker SHALL assign a monotonically increasing broker revision transactionally and SHALL apply adapter-specific session and turn ordering, transition guards, and effective-cause deduplication so delayed hook events cannot regress current metadata, change current attention, resurrect a completed turn, or produce duplicate terminal notification causes incorrectly.

#### Scenario: Late tool event follows turn completion
- **WHEN** a delayed tool activity event arrives after completion for the same identified turn
- **THEN** the reducer retains idle unless the event proves a new turn began

#### Scenario: Event from an older provider session arrives
- **WHEN** a hook carries a provider session ID different from the latest explicitly started provider session
- **THEN** the reducer does not change current activity, attention, turn, usage, or provider metadata and exposes no actionable effective cause

#### Scenario: Older provider session ends after a newer session starts
- **WHEN** provider session B starts and provider session A later emits `SessionEnd`
- **THEN** session B remains current and the invocation lifecycle remains alive

#### Scenario: Current provider session ends before process exit
- **WHEN** the current provider session ends while the supervised child remains alive
- **THEN** provider-session state closes without setting process lifecycle to exited

#### Scenario: Duplicate event delivery
- **WHEN** the broker receives an event with an event ID it already committed
- **THEN** it does not apply or publish the event twice

#### Scenario: Duplicate stop signal has a distinct event ID
- **WHEN** another completed or failed signal arrives for the identified turn already marked terminal
- **THEN** the update does not expose a second completed or failed cause

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
`sessiontap --listen` and `sessiontap listen` SHALL first emit a snapshot envelope with its revision and local active-attention baseline and then emit one JSON object per committed change after that revision without a subscription gap. Each post-baseline update SHALL retain the invocation `snapshot` and include the effective event metadata that caused it.

#### Scenario: Event races listener startup
- **WHEN** a state event commits while a listener is obtaining its initial snapshot
- **THEN** the event appears either in that snapshot baseline or as a following update, but is not omitted

#### Scenario: Listener reconnects
- **WHEN** a listener reconnects after disconnection
- **THEN** it receives a new complete snapshot and active-attention baseline before later updates

#### Scenario: Turn completes
- **WHEN** the reducer commits the first normal terminal event for a turn generation
- **THEN** the update envelope contains `event.kind` equal to `completed` even though the snapshot activity is idle

#### Scenario: Turn fails
- **WHEN** the reducer commits the first documented failure terminal event for a turn generation
- **THEN** the update envelope contains `event.kind` equal to `failed` even though the snapshot activity is idle

#### Scenario: Legacy consumer reads an update
- **WHEN** a consumer ignores unknown listen-envelope fields and reads only the existing `snapshot` field
- **THEN** it continues to receive a valid invocation snapshot

#### Scenario: Listener falls behind
- **WHEN** the daemon replaces missed updates with a fresh snapshot envelope
- **THEN** the replacement contains the current invocation snapshots and active-attention baseline at the reported revision

### Requirement: Listen updates expose effective event causes
Every update emitted by a current daemon SHALL include provider-neutral live event metadata using the existing normalized `EventKind` vocabulary and SHALL distinguish waiting approval, waiting input, completion, failure, session end, work, new turns, and enrichment where those causes are observable.

#### Scenario: Approval blocks an invocation
- **WHEN** the broker commits the first unresolved approval wait in an attention episode
- **THEN** the update event kind is waiting_approval and includes available bounded attention context

#### Scenario: Ordinary input blocks an invocation
- **WHEN** the broker commits a provider question or elicitation distinguishable from approval
- **THEN** the update event kind is waiting_input and includes available bounded attention context

#### Scenario: Process exits
- **WHEN** the supervised provider process exits
- **THEN** the update event kind is session_ended and the snapshot contains the resulting process metadata

#### Scenario: Initial snapshot is observed
- **WHEN** a notification consumer receives the initial or lag-recovery snapshot envelope
- **THEN** it can seed current state without treating that envelope as a newly occurring notification event

### Requirement: Active attention is local durable state
The broker SHALL persist at most one bounded active-attention object per invocation outside `InvocationSnapshot`, SHALL update it transactionally with event reduction, and SHALL expose it only through local listen baselines and updates.

#### Scenario: Broker restarts while an invocation is blocked
- **WHEN** the last committed state for a retained live invocation is waiting_approval or waiting_input
- **THEN** a listener's initial snapshot baseline includes the retained active attention for that invocation

#### Scenario: New attention replaces old attention
- **WHEN** another attention context is committed for an already blocked invocation
- **THEN** the broker retains only the newest bounded active object rather than a history of attention contexts

#### Scenario: Work resumes
- **WHEN** a new turn begins or a reliable provider event shows that work resumed
- **THEN** the broker clears the invocation's active attention in the same transaction

#### Scenario: Turn or invocation terminates
- **WHEN** the invocation completes, fails, exits, or is reconciled as lost
- **THEN** the broker clears its active attention

#### Scenario: Invocation retention expires
- **WHEN** a retained invocation is deleted under the existing retention policy
- **THEN** its local active-attention row is deleted as well

### Requirement: Public invocation snapshots remain notification-context free
`InvocationSnapshot`, status responses, and persisted snapshot JSON SHALL NOT gain raw or bounded commands, questions, assistant messages, or local attention fields as part of this change.

#### Scenario: Invocation is blocked
- **WHEN** status is requested for an invocation with active attention
- **THEN** the invocation snapshot reports blocked activity/status without embedding its attention object

### Requirement: Status payload uses typed structured fields
Each snapshot SHALL contain the invocation ID, provider, executable, sanitized argument array, cwd, process metadata, timestamps, lifecycle, activity, derived status, optional ordered provider session, optional provider metadata containing model, effort, permission mode and current turn ID, optional usage containing token counts and context-window utilization, optional repository metadata, optional multiplexer metadata, and advertised capabilities.

#### Scenario: Enrichment is unavailable
- **WHEN** token usage, context utilization, provider metadata, repository information, session name, or multiplexer information cannot be determined
- **THEN** the snapshot remains valid with the corresponding optional field absent or null

#### Scenario: Provider session changes within one invocation
- **WHEN** an explicit session-start hook introduces a new provider session ID
- **THEN** the snapshot records the new current session with a higher generation and its normalized start reason when available

#### Scenario: Current turn changes
- **WHEN** a new prompt supplies a verified provider turn identifier
- **THEN** the snapshot exposes that identifier as current only within the authoritative provider session

#### Scenario: Repeated metadata is unchanged
- **WHEN** consecutive hooks repeat identical normalized model, effort, permission, turn, session, and usage values without another public transition
- **THEN** the broker does not create a sink-visible metadata-only change
