# Provider Session Collection

## Purpose

Define provider-owned, asynchronous, authenticated collection of normalized session metadata and usage from supported hooks and provider artifacts.

## Requirements

### Requirement: Providers expose one normalized collection API
Each built-in provider module SHALL implement the same asynchronous session-collection contract and SHALL return only sanitized provider-neutral enrichment, bounded diagnostics, and opaque provider cursor state. Common adapter code SHALL select and invoke the concrete provider implementation without containing provider event names, paths, record fields, accounting rules, or parsing branches.

#### Scenario: Central orchestration collects Claude data
- **WHEN** the registry selects the Claude adapter for an authenticated session refresh
- **THEN** common orchestration invokes Claude's standard collection operation and consumes its normalized result without interpreting Claude records

#### Scenario: Provider artifact format changes
- **WHEN** one provider changes its session artifact schema
- **THEN** the implementation and fixtures for that concrete provider change without adding provider-specific logic to common adapter orchestration or another provider module

### Requirement: Each provider owns artifact collection end to end
The Claude, Codex, and Qwen modules SHALL each own their session locator extraction, allowed-root validation, file access, record decoding, bounded-read policy, cursor representation, deduplication, accounting, metadata extraction, cancellation checkpoints, fixtures, and tests. The system SHALL NOT use a cross-provider JSONL parser or metadata extraction function.

#### Scenario: Qwen session data is collected
- **WHEN** Qwen collection receives a verified Qwen agent-session locator
- **THEN** only the Qwen module locates, reads, parses, and accounts for that artifact before returning normalized enrichment

#### Scenario: Provider cursor crosses the common boundary
- **WHEN** a provider returns incremental collection state
- **THEN** common orchestration stores and returns it opaquely without interpreting provider-specific identities, offsets, or accumulators

### Requirement: Collection uses hooks and provider artifacts only
SessionTap SHALL derive provider session metadata and usage only from verified supported hook fields or provider-owned session artifacts. It SHALL NOT install, replace, wrap, execute, or inspect provider statusline configuration, and it SHALL leave any value absent when neither allowed source verifies it.

#### Scenario: Claude lacks a verified context denominator
- **WHEN** Claude hooks and the selected root session artifact expose context tokens but no exact denominator or percentage
- **THEN** SessionTap publishes the verified context tokens and leaves context-window percentage absent

#### Scenario: User configured a custom statusline
- **WHEN** SessionTap setup or collection runs for Claude
- **THEN** the user's statusline configuration and command remain unread, unmodified, and unexecuted by SessionTap

### Requirement: Collection is asynchronous and hook driven
After an authenticated eligible root hook or lifecycle event identifies a provider agent-session, SessionTap SHALL schedule collection without waiting for artifact access, parsing, or enrichment persistence before completing hook or lifecycle processing. Events without a verified provider agent-session ID SHALL schedule no artifact collection.

#### Scenario: Hook triggers a slow provider read
- **WHEN** an accepted root hook identifies an artifact whose provider collection is slow
- **THEN** hook acknowledgement completes independently while collection continues asynchronously

#### Scenario: Session identity is unavailable
- **WHEN** an eligible lifecycle event occurs before a verified provider agent-session ID is known
- **THEN** SessionTap performs no artifact scan and does not synthesize a session identity

### Requirement: Collection is debounced per provider agent-session
The coordinator SHALL key debounce state by provider-qualified agent-session ID. A newer eligible event for the same key SHALL supersede its pending timer or running collection, while different keys MAY collect independently under the global concurrency bound. Qualification SHALL prevent equal raw session IDs belonging to different configured providers or concrete adapters from sharing state.

#### Scenario: Several hooks fire for one agent session
- **WHEN** multiple eligible hooks for the same provider agent-session arrive inside the debounce window
- **THEN** SessionTap cancels and restarts that session's debounce and invokes collection once for the newest generation

#### Scenario: Hook arrives during collection
- **WHEN** a newer hook for the same provider agent-session arrives while collection is running
- **THEN** SessionTap cancels the running generation, waits for it to exit, discards any result, and reruns collection after the newest debounce interval

#### Scenario: Different sessions receive hooks
- **WHEN** hooks identify two different provider-qualified agent-session IDs
- **THEN** each session maintains independent debounce and collection state

### Requirement: Provider collectors support cooperative cancellation
Every provider collector SHALL observe cancellation before file access and at bounded checkpoints during reading and parsing. A cancelled or superseded generation MUST NOT publish enrichment, and no two collectors for one provider-qualified agent-session SHALL run concurrently.

#### Scenario: Cancellation occurs during a file read
- **WHEN** a newer event cancels collection while a non-cancellable filesystem operation is executing
- **THEN** SessionTap waits for control to return, rejects the stale result, and starts no replacement collector concurrently

#### Scenario: Cancellation occurs between records
- **WHEN** a provider collector observes cancellation at a record boundary
- **THEN** it stops without returning a publishable partial snapshot

### Requirement: Artifact access remains authenticated, private, and bounded
Each provider implementation SHALL bind its locator to an authenticated configured provider, concrete adapter, provider agent-session, and eligible invocation. It SHALL enforce its own allowed provider storage roots, file identity requirements, read limits, and session-agreement checks. Paths, raw records, cursor internals, and diagnostics MUST NOT enter normalized snapshots, public output, sinks, or hub envelopes.

#### Scenario: Valid session artifact is collected
- **WHEN** a provider implementation verifies its session locator, storage root, file identity, and agent-session agreement
- **THEN** it reads only its bounded eligible data and returns typed normalized enrichment

#### Scenario: Locator escapes provider storage
- **WHEN** a locator resolves outside the selected provider's allowed storage or violates that provider's identity rules
- **THEN** collection fails open without changing hook processing or public state

### Requirement: Results remain bound to current authenticated sessions
Before applying collected enrichment, SessionTap SHALL revalidate the collection generation, configured provider, concrete adapter, provider agent-session, and target invocation binding. An invocation that changes provider agent-session SHALL detach from the prior collection key, and stale results SHALL NOT update it.

#### Scenario: Invocation changes provider session during collection
- **WHEN** collection for the old provider agent-session completes after the invocation binds a new provider agent-session
- **THEN** SessionTap discards the old result for that invocation

#### Scenario: Several invocations observe one provider session
- **WHEN** more than one authenticated invocation remains eligible for the same provider-qualified agent-session
- **THEN** SessionTap performs one collection and applies its normalized result only to bindings that still pass revalidation

### Requirement: Provider accounting remains provider specific and exact
Each provider module SHALL calculate session totals and current context according to verified semantics for its own records, use checked arithmetic, reject partial totals, and omit unverifiable values. Claude SHALL deduplicate stable response identities and account for verified cache input; Codex SHALL select its latest cumulative snapshot rather than sum snapshots; Qwen SHALL sum valid assistant usage while excluding duplicative telemetry.

#### Scenario: Duplicate Claude response rows exist
- **WHEN** Claude's session artifact repeats usage for one stable response identity
- **THEN** the Claude collector includes that response once according to Claude's accounting rules

#### Scenario: Codex has several cumulative snapshots
- **WHEN** a Codex session contains several valid cumulative usage snapshots
- **THEN** the Codex collector uses the latest valid snapshot and does not sum them

#### Scenario: Qwen telemetry duplicates assistant usage
- **WHEN** a Qwen session contains assistant usage and duplicative telemetry
- **THEN** the Qwen collector calculates totals only from its verified assistant records

### Requirement: Collection failures preserve verified state
Missing, malformed, oversized, unsupported, cancelled, or temporarily unreadable provider artifacts SHALL NOT fail hooks, lifecycle events, or provider execution. SessionTap SHALL preserve the last verified enrichment, emit no estimated or partial replacement, and expose only bounded local diagnostics.

#### Scenario: Provider record is malformed
- **WHEN** a provider-specific parser cannot produce an exact normalized result
- **THEN** SessionTap retains prior verified public state and records a bounded local diagnostic without artifact content or path disclosure

#### Scenario: Superseded scan accumulated partial totals
- **WHEN** a collector is cancelled after processing part of an artifact
- **THEN** it publishes none of that generation's partial enrichment
