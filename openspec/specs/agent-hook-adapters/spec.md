# Agent Hook Adapters

## Purpose

Session-gated Claude Code, Codex, and Qwen event ingestion — mapping provider hook signals into the common event model through a versioned adapter contract, with safe hook configuration management and clean-room implementation constraints.

## Requirements

### Requirement: Built-in providers are normalized
SessionTap SHALL provide independently implemented adapters for Claude Code, Codex, and Qwen that map documented session, prompt, tool, permission, notification, stop, and session-end signals into the common event model when those signals are available.

#### Scenario: New user turn begins
- **WHEN** a built-in adapter receives its provider's user-prompt or turn-start signal
- **THEN** it emits working activity and identifies the event as a new turn

#### Scenario: Provider asks a question
- **WHEN** an adapter can distinguish a user question from a permission approval
- **THEN** it emits waiting_input rather than waiting_approval

#### Scenario: Provider ends a turn
- **WHEN** a provider emits its normal or failure turn-stop signal
- **THEN** the adapter emits idle activity without marking the process stopped

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
