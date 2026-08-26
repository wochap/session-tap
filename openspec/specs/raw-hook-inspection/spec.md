# Raw Hook Inspection

## Purpose

Provide bounded, ephemeral foreground inspection of complete provider hook inputs for SessionTap-wrapped sessions without persisting or forwarding raw payloads.

## Requirements

### Requirement: Inspection is an explicit live command
SessionTap SHALL provide `sessiontap inspect-hooks` as a foreground command that observes hooks from SessionTap-wrapped providers only while the command is running. SessionTap MUST NOT provide a persistent configuration switch that globally enables raw inspection.

#### Scenario: Observe multiple providers
- **WHEN** a user runs `sessiontap inspect-hooks` while wrapped Codex and Claude sessions emit hooks
- **THEN** the command emits records for both providers until the inspector exits

#### Scenario: Inspector is not running
- **WHEN** wrapped providers emit hooks while no `sessiontap inspect-hooks` process is active
- **THEN** SessionTap exposes no raw hook payloads and ordinary tracking continues unchanged

### Requirement: Inspection exposes complete unnormalized hook input
For each observed managed hook, SessionTap SHALL emit exactly one JSON Lines record to inspector stdout with `provider`, `hook_type`, and `payload` fields. `hook_type` SHALL contain the provider's public event discriminator when available and SHALL otherwise be `null`; `payload` SHALL contain the complete unnormalized JSON value. SessionTap MUST expose unknown events and payloads that cannot be normalized, and MUST identify invalid JSON or oversized input without presenting truncated content as a complete payload.

#### Scenario: Provider supplies unknown fields
- **WHEN** an inspected hook receives valid JSON containing fields unknown to the adapter
- **THEN** the record's `payload` includes those fields and values without allowlisting, redaction, or normalization

#### Scenario: Normalization rejects an event
- **WHEN** an inspected hook payload cannot produce a normalized SessionTap event
- **THEN** the raw diagnostic record is still emitted and ordinary normalized ingestion remains fail-open

#### Scenario: Invalid hook input
- **WHEN** an inspected hook receives input that is not valid JSON
- **THEN** SessionTap emits a diagnostic record that identifies invalid input and safely represents the received bytes

### Requirement: Raw inspection data is never persisted or forwarded by SessionTap
SessionTap SHALL keep raw inspection records outside the broker protocol, database, state files, logs, configuration, normalized status and listen output, and configured sinks. SessionTap SHALL hold only bounded transient in-memory records needed to deliver foreground output.

#### Scenario: Inspection completes
- **WHEN** the foreground inspector exits
- **THEN** SessionTap removes its inspection endpoint and retains no raw payload in SessionTap-managed storage

#### Scenario: Sinks are configured
- **WHEN** raw inspection runs while stdout or HTTP sinks are enabled
- **THEN** no raw inspection payload is placed in any sink payload or durable outbox record

### Requirement: Inspection warns about sensitive output
SessionTap SHALL warn on inspector stderr when inspection starts that raw hook payloads can contain sensitive information and that terminal scrollback or explicit user redirection may retain the output.

#### Scenario: User starts inspection
- **WHEN** the `inspect-hooks` command is accepted
- **THEN** the warning is displayed before the inspection endpoint begins accepting records

### Requirement: Inspection remains bounded and fail-open
SessionTap MUST NOT allow unavailable, slow, malformed, or overloaded inspection handling to block provider operation or prevent the ordinary normalized hook path. Inspection transport and queues SHALL be bounded.

#### Scenario: Inspector is unavailable
- **WHEN** a managed hook cannot connect or authenticate to the invocation's inspection endpoint
- **THEN** it continues the ordinary normalization and broker submission path without provider-visible failure

#### Scenario: Foreground consumer is slow
- **WHEN** inspection records arrive faster than the foreground can print them
- **THEN** SessionTap bounds memory and hook latency and reports dropped diagnostic records without persisting them
