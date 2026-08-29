# Normalized Session State

## Purpose

Provider-neutral lifecycle/activity reduction, SQLite persistence through a single-writer broker, versioned status snapshots, and race-free live JSONL observation.

## Requirements

### Requirement: Lifecycle and activity are independent
The broker SHALL store process lifecycle as `starting`, `alive`, `exited`, or `lost` and activity as `unknown`, `idle`, `working`, `waiting_input`, `waiting_approval`, or `stopped`. Stopped activity SHALL mean that the provider finished or failed a response while the supervised process may remain alive.

#### Scenario: Completed turn in a live TUI
- **WHEN** a provider reports that a turn stopped while its process remains alive
- **THEN** lifecycle remains alive, activity becomes stopped, and public status becomes stopped

#### Scenario: Explicit idle follows a stopped turn
- **WHEN** a provider emits a verified idle signal after reporting turn stop
- **THEN** lifecycle remains alive, activity becomes idle, and public status becomes idle

#### Scenario: Approval is required
- **WHEN** a provider reports an unresolved permission request for a live root invocation
- **THEN** activity becomes waiting_approval and public status becomes blocked

### Requirement: Public status is derived consistently
The broker SHALL derive exactly one public status using the precedence stopped for exited or lost lifecycle, blocked for either waiting activity, stopped for stopped activity on a live process, running for working activity, and idle for alive unknown or idle activity.

#### Scenario: Process exits while blocked
- **WHEN** a blocked provider process exits
- **THEN** its public status becomes stopped and its blocked reason is cleared

#### Scenario: Live turn completes
- **WHEN** a normal completion event changes a live invocation from working or blocked to stopped activity
- **THEN** public status becomes stopped without implying that the process exited

### Requirement: Provider events reduce deterministically
The broker SHALL assign a monotonically increasing broker revision transactionally and SHALL apply adapter-specific session and turn ordering, transition guards, and effective-cause deduplication so delayed hook events cannot regress current metadata, change current status reason, resurrect a stopped turn, or produce duplicate terminal notification causes incorrectly.

#### Scenario: Late tool event follows turn completion
- **WHEN** a delayed tool activity event arrives after completion for the same identified turn
- **THEN** the reducer retains stopped activity unless the event proves a new turn began

#### Scenario: Event from an older provider session arrives
- **WHEN** a hook carries a provider session ID different from the latest explicitly started provider session
- **THEN** the reducer does not change current activity, reason, turn, usage, or provider metadata and exposes no actionable effective cause

#### Scenario: Older provider session ends after a newer session starts
- **WHEN** provider session B starts and provider session A later emits `SessionEnd`
- **THEN** session B remains current and the invocation lifecycle remains alive

#### Scenario: Current provider session ends before process exit
- **WHEN** the current provider session ends while the supervised child remains alive
- **THEN** provider-session state closes without setting process lifecycle to exited or resetting stopped activity to idle

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
`sessiontap --status` and `sessiontap status` SHALL print a JSON array of retained `PublicAgentView` values. The command SHALL project each value explicitly from internal normalized state and SHALL NOT serialize the internal invocation snapshot directly.

#### Scenario: No tracked invocations
- **WHEN** no invocation exists within retention
- **THEN** the command prints an empty JSON array and exits successfully

#### Scenario: Internal control state exists
- **WHEN** a retained invocation contains process and multiplexer data used for local control
- **THEN** status output omits that data while retaining the agent's normalized public state

### Requirement: Listen command is race-free JSONL
`sessiontap --listen` and `sessiontap listen` SHALL first emit a public snapshot envelope with its revision and complete current `PublicAgentView` values, then emit one public update envelope per meaningful projected change after that revision without a subscription gap. Each update SHALL contain the complete resulting public agent view and a deterministic non-empty set of changed public field paths, not internal normalized event metadata.

#### Scenario: Event races listener startup
- **WHEN** normalized state changes while a listener is obtaining its initial snapshot
- **THEN** the resulting public view appears either in that snapshot baseline or as a following update, but is not omitted

#### Scenario: Listener reconnects
- **WHEN** a listener reconnects after disconnection
- **THEN** it receives a new complete public snapshot before later updates

#### Scenario: Turn completes
- **WHEN** the reducer commits the first normal terminal event for a live turn
- **THEN** the update contains a stopped public agent view with an optional completed reason without exposing the internal completed event kind

#### Scenario: Turn fails
- **WHEN** the reducer commits the first documented failure event while the process remains alive
- **THEN** the update contains a stopped public agent view with an optional failed reason rather than a permanent internal failure status

#### Scenario: Listener falls behind
- **WHEN** the daemon replaces missed updates with a fresh snapshot envelope
- **THEN** the replacement contains complete current public agent views at the reported revision

### Requirement: Active attention is local durable state
The broker SHALL persist at most one bounded current status-reason object per invocation outside `InvocationSnapshot`, SHALL update it transactionally with event reduction, and SHALL expose it only through the explicit public reason projection in local views and configured sinks. Selected excerpts SHALL not be stored in normalized event history.

#### Scenario: Broker restarts while an invocation is blocked
- **WHEN** the last committed state for a retained live invocation is waiting_approval or waiting_input
- **THEN** a listener's initial snapshot baseline includes the retained active blocked reason for that invocation

#### Scenario: Broker restarts after completion
- **WHEN** the last committed state is a stopped live invocation with a bounded completed reason
- **THEN** a listener's initial snapshot includes that current stopped reason

#### Scenario: New reason replaces old reason
- **WHEN** a new status reason is committed for an invocation
- **THEN** the broker retains only the newest bounded current object rather than a history of reason excerpts

#### Scenario: Work resumes
- **WHEN** a new turn begins or a reliable provider event shows that work resumed
- **THEN** the broker clears the invocation's current reason in the same transaction

#### Scenario: Explicit idle follows stop
- **WHEN** a verified idle event changes stopped activity to idle
- **THEN** the broker clears the completed or failed reason

#### Scenario: Lifecycle-only exit occurs
- **WHEN** a running, blocked, or idle invocation exits or is reconciled as lost without a provider stop outcome
- **THEN** the broker clears any incompatible reason and projects stopped with no reason

#### Scenario: Process exits after reported completion
- **WHEN** an invocation already projects stopped with a completed or failed reason and its process later exits
- **THEN** the current outcome reason remains and no public update is emitted if no projected field changed

#### Scenario: Invocation retention expires
- **WHEN** a retained invocation is deleted under the existing retention policy
- **THEN** its current status-reason row is deleted as well

### Requirement: Status payload uses typed structured fields
Each `PublicAgentView` SHALL contain public invocation identity, configured provider identity, derived status, timestamps, working directory, optional provider session identity/name/start reason, optional sanitized provider metadata, optional verified usage and context measurements, optional repository metadata, and an optional bounded status-compatible reason. Reason kinds SHALL be `input`, `approval`, `completed`, or `failed`. The public type SHALL NOT contain adapter dialect, credentials, raw hooks, executable arguments, private process identities, multiplexer details, internal lifecycle or activity, internal normalized event kinds, reducer bookkeeping, or control authority. Working directories, repository paths, session names, and bounded reasons SHALL be treated as potentially sensitive observer data intentionally shared with the single operator and configured sinks.

#### Scenario: Enrichment is unavailable
- **WHEN** usage, context utilization, provider metadata, repository information, session name, or status reason cannot be determined
- **THEN** the public view remains valid with the corresponding optional field absent or null

#### Scenario: Provider session changes within one invocation
- **WHEN** an explicit session-start hook introduces a new provider session ID
- **THEN** the public view records the new current session with its normalized name and start reason when available

#### Scenario: Current turn changes
- **WHEN** a new prompt supplies a verified provider turn identifier
- **THEN** the public view may expose that bounded identifier only as explicitly defined safe provider metadata

#### Scenario: Repeated metadata is unchanged
- **WHEN** consecutive hooks repeat identical projected model, effort, permission, turn, session, usage, repository, reason, and status values
- **THEN** the broker does not create a listener- or sink-visible metadata-only change

### Requirement: Public agent status has four values
Every `PublicAgentView` SHALL expose exactly one of `running`, `blocked`, `idle`, or `stopped`. The projection SHALL map working activity on a live process to running, either waiting activity on a live process to blocked, stopped activity on a live process to stopped, unknown or idle activity on a live process to idle, and exited or lost lifecycle to stopped regardless of activity.

#### Scenario: Invocation is starting
- **WHEN** internal lifecycle is starting with any activity
- **THEN** public status is idle

#### Scenario: Live agent is working
- **WHEN** internal lifecycle is alive and activity is working
- **THEN** public status is running

#### Scenario: Live agent awaits ordinary input
- **WHEN** internal lifecycle is alive and activity is waiting_input
- **THEN** public status is blocked with an optional input reason

#### Scenario: Live agent awaits approval
- **WHEN** internal lifecycle is alive and activity is waiting_approval
- **THEN** public status is blocked with an optional approval reason

#### Scenario: Live agent has stopped responding
- **WHEN** internal lifecycle is alive and activity is stopped
- **THEN** public status is stopped with an optional completed or failed reason

#### Scenario: Live agent has unknown activity
- **WHEN** internal lifecycle is alive and activity is unknown
- **THEN** public status is idle

#### Scenario: Blocked process exits
- **WHEN** internal lifecycle becomes exited or lost while activity is waiting_input or waiting_approval
- **THEN** public status is stopped with no blocked reason

### Requirement: Public changed fields are distinct from current status
A public update envelope SHALL contain a deterministic non-empty set of typed public field paths describing every material projected field changed by the transaction, while the embedded view status SHALL describe current agent state. An update SHALL NOT choose one cause when several fields change. Internal activity and normalized event-kind enums SHALL remain internal reduction concepts.

#### Scenario: Metadata changes during work
- **WHEN** verified usage changes while public status remains running
- **THEN** the changed set contains `usage` and the complete resulting view remains running

#### Scenario: Approval begins
- **WHEN** internal reduction changes activity from working to waiting_approval
- **THEN** the changed set contains `status` and every other changed projected field, and the complete resulting view is blocked with an optional approval reason

#### Scenario: Status and session change together
- **WHEN** one transaction changes both the public status and provider session
- **THEN** the changed set contains both `status` and `session` without applying cause precedence

### Requirement: Public projection is an explicit privacy boundary
The daemon SHALL construct public views field-by-field from normalized state and current bounded status reason read at the same committed revision before local serialization or sink enqueueing. It SHALL compare and publish the exact projected view associated with that revision. Raw provider data, complete prompts or assistant messages, transcripts, and internal control metadata SHALL remain inaccessible through status, listen, sink, hub ingestion, and hub listening protocols. Selected bounded status summaries SHALL be intentionally observer-facing and MAY be delivered to sinks explicitly configured by the operator.

#### Scenario: Invocation runs inside tmux
- **WHEN** internal state contains a tmux socket, session, window, pane, TTY, and process identities
- **THEN** none of those fields or values appears in the serialized public agent view

#### Scenario: Hook contains conversational content
- **WHEN** raw hook input contains prompts, assistant text, transcript paths, or arbitrary tool input
- **THEN** public output contains only explicitly selected bounded normalized fields, including at most an eligible 100-character status excerpt, and never the raw content

#### Scenario: Status reason changes while a baseline is read
- **WHEN** the current reason changes concurrently with a status or listen baseline query
- **THEN** each returned public view combines internal invocation state and reason from one consistent committed revision

#### Scenario: Canonical envelope is serialized
- **WHEN** a local snapshot or update is serialized
- **THEN** its shape matches the shared golden public-envelope fixtures used by daemon, sink, hub, and receiver tests

### Requirement: Interrupted turns are terminal without being completed
The broker SHALL reduce `Interrupted` to stopped activity for the current live turn and mark that turn terminal. It SHALL expose public status `stopped` without a completed or failed reason and SHALL NOT treat interruption as a completion-reactive cause.

#### Scenario: Live turn is interrupted
- **WHEN** the broker accepts `Interrupted` for the active turn of an alive invocation
- **THEN** lifecycle remains alive, activity becomes stopped, and public status becomes stopped without a reason

#### Scenario: Completion listener observes interruption
- **WHEN** an interrupted transition is published to local listeners, sinks, or the hub
- **THEN** the complete resulting public view contains stopped status with no completed or failed reason

#### Scenario: Late activity follows interruption
- **WHEN** working or attention activity for the same identified turn arrives after interruption
- **THEN** the reducer retains stopped activity and exposes no actionable effective cause

#### Scenario: Duplicate terminal signal follows interruption
- **WHEN** a completed, failed, or interrupted signal arrives for the turn already marked interrupted
- **THEN** the reducer does not expose a second terminal cause

#### Scenario: New turn follows interruption
- **WHEN** a verified new-turn event arrives after an interrupted turn
- **THEN** the reducer starts a new turn generation and changes activity to working

### Requirement: Internal event schema changes apply in place
Internal normalized event kinds and reducer inputs SHALL use the target schema directly without a schema-version increment, compatibility variants, or legacy aliases. Public envelope versions and the four-value public status vocabulary SHALL remain unchanged.

#### Scenario: Interrupted event is serialized
- **WHEN** an interrupted normalized event is persisted
- **THEN** it uses the canonical in-place internal event representation

#### Scenario: Incompatible retained alpha data is encountered
- **WHEN** retained internal data cannot decode under the target schema
- **THEN** SessionTap reports the incompatibility without invoking a legacy decoder or changing a schema version
