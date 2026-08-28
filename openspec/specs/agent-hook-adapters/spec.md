# Agent Hook Adapters

## Purpose

Session-gated Claude Code, Codex, and Qwen event ingestion — mapping provider hook signals into the common event model through a versioned adapter contract, with safe hook configuration management and clean-room implementation constraints.

## Requirements

### Requirement: Built-in providers are normalized
SessionTap SHALL provide independently implemented adapters for Claude Code, Codex, and Qwen that map documented session, prompt, tool, permission, notification, stop, failure-stop, and session-end signals into the common event model when those signals are available. Adapters SHALL distinguish provider-session lifecycle from supervised process lifecycle and SHALL normalize verified provider turn IDs, model, effort, permission mode, and usage fields into provider-neutral optional metadata.

#### Scenario: New user turn begins
- **WHEN** a built-in adapter receives its provider's user-prompt or turn-start signal
- **THEN** it emits working activity, identifies the event as a new turn, and includes the provider's verified turn identifier when available

#### Scenario: Provider asks a question
- **WHEN** an adapter can distinguish a user question or elicitation from a permission approval, including a recognized Claude `AskUserQuestion` permission request
- **THEN** it emits waiting_input rather than waiting_approval without retaining the raw question or options

#### Scenario: Provider requests approval
- **WHEN** a built-in adapter receives a provider permission request that is not a recognized ordinary-input tool
- **THEN** it emits waiting_approval with optional bounded provider-neutral attention context

#### Scenario: Provider ends a turn normally
- **WHEN** a provider emits its normal turn-stop signal
- **THEN** the adapter identifies the event as completed and emits idle activity without marking the process stopped

#### Scenario: Provider turn fails
- **WHEN** a provider emits a documented failure-stop signal
- **THEN** the adapter identifies the event as failed and emits idle activity without marking the process stopped

#### Scenario: Provider emits an idle reminder
- **WHEN** a provider emits an idle-prompt notification without a turn-stop cause
- **THEN** the adapter emits enrichment rather than completed or failed

#### Scenario: Provider session ends while wrapper remains alive
- **WHEN** a provider emits `SessionEnd` without a supervised process-exit observation
- **THEN** the adapter emits a provider-session end cause that does not mark the invocation lifecycle exited

#### Scenario: Verified provider metadata is present
- **WHEN** a supported hook includes verified model, effort, permission mode, turn, token, or context-utilization fields
- **THEN** the adapter emits only their sanitized provider-neutral representations and omits unavailable fields

#### Scenario: Usage is unavailable
- **WHEN** a Codex or Claude hook contains no verified token or context-utilization fields
- **THEN** the adapter does not estimate or synthesize usage

### Requirement: Provider metadata extraction is bounded and evidence-based
Built-in adapters SHALL normalize only provider metadata established by public contracts or sanitized independent captures, SHALL validate categorical and numeric values, and SHALL discard arbitrary unknown metadata and conversational content.

#### Scenario: Claude turn metadata repeats across hooks
- **WHEN** Claude supplies one `prompt_id` on prompt, tool, permission, notification, and stop hooks
- **THEN** SessionTap maps it to one provider-neutral turn ID for correlation

#### Scenario: Permission mode changes during a turn
- **WHEN** a verified provider hook changes permission mode from `default` to `acceptEdits` or `auto`
- **THEN** SessionTap updates the normalized current permission mode without copying tool input

#### Scenario: Model contains terminal formatting
- **WHEN** a provider model value contains control or terminal-formatting sequences
- **THEN** SessionTap removes those sequences, bounds the result, and omits the field if no safe value remains

#### Scenario: Qwen reports fractional context utilization
- **WHEN** a verified Qwen hook supplies `context_usage` equal to `0.5`
- **THEN** SessionTap normalizes context-window utilization to 50 percent

#### Scenario: Qwen reports hook metadata
- **WHEN** a verified Qwen hook supplies a valid timestamp or an observed permission mode such as `auto`, `plan`, or `yolo`
- **THEN** SessionTap retains the provider-observed time and sanitized permission mode in provider-neutral fields

#### Scenario: Qwen emits an empty prompt callback
- **WHEN** Qwen emits `UserPromptSubmit` with an empty prompt around tool activity
- **THEN** SessionTap treats it as enrichment rather than starting a new turn

#### Scenario: Unrecognized usage shape arrives
- **WHEN** a hook supplies token-like fields at an unverified path or with invalid units
- **THEN** SessionTap ignores those fields rather than guessing their meaning

### Requirement: Attention context is bounded and provider-neutral
Built-in adapters SHALL derive optional attention context while the raw hook payload is available, SHALL use deterministic safe fallback rules, and SHALL NOT include the complete hook payload or arbitrary unknown tool input.

#### Scenario: Provider supplies a description
- **WHEN** a permission request includes a non-empty provider-supplied description
- **THEN** the adapter uses a sanitized bounded one-line form of that description before considering command text

#### Scenario: File tool has no description
- **WHEN** a recognized file operation lacks a description but includes a path
- **THEN** the adapter emits a tool-specific summary containing no file contents and no more path information than required for display

#### Scenario: Patch tool requests approval
- **WHEN** an apply-patch operation requires approval
- **THEN** the adapter emits a generic patch summary and does not expose the patch body

#### Scenario: Unknown tool requests approval
- **WHEN** an unrecognized tool requests approval without a safe description
- **THEN** the adapter exposes the bounded tool name alone and does not serialize its arguments

#### Scenario: Context contains multiple lines or controls
- **WHEN** selected display context contains line breaks or control characters
- **THEN** the adapter removes controls, collapses whitespace to one line, and enforces conservative character and UTF-8 byte limits

### Requirement: Raw conversational content remains private
Adapters SHALL discard prompts, transcripts, assistant messages, raw error details, and unselected tool input, and SHALL treat command redaction as conservative best effort rather than a guarantee.

#### Scenario: Claude transcript contains a session title
- **WHEN** a Claude hook supplies a readable transcript path containing `aiTitle` or `customTitle` metadata
- **THEN** the adapter exposes only the latest bounded one-line title as the provider session name and does not normalize or persist the transcript path or body

#### Scenario: Stop includes an assistant message
- **WHEN** a provider stop hook includes `last_assistant_message`
- **THEN** SessionTap emits the completion cause without copying that message into normalized state, active attention, or live event metadata

#### Scenario: Command may contain a credential
- **WHEN** a command fallback cannot be summarized without potentially exposing sensitive syntax
- **THEN** the adapter may replace it with a generic shell summary rather than claiming complete redaction

### Requirement: Provider mappings avoid duplicate attention causes
Adapters and the broker SHALL distinguish direct attention signals from delayed provider reminder notifications so one unresolved attention episode does not produce repeated notification causes.

#### Scenario: Delayed permission notification follows a direct request
- **WHEN** a provider emits `Notification(permission_prompt)` after an unresolved `PermissionRequest` for the invocation
- **THEN** SessionTap treats the notification as enrichment of the active attention state rather than a second waiting_approval cause

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
The core SHALL depend on a versioned adapter trait rather than provider conditionals, and configuration SHALL allow a custom executable name to inherit a built-in adapter dialect and redaction policy.

#### Scenario: Claude-compatible wrapper
- **WHEN** the user configures `company-claude` to inherit the Claude adapter
- **THEN** `sessiontap company-claude [args...]` launches that executable with Claude-compatible hook normalization and a distinct configured provider identity

### Requirement: Implementation is clean-room MIT work
The project SHALL NOT copy or mechanically transform external non-MIT source, tests, generated hook scripts, or implementation-specific trust algorithms and SHALL document the public or independently captured basis for each provider mapping.

#### Scenario: Adapter contribution
- **WHEN** a provider mapping is added or changed
- **THEN** its tests and documentation identify the provider contract or sanitized independent fixture used to establish the behavior
