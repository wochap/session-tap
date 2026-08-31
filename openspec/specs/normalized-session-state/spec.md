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

### Requirement: Evidence authority is enforced by observation channel
The broker SHALL accept normalized facts only within the authority assigned to their trusted evidence channel. Managed hooks and verified provider side channels SHALL be allowed to assert mapped provider state; process observations SHALL assert lifecycle only; provider-artifact observations SHALL enrich approved metadata or usage only.

#### Scenario: Process observation carries activity
- **WHEN** a process-observation event attempts to assert working, waiting, idle, or terminal provider activity
- **THEN** the broker rejects that activity assertion while retaining an eligible lifecycle observation

#### Scenario: Artifact observation carries lifecycle
- **WHEN** provider-artifact evidence attempts to assert process lifecycle, agent activity, attention, or tool execution
- **THEN** the broker rejects those facts without changing current state

#### Scenario: Authenticated hook asserts mapped activity
- **WHEN** authenticated managed-hook evidence carries an adapter-approved activity assertion
- **THEN** the broker applies it subject to session, turn, ordering, and terminal-generation guards

### Requirement: State timing and confirmation are private durable state
The broker SHALL retain the trusted receipt time at which current activity began, the latest accepted authoritative assertion time, whether restored nonterminal activity has live confirmation, and the latest accepted evidence. Provider observation time SHALL NOT determine freshness.

#### Scenario: State changes
- **WHEN** an accepted authoritative event changes normalized activity
- **THEN** state start and last assertion times become the trusted receipt time and confirmation becomes live

#### Scenario: Same state is asserted again
- **WHEN** an authoritative event repeats current activity without changing it
- **THEN** the broker preserves state start time and refreshes last assertion time

#### Scenario: Enrichment arrives
- **WHEN** an enrichment-only event changes metadata or usage
- **THEN** it does not refresh activity assertion time or live confirmation

#### Scenario: Daemon restores nonterminal activity
- **WHEN** startup hydrates working, waiting-input, or waiting-approval activity for a process that remains alive
- **THEN** the broker marks that activity restored-unconfirmed until a matching authoritative live assertion arrives

### Requirement: Stale working activity expires without a terminal cause
The broker SHALL change working activity to unknown after 30 minutes without an accepted authoritative activity assertion. It SHALL preserve live process lifecycle and provider-session identity, clear current tool activity and incompatible status reason, and SHALL NOT emit a completed, failed, interrupted, idle, or lifecycle-exit cause.

#### Scenario: Working evidence becomes stale
- **WHEN** an alive invocation remains working for 30 minutes without an authoritative activity assertion
- **THEN** activity becomes unknown and public status becomes idle without a terminal reason

#### Scenario: Tool operation exceeds the stale interval
- **WHEN** current tool activity receives no authoritative progress or state assertion during the stale interval
- **THEN** the stale sweep clears the tool and changes working activity to unknown without claiming completion

#### Scenario: Waiting state remains silent
- **WHEN** an alive invocation remains waiting-input or waiting-approval without another hook
- **THEN** silence alone does not clear the waiting state or its compatible reason

#### Scenario: Activity resumes after expiry
- **WHEN** a later authoritative event asserts working for a nonterminal current turn
- **THEN** activity becomes working with live confirmation and fresh state timing

### Requirement: Current root tool activity reduces deterministically
The broker SHALL retain at most one private current root tool activity containing a bounded correlation ID, normalized label, optional safe detail, start time, and last-observed time. Tool updates SHALL obey turn, session, terminal, evidence-authority, and matching-ID guards.

#### Scenario: New tool starts
- **WHEN** an accepted authoritative start update identifies a root tool
- **THEN** it replaces current tool activity and records trusted start and last-observed times

#### Scenario: Matching tool progresses
- **WHEN** an accepted progress update matches current tool identity
- **THEN** it refreshes last-observed time without changing the original start time

#### Scenario: Matching tool finishes
- **WHEN** an accepted finish or failure update matches current tool identity
- **THEN** the broker clears current tool activity without treating tool failure as turn failure

#### Scenario: Late tool completion arrives
- **WHEN** a finish update identifies an older tool after another tool became current
- **THEN** the broker preserves the newer current tool activity

#### Scenario: Turn or session boundary arrives
- **WHEN** a new turn, idle state, terminal turn outcome, provider-session boundary, lifecycle exit, lifecycle loss, or stale-working expiry is accepted
- **THEN** the broker clears current tool activity

### Requirement: Evidence and activity remain private projections
Event evidence, confirmation, state timing, source ordering, collector revision, tool identity, and tool detail SHALL remain absent from `PublicAgentView`, public changed fields, local public envelopes, sinks, hub persistence, hub listening, routing input, and receiver output.

#### Scenario: Only private activity changes
- **WHEN** accepted tool progress changes private activity without changing any public field
- **THEN** the broker persists private state without enqueueing a public listener or sink update

#### Scenario: Public state changes with private activity
- **WHEN** one transaction changes both private activity and a public field
- **THEN** the published complete view contains only recognized public fields and its changed set excludes private paths

#### Scenario: Public golden fixtures are serialized
- **WHEN** status, listen, sink, hub, or receiver envelopes are serialized
- **THEN** their public schema remains unchanged

### Requirement: Evidence and activity schemas apply in place
Internal normalized events, request payloads, reducer state, and persisted snapshots SHALL use the target evidence, timing, confirmation, and activity types without schema-version increments, compatibility fields, or legacy aliases.

#### Scenario: Target snapshot is persisted
- **WHEN** the broker commits private evidence or current activity
- **THEN** it writes the target in-place internal representation while public envelope versions remain unchanged

#### Scenario: Incompatible retained alpha state is encountered
- **WHEN** retained internal data cannot decode into the target representation
- **THEN** SessionTap reports the incompatibility without legacy decoding or version negotiation

### Requirement: Public usage has stable cross-provider semantics
`PublicAgentView.usage.input_tokens` and `output_tokens` SHALL represent cumulative totals for the current provider session, `context_tokens` SHALL represent the provider's latest verified active-context occupancy, and `context_window_percent` SHALL represent the corresponding used percentage rounded to the nearest whole number and clamped to 0 through 100. Unavailable values SHALL remain absent rather than zero, and a new provider session SHALL clear usage inherited from the prior provider session.

#### Scenario: Cumulative totals exceed the context window
- **WHEN** repeated model calls cause cumulative input to exceed the provider's context capacity
- **THEN** SessionTap retains the cumulative input total while deriving context fields only from the latest active-context measurement

#### Scenario: Provider session changes
- **WHEN** a root invocation adopts a different provider session identity after clear, branch, or resume behavior that creates a new identity
- **THEN** SessionTap clears prior usage until verified values for the new provider session arrive

#### Scenario: Percentage has fractional precision
- **WHEN** a provider reports or yields a valid used percentage containing a fractional part
- **THEN** SessionTap publishes the nearest whole percentage within 0 through 100

### Requirement: Complete provider-artifact snapshots reduce atomically
A provider collector SHALL return one complete normalized enrichment candidate
from verified hooks and provider artifacts. Applying that candidate SHALL
replace prior usage atomically, SHALL produce a public update only when the
projection changes, and SHALL NOT change lifecycle, activity, status reason,
tool activity, state assertion timing, or live confirmation. Unverifiable
context values SHALL remain absent.

#### Scenario: Claude has no verified context denominator
- **WHEN** a Claude transcript verifies current context tokens and cumulative totals without an exact denominator
- **THEN** the collector emits those verified values with context-window percentage absent

#### Scenario: Collected usage is unchanged
- **WHEN** an asynchronous refresh yields a usage snapshot equal to normalized current usage
- **THEN** SessionTap creates no public listener, sink, or hub update for that result

#### Scenario: Context becomes temporarily unavailable
- **WHEN** a complete provider result retains cumulative totals but has no verified current context
- **THEN** SessionTap atomically replaces usage with the supplied totals and absent context fields without changing agent status

### Requirement: Provider artifact enrichment uses complete normalized values
The broker SHALL apply provider-artifact collection results only as complete typed provider-neutral enrichment for a currently authenticated provider agent-session binding. Cumulative input and output SHALL describe the current provider session, current context SHALL describe the latest verified active occupancy, and unavailable values SHALL remain absent. Provider-artifact evidence SHALL NOT assert lifecycle, activity, attention, turn, or tool state.

#### Scenario: Collected usage changes during work
- **WHEN** a current authenticated provider-session result changes verified usage while agent activity remains working
- **THEN** the broker atomically updates usage without changing activity, lifecycle, reason, turn, or tool state

#### Scenario: Context percentage is unavailable
- **WHEN** a provider collector verifies cumulative totals and context tokens but no exact percentage or denominator
- **THEN** the public view contains the verified values and leaves context-window percentage absent

#### Scenario: Provider session changes
- **WHEN** an invocation binds a different provider agent-session ID
- **THEN** enrichment inherited from the prior session is cleared until verified normalized values for the new session arrive

#### Scenario: Collection result is unchanged
- **WHEN** a complete normalized collection result projects the same public values already stored
- **THEN** the broker creates no listener-, sink-, or hub-visible update
