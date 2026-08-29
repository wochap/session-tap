# Agent Hook Adapters

## Purpose

Session-gated Claude Code, Codex, and Qwen event ingestion — mapping provider hook signals into the common event model through a versioned adapter contract, with safe hook configuration management and clean-room implementation constraints.

## Requirements

### Requirement: Built-in providers are normalized
SessionTap SHALL provide separate concrete adapters for Claude Code, Codex, and Qwen. Each adapter SHALL own its provider's hook configuration, launch preparation, raw payload interpretation, normalized event mapping, metadata extraction, fixtures, and tests. Each adapter SHALL map supported root-agent session, prompt, tool, permission, notification, stop, failure-stop, explicit-idle, and session-end signals into the common internal event model when those signals are available. Adapters SHALL distinguish provider-session lifecycle from supervised process lifecycle and SHALL normalize verified provider turn IDs, model, effort, permission mode, and usage fields into provider-neutral optional metadata.

#### Scenario: Provider behavior changes independently
- **WHEN** one provider changes its hook names, payload shape, setup requirements, or launch options
- **THEN** SessionTap changes that provider's concrete adapter without adding provider conditionals to another provider adapter

#### Scenario: New user turn begins
- **WHEN** a built-in adapter receives its provider's root-agent user-prompt or turn-start signal
- **THEN** it emits working activity, identifies the internal event as a new turn, and includes the provider's verified turn identifier when available

#### Scenario: Provider asks a question
- **WHEN** an adapter can distinguish a root-agent user question or elicitation from a permission approval, including a recognized Claude `AskUserQuestion` permission request
- **THEN** it emits waiting_input rather than waiting_approval with optional bounded question context

#### Scenario: Provider requests approval
- **WHEN** a built-in adapter receives a root-agent permission request that is not a recognized ordinary-input tool
- **THEN** it emits waiting_approval with optional bounded provider-neutral approval context

#### Scenario: Provider ends a turn normally
- **WHEN** a provider emits its normal root-agent turn-stop signal
- **THEN** the adapter identifies the internal event as completed and emits stopped activity without marking the process exited

#### Scenario: Provider turn fails
- **WHEN** a provider emits a documented root-agent failure-stop signal
- **THEN** the adapter identifies the internal event as failed and emits stopped activity without marking the process exited

#### Scenario: Provider emits an explicit idle signal
- **WHEN** a provider emits a verified root-agent idle notification after a turn stop
- **THEN** the adapter emits explicit idle activity rather than enrichment or another completion

#### Scenario: Provider session ends while wrapper remains alive
- **WHEN** a provider emits `SessionEnd` without a supervised process-exit observation
- **THEN** the adapter emits a provider-session end cause that does not mark the invocation lifecycle exited or reset stopped activity

#### Scenario: Verified provider metadata is present
- **WHEN** a supported hook includes verified model, effort, permission mode, turn, token, or context-utilization fields
- **THEN** the adapter emits only their sanitized provider-neutral representations and omits unavailable fields

#### Scenario: Usage is unavailable
- **WHEN** a provider hook contains no verified token or context-utilization fields
- **THEN** the adapter does not estimate or synthesize usage

### Requirement: Provider metadata extraction is bounded and evidence-based
Built-in adapters SHALL normalize only provider metadata established by public contracts or sanitized independent captures, SHALL validate categorical and numeric values, and SHALL discard arbitrary unknown metadata and conversational content except for explicitly selected bounded status summaries.

#### Scenario: Claude turn metadata repeats across hooks
- **WHEN** Claude supplies one `prompt_id` on prompt, tool, permission, notification, and stop hooks
- **THEN** SessionTap maps it to one provider-neutral turn ID for correlation

#### Scenario: Permission mode changes during a turn
- **WHEN** a verified provider hook changes permission mode to a documented value including `default`, `acceptEdits`, `auto`, `auto_edit`, `plan`, `yolo`, `dontAsk`, or `bypassPermissions`
- **THEN** SessionTap updates the normalized current permission mode without copying unrelated tool input

#### Scenario: Model contains terminal formatting
- **WHEN** a provider model value contains control or terminal-formatting sequences
- **THEN** SessionTap removes those sequences, bounds the result, and omits the field if no safe value remains

#### Scenario: Qwen reports fractional context utilization
- **WHEN** a verified Qwen hook supplies `context_usage` equal to `0.5`
- **THEN** SessionTap normalizes context-window utilization to 50 percent

#### Scenario: Qwen reports hook metadata
- **WHEN** a verified Qwen hook supplies a valid timestamp or documented permission mode
- **THEN** SessionTap retains the provider-observed time and sanitized permission mode in provider-neutral fields

#### Scenario: Qwen emits an empty prompt callback
- **WHEN** Qwen emits `UserPromptSubmit` with an empty prompt around tool activity
- **THEN** SessionTap treats it as enrichment rather than starting a new turn

#### Scenario: Unrecognized usage shape arrives
- **WHEN** a hook supplies token-like fields at an unverified path or with invalid units
- **THEN** SessionTap ignores those fields rather than guessing their meaning

### Requirement: Attention context is bounded and provider-neutral
Built-in adapters SHALL derive optional current status context while the raw hook payload is available, SHALL use deterministic event-specific selection rules, and SHALL NOT include the complete hook payload or arbitrary unknown tool input. Selected question, message, description, command, and final-response text SHALL remove controls, collapse whitespace, and retain at most the first 100 Unicode characters before any short tool prefix is added.

#### Scenario: Approval includes a description
- **WHEN** a permission request includes a non-empty provider-supplied description for a Bash tool
- **THEN** the adapter emits a summary in the form `bash <description excerpt>`

#### Scenario: Command approval has no description
- **WHEN** a recognized command tool lacks a description but includes a command
- **THEN** the adapter emits `<normalized tool label> <first 100 sanitized command characters>`

#### Scenario: Provider asks an explicit question
- **WHEN** an input event contains a direct question or a documented non-empty questions array
- **THEN** the adapter uses the first question before considering a generic agent or notification message

#### Scenario: Input has only an agent message
- **WHEN** an input event contains no explicit question but includes an agent or provider message
- **THEN** the adapter uses the first 100 sanitized characters of that message

#### Scenario: Stop includes a final assistant message
- **WHEN** a normal root-agent stop includes `last_assistant_message`
- **THEN** the adapter emits completed context containing its first 100 sanitized characters

#### Scenario: Stop has no usable message
- **WHEN** a normal root-agent stop has no non-empty eligible message
- **THEN** the adapter emits completion without a status reason and does not synthesize generic text

#### Scenario: Unknown tool requests approval
- **WHEN** an unrecognized tool requests approval without a description or command
- **THEN** the adapter exposes one bounded sanitized tool label and does not serialize its arguments

#### Scenario: Patch tool requests approval
- **WHEN** an apply-patch operation requires approval without a description
- **THEN** the adapter emits only a short normalized patch label and does not expose the patch body

### Requirement: Raw conversational content remains private
Adapters SHALL discard prompts, transcripts, complete assistant messages, raw error details, and unselected tool input. They MAY select only the bounded current question/message, approval description/command, final assistant excerpt, or allowlisted failure category required by the public status-reason contract.

#### Scenario: Claude transcript contains a session title
- **WHEN** a Claude hook supplies a readable transcript path containing `aiTitle` or `customTitle` metadata
- **THEN** the adapter exposes only the latest bounded one-line title as the provider session name and does not normalize or persist the transcript path or body

#### Scenario: Stop includes an assistant message
- **WHEN** a root-agent stop hook includes `last_assistant_message`
- **THEN** SessionTap selects at most its first 100 sanitized characters as current completed context and does not retain the complete message

#### Scenario: Non-stop hook includes an assistant message
- **WHEN** a hook not selected for question or completion context includes assistant text
- **THEN** SessionTap does not copy that text into normalized state, current reason, or public metadata

#### Scenario: Command fallback is selected
- **WHEN** approval context lacks a description and a command is available
- **THEN** the adapter selects only its first 100 sanitized characters after a normalized tool label and discards the remaining command and arguments

### Requirement: Provider mappings avoid duplicate attention causes
Adapters and the broker SHALL distinguish direct attention signals from delayed provider reminders and explicit idle signals so one unresolved attention episode does not produce repeated causes and an idle notification does not masquerade as completion.

#### Scenario: Delayed permission notification follows a direct request
- **WHEN** a provider emits `Notification(permission_prompt)` after an unresolved `PermissionRequest` for the invocation
- **THEN** SessionTap treats the notification as enrichment of the active blocked state rather than a second waiting_approval cause

#### Scenario: Idle notification follows completion
- **WHEN** a provider emits a documented `idle_prompt` after a stopped response
- **THEN** SessionTap emits one explicit idle transition and clears the completed or failed reason

### Requirement: Global hooks observe only wrapped sessions
Managed hook entries SHALL be safe to install in user-level provider configuration but SHALL emit events only when both a valid SessionTap invocation credential is present and the launch provider matches the hook provider.

#### Scenario: Provider launched without SessionTap
- **WHEN** a globally configured managed hook runs without SessionTap invocation context
- **THEN** it drains required input, emits no event, prints no provider-visible output, and exits successfully

#### Scenario: Nested unwrapped provider inherits context
- **WHEN** an unwrapped Codex process inherits context from an SessionTap-wrapped Claude process
- **THEN** the Codex hook rejects the provider mismatch and does not attribute events to the Claude invocation

### Requirement: Hook configuration merges are reversible
SessionTap SHALL atomically and idempotently add only its own hook entries, preserve user-authored entries and ordering where provider semantics require it, diagnose invalid configuration without overwriting it, and remove only SessionTap-owned entries on request.

#### Scenario: Existing user hooks
- **WHEN** setup runs against a valid configuration containing user-authored hooks
- **THEN** those hooks remain configured after SessionTap's managed entries are added or refreshed

#### Scenario: Invalid provider configuration
- **WHEN** the existing provider configuration cannot be parsed safely
- **THEN** SessionTap leaves it unchanged, reports the problem, and launches the provider in fail-open untracked mode if necessary

### Requirement: Managed hooks support ephemeral raw inspection
Managed provider hooks SHALL route the complete hook input to the ephemeral inspection channel when an inspector is active. The inspection branch MUST retain provider binding, invocation gating, bounded execution, and fail-open semantics, and MUST run independently of normalization success.

#### Scenario: Inspected wrapped session
- **WHEN** a managed hook runs for a wrapped provider invocation while the inspector endpoint exists
- **THEN** it sends the unnormalized hook input to the inspection channel and continues ordinary normalization

#### Scenario: Hook lacks valid inspection context
- **WHEN** a managed hook runs globally, for a different provider, without valid SessionTap invocation context, or while no inspector is active
- **THEN** it sends no raw input to an inspection channel and preserves existing session-gated hook behavior

### Requirement: Codex hook trust is handled independently
The Codex adapter SHALL satisfy the installed Codex version's hook trust requirements using public contracts or independently verified behavior and SHALL fail diagnostically rather than copying external trust implementations.

#### Scenario: Unsupported Codex hook contract
- **WHEN** SessionTap cannot establish that its hook entry is trusted for the installed Codex version
- **THEN** it reports degraded Codex observability without modifying unrelated trust entries

### Requirement: Qwen remains an interactive TUI
The Qwen adapter SHALL use hooks as its baseline and MAY add Qwen's dedicated dual-output side channel for richer events, but SHALL NOT select headless stream-json output for an interactive launch.

#### Scenario: Qwen supports dual output
- **WHEN** the installed Qwen version supports a non-TUI JSON side channel and the user did not supply a conflicting side-channel option
- **THEN** SessionTap may inject a private side-channel destination while Qwen continues rendering its ordinary interactive TUI

#### Scenario: User supplied a Qwen side-channel option
- **WHEN** Qwen arguments already select a JSON side-channel destination
- **THEN** SessionTap preserves the user's argument and uses hook-only observation rather than overriding it

### Requirement: Adapter behavior is versioned and extensible
The core SHALL depend on the adapter trait rather than provider conditionals. The registry SHALL instantiate separate built-in Claude, Codex, and Qwen adapter types, and configuration SHALL allow a custom executable name to inherit one built-in adapter's complete behavior and redaction policy.

#### Scenario: Claude-compatible wrapper
- **WHEN** the user configures `company-claude` to inherit the Claude adapter
- **THEN** `sessiontap company-claude [args...]` launches that executable with Claude-compatible hook normalization and a distinct configured provider identity

#### Scenario: Built-in adapter registration
- **WHEN** the adapter registry is initialized
- **THEN** each built-in provider resolves to its own concrete adapter implementation rather than a dialect-parameterized generic implementation

### Requirement: Implementation is clean-room MIT work
The project SHALL NOT copy or mechanically transform external non-MIT source, tests, generated hook scripts, or implementation-specific trust algorithms and SHALL document the public or independently captured basis for each provider mapping.

#### Scenario: Adapter contribution
- **WHEN** a provider mapping is added or changed
- **THEN** its tests and documentation identify the provider contract or sanitized independent fixture used to establish the behavior

### Requirement: Adapter output is normalized-only
A provider adapter SHALL pass only typed provider-neutral events and selected bounded status context across the adapter boundary, or explicitly report that a payload was ignored. It SHALL NOT choose or overwrite the configured provider identity and SHALL NOT expose raw provider JSON, provider store records, arbitrary unknown fields, or provider-specific payload types to the daemon, storage, sinks, or hub. The daemon SHALL stamp normalized facts with the configured provider identity associated with the authenticated invocation; adapter dialect SHALL remain an internal dispatch detail.

#### Scenario: Hook contains unknown provider data
- **WHEN** a raw root-agent hook contains fields that the provider adapter does not explicitly normalize
- **THEN** those fields do not cross the adapter boundary or appear in persisted normalized state

#### Scenario: Hook belongs to a subagent
- **WHEN** a hook includes a non-empty documented `agent_id` or is a subagent lifecycle event
- **THEN** the adapter ignores the payload before extracting activity, reason, session, metadata, or usage and the root public view does not change

#### Scenario: Main session uses a named agent
- **WHEN** a root hook contains `agent_type` without a subagent `agent_id`
- **THEN** the adapter does not ignore it solely because the main session has an agent type

#### Scenario: Configured provider inherits a dialect
- **WHEN** `company-claude` is configured to inherit the Claude adapter
- **THEN** the Claude adapter normalizes its hook while the daemon records `company-claude`, not `claude`, as the invocation's configured provider identity

### Requirement: Future provider enrichment shares the normalized boundary
A future provider-specific collector that reads agent-owned stores, histories, or transcripts SHALL live with that provider integration and SHALL emit sanitized typed metadata through the same normalized internal state path used by hook-derived metadata.

#### Scenario: Provider history contains additional metadata
- **WHEN** a future collector extracts a verified safe fact unavailable in hook payloads
- **THEN** downstream storage, public projection, sinks, and hub aggregation consume the normalized fact without parsing or identifying the provider's source format

### Requirement: Provider event dispatch is exact and fail-closed
Each built-in adapter SHALL classify event names, notification subtypes, and special tool names using provider-owned exact mappings with explicit accepted spelling variants. An unsupported value SHALL produce `AdapterOutcome::Ignored` unless that exact value is explicitly defined as provider-neutral enrichment.

#### Scenario: Documented event is received
- **WHEN** a built-in adapter receives an exact supported root-agent event name
- **THEN** it emits the event kind and selected bounded context defined by that provider's mapping

#### Scenario: Unknown event resembles a supported event
- **WHEN** an event name merely contains text such as `permission`, `stop`, `question`, or `tool` but is not an accepted exact value
- **THEN** the adapter ignores it without changing normalized state or persisting an enrichment event

#### Scenario: Supported spelling variants exist
- **WHEN** a provider contract establishes multiple spellings for one event or field
- **THEN** the adapter lists those variants explicitly and maps them to one semantic outcome

#### Scenario: Unknown notification subtype arrives
- **WHEN** a recognized notification event carries a subtype outside the provider's exact allowlist
- **THEN** the adapter ignores the notification without asserting attention or idle state

### Requirement: Managed root hooks cover supported authoritative signals
Each built-in adapter SHALL register and explicitly map every available root-agent hook needed for session boundaries, new turns, tool progress, ordinary input, approval, explicit idle, compaction, interruption, normal completion, failure, and provider-session end. Registration SHALL remain provider-specific and fail open when an installed provider does not support an expected hook.

#### Scenario: Tool lifecycle completes
- **WHEN** Claude, Codex, or Qwen emits a supported root `PostToolUse` event
- **THEN** the adapter emits working activity for the current turn without creating a new turn

#### Scenario: Provider asks for ordinary input through a tool
- **WHEN** Codex invokes `request_user_input` or Claude or Qwen invokes an explicitly recognized ask-user tool
- **THEN** the adapter emits waiting-input with optional bounded question context

#### Scenario: Provider emits an elicitation notification
- **WHEN** a supported provider emits an exact elicitation or agent-needs-input notification subtype
- **THEN** the adapter emits waiting-input rather than approval or enrichment

#### Scenario: Provider compacts context
- **WHEN** a supported root-agent pre-compact or post-compact event establishes active compaction
- **THEN** the adapter emits the provider-defined working or enrichment assertion without creating a new session or turn

#### Scenario: Hook is unavailable
- **WHEN** setup establishes that the installed provider cannot register a configured event
- **THEN** setup reports degraded observability and preserves an interactive fail-open launch

### Requirement: Provider interruptions are normalized explicitly
A built-in adapter SHALL map a documented root-agent cancellation, interrupt event, or unambiguous interrupted stop field to `Interrupted`. It SHALL NOT map session closure, channel shutdown, or user cancellation to normal completion.

#### Scenario: Codex reports interrupt
- **WHEN** Codex emits its exact root-agent `Interrupt` event
- **THEN** the adapter emits `Interrupted` for the identified turn

#### Scenario: Provider stop carries interruption evidence
- **WHEN** a supported provider stop event contains documented unambiguous interruption evidence
- **THEN** the adapter emits `Interrupted` instead of `Completed`

#### Scenario: Stop has no interruption evidence
- **WHEN** a normal root-agent stop contains no documented interruption evidence
- **THEN** the adapter emits `Completed`

### Requirement: Provider artifact access is private and validated
An adapter SHALL validate a provider-supplied transcript or session-artifact path before reading it. Validation SHALL bind the path to the authenticated invocation and provider, canonicalize it beneath an allowed provider-owned root, require an eligible regular file, and enforce bounded reads. Failure SHALL omit only artifact-derived enrichment.

#### Scenario: Valid transcript supplies a session title
- **WHEN** an authenticated provider hook supplies an eligible transcript beneath its allowed provider data root
- **THEN** the adapter may derive a bounded session title without placing the path or transcript body in normalized output

#### Scenario: Path escapes the provider root
- **WHEN** a candidate path is absolute, relative, or symlinked outside the allowed provider data root
- **THEN** the adapter rejects artifact access and continues normalizing safe hook fields

#### Scenario: Artifact is invalid or oversized
- **WHEN** the candidate is missing, not an eligible regular file, changes identity during access, or exceeds configured read bounds
- **THEN** the adapter omits artifact-derived enrichment without failing the provider session

#### Scenario: Normalized output is serialized
- **WHEN** an accepted hook produces internal or public data
- **THEN** no artifact path appears in normalized events, snapshots, reasons, status output, sinks, or hub envelopes

### Requirement: Subagent payloads remain outside root normalization
Built-in adapters SHALL reject a payload with a documented non-empty child-agent identity or child-agent lifecycle event before extracting state, attention, metadata, usage, or artifacts.

#### Scenario: Child hook includes root session identity
- **WHEN** a subagent payload also contains the root provider session ID or turn ID
- **THEN** the adapter ignores the entire payload and the root invocation remains unchanged

### Requirement: Adapter evidence is typed and transport-stamped
Every normalized adapter event SHALL carry typed evidence identifying its observation channel and trust basis. Collector revision, bounded collector instance identity, and source sequence SHALL be optional. Evidence fields MUST be stamped after invocation authentication or by a trusted local collector and MUST NOT be copied from provider payload fields.

#### Scenario: Managed hook is authenticated
- **WHEN** a managed provider hook passes invocation and provider authentication
- **THEN** the daemon stamps authenticated managed-hook evidence and the active collector revision

#### Scenario: Provider attempts to supply evidence
- **WHEN** provider JSON contains fields named `source`, `verified`, `collectorRevision`, `clientRevision`, or `sequence`
- **THEN** those values do not control normalized evidence unless the trusted transport independently establishes the corresponding fact

#### Scenario: Local process observation is created
- **WHEN** SessionTap observes child-process binding, exit, or liveness locally
- **THEN** it stamps local process-observation evidence rather than authenticated-hook evidence

#### Scenario: Collector supplies no ordering
- **WHEN** a trusted collector cannot establish a source sequence
- **THEN** it omits source ordering and the normalized event remains valid

### Requirement: Adapters emit bounded root tool activity
A built-in adapter SHALL convert supported root-agent tool lifecycle payloads into provider-neutral tool activity updates containing only phase, a bounded normalized tool label, an optional bounded correlation ID, and optional allowlisted safe detail. It SHALL discard arbitrary tool input, commands, URLs, results, errors, and provider-specific objects.

#### Scenario: Root tool starts
- **WHEN** a supported root pre-tool event contains a valid tool name and correlation ID
- **THEN** the adapter emits a start update with the normalized label and bounded correlation ID

#### Scenario: Root tool completes
- **WHEN** a supported root post-tool event contains a correlation ID
- **THEN** the adapter emits a finish update identifying the matching tool without copying its result

#### Scenario: Tool has an eligible description
- **WHEN** an exact provider/tool mapping permits a scalar description and the value passes sanitization, redaction, and bounds
- **THEN** the adapter may emit that value as safe activity detail

#### Scenario: Tool targets a workspace file
- **WHEN** an exact provider/tool mapping permits a file target that canonicalizes beneath the invocation workspace
- **THEN** the adapter may emit a bounded workspace-relative target as safe activity detail

#### Scenario: Tool input contains a command or URL
- **WHEN** arbitrary tool input contains a shell command, URL, credential, nested object, or unapproved scalar
- **THEN** the adapter omits it from tool activity and all normalized metadata

#### Scenario: Subagent tool event arrives
- **WHEN** a tool lifecycle payload belongs to a documented child agent
- **THEN** the adapter ignores it before emitting evidence or tool activity

### Requirement: Adapter output changes apply in place
Normalized adapter output SHALL use typed evidence and tool activity directly without retaining the free-form source field, compatibility aliases, or version-negotiated variants.

#### Scenario: Adapter event is serialized
- **WHEN** a built-in adapter emits a normalized root event
- **THEN** its target internal representation contains typed evidence and no legacy source field
