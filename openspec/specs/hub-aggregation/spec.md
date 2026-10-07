# Hub Aggregation

## Purpose

Define canonical multi-source ingestion, persistence, repair, and live observation for the SessionTap hub.

## Requirements

### Requirement: Hub ingests only canonical SessionTap envelopes
The hub SHALL accept canonical source snapshot and update envelopes produced by SessionTap sinks. Each envelope SHALL contain complete `PublicAgentView` values and no internal invocation snapshot, internal normalized event, or raw provider payload. The hub SHALL validate the canonical schema and SHALL NOT perform provider-specific normalization or reinterpret provider hooks. When per-source ingestion tokens are configured, the hub SHALL authenticate each ingestion request before interpreting its body and SHALL accept an envelope only when its source ID is bound to the presented token. The hub SHALL distinguish rejections by status code: 400 for a request line or headers it cannot parse, 401 for a missing or unrecognized bearer token, 403 for a valid token presented with an envelope whose source ID is not bound to it, 411 for a missing or non-numeric `content-length`, 431 for headers exceeding the header limit, and 413 only for a body exceeding the configured body limit. Every rejection body SHALL carry a structured error code, and a rejected request SHALL NOT change persisted state, publish to listeners or remote devices, or invoke subscriptions. When the hub rejects an envelope as `malformed_envelope` or `unsupported_schema_version`, it SHALL log a diagnostic naming the source ID when known and the validation reason, and SHALL include that reason as a string `detail` field in the rejection body.

#### Scenario: Canonical update arrives
- **WHEN** a source sends a valid public update containing source identity, delivery identity, changed public field paths, and complete resulting public agent view
- **THEN** the hub stores that public state without applying provider-specific transformations

#### Scenario: Private source state arrives
- **WHEN** an otherwise valid ingestion request contains multiplexer details, credentials, raw hooks, or another unrecognized field outside the public schema
- **THEN** the hub discards the unrecognized field and persists and forwards only its recognized canonical public projection

#### Scenario: Unknown or malformed envelope arrives
- **WHEN** an ingestion request omits or invalidates a required canonical public-envelope field
- **THEN** the hub rejects it without changing persisted state or invoking subscriptions

#### Scenario: Malformed envelope reason is reported
- **WHEN** the hub rejects an update because it does not change the public view
- **THEN** it responds 400 with error code `malformed_envelope` and a `detail` naming that reason, and writes a log line naming the source ID and the reason

#### Scenario: Future source adds optional public metadata
- **WHEN** a newer source includes an unrecognized optional public field while all required canonical fields remain valid
- **THEN** the hub accepts the envelope, ignores the unknown field, and does not echo it to listeners or commands

#### Scenario: Request is not parseable HTTP
- **WHEN** a connection sends bytes that do not form a request line and headers before the connection ends
- **THEN** the hub responds 400 with error code `malformed_request` and changes no state

#### Scenario: Content length is missing
- **WHEN** a request has a parseable request line and headers but no numeric `content-length`
- **THEN** the hub responds 411 with error code `length_required` rather than treating the body as empty

#### Scenario: Body exceeds the limit
- **WHEN** `content-length` or the received body exceeds the configured maximum body size
- **THEN** the hub responds 413 with error code `payload_too_large`

#### Scenario: Headers exceed the limit
- **WHEN** the request headers exceed the header size limit
- **THEN** the hub responds 431 with error code `headers_too_large`

#### Scenario: Request without a token reaches an authenticated hub
- **WHEN** source tokens are configured and a `POST` carries no bearer token or one matching no configured source token
- **THEN** the hub responds 401 with error code `unauthorized` without parsing the envelope, changing state, notifying devices, or running subscriptions

#### Scenario: Valid token writes another source
- **WHEN** a request presents the `sandbox` source's token with an envelope whose source ID is `host`
- **THEN** the hub responds 403 with error code `source_not_permitted`, leaves the `host` agents unchanged, and publishes nothing

### Requirement: Hub merges stable source identities
Each source SHALL have a configured stable ID and optional display name, and the hub SHALL identify an agent by the pair of source ID and invocation ID.

#### Scenario: Same invocation identifier appears from two sources
- **WHEN** host and sandbox sources publish the same invocation ID
- **THEN** the hub retains two distinct agents keyed by their respective source IDs

### Requirement: Hub persists the merged current state
The hub SHALL persist source metadata, current source revision, canonical `PublicAgentView` values, accepted delivery identities, and a monotonically increasing hub revision in SQLite. It SHALL NOT persist source-internal invocation snapshots or provider hook payloads.

#### Scenario: Hub restarts
- **WHEN** the hub restarts after accepting source state
- **THEN** it restores the merged public agent view before accepting consumers or subsequent updates

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
The hub SHALL provide a local command that emits one persisted merged public snapshot followed by one JSON object per accepted public update after the snapshot revision without a subscription gap. The same stream SHALL be available to paired remote devices through the remote `listen` method. Agents SHALL be identified by source ID and invocation ID.

#### Scenario: Status-bar listener starts
- **WHEN** a consumer runs `sessiontap-hub listen`
- **THEN** it receives complete public views for current agents from all sources and then receives live normalized public updates without polling

#### Scenario: Listener reconnects
- **WHEN** a live consumer reconnects after the hub or consumer restarts
- **THEN** it receives a new complete persisted public baseline before subsequent updates

#### Scenario: Multiple fields change in one source update
- **WHEN** one accepted update changes multiple projected fields
- **THEN** the hub listener receives the complete view and the full deterministic changed-field set from that update

#### Scenario: Remote device listens
- **WHEN** a paired remote device calls `listen`
- **THEN** it receives the same snapshot and update envelopes as a local `sessiontap-hub listen` consumer

### Requirement: Hub retains explicit current attention state
The hub SHALL retain only the optional bounded public status reason carried inside each complete `PublicAgentView`, including blocked `input`/`approval` and stopped `completed`/`failed` reasons. It SHALL replace or clear the prior reason whenever an accepted complete view replaces the materialized agent state and SHALL NOT infer a reason for a stopped view that omits one.

#### Scenario: Agent stops waiting for input
- **WHEN** an update changes an agent from blocked to running and its complete public view has no reason
- **THEN** the merged state and live update no longer expose the prior blocked reason

#### Scenario: Approval changes to ordinary input
- **WHEN** a blocked public view with an approval reason is replaced by a blocked view with an input reason
- **THEN** the hub retains only the input reason from the latest complete view

#### Scenario: Agent completes a response
- **WHEN** a stopped public view with a completed reason replaces a running or blocked view
- **THEN** the hub retains that completed reason and exposes it to listeners and subscription matching

#### Scenario: Process exits without completion context
- **WHEN** a stopped public view without a reason replaces a running, blocked, or idle view
- **THEN** the hub clears the prior reason and does not infer completed or failed

### Requirement: Hub relays agent terminals only for opted-in sources
The hub SHALL expose agent terminal viewing and input only as the scoped relay defined by the `hub-terminal-relay` capability, and only for sources that keep a control channel open. The hub SHALL NOT capture panes, run multiplexer commands, or write to terminals itself, and SHALL NOT store terminal content. Ingestion, merged state, listeners, subscriptions, and forget SHALL behave the same whether or not any terminal stream is open. Forgetting a stopped agent SHALL change only hub state and SHALL send nothing to the source daemon.

#### Scenario: Source without control channel
- **WHEN** a device asks to view or control an agent whose source never opened a control channel
- **THEN** the hub refuses without contacting the source and its merged state is unchanged

#### Scenario: Remote device forgets an agent
- **WHEN** a paired device forgets a stopped agent
- **THEN** the hub changes only its own state and sends nothing to the source daemon or the agent's terminal

### Requirement: Hub carries the public terminal descriptor
The optional `terminal` descriptor of `PublicAgentView` (its quick-pick mode) SHALL be part of the canonical public schema the hub ingests. The hub SHALL persist it with the agent's view, SHALL include it in `sessiontap-hub listen` and remote `listen` snapshots and updates, SHALL report `terminal` in changed field paths when it appears, changes, or disappears, and SHALL keep the key absent for agents whose source sent none. The descriptor SHALL be descriptive only: the hub SHALL NOT treat its presence as permission to open a terminal.

#### Scenario: Interactive agent in tmux
- **WHEN** a source sends a view whose `terminal` descriptor has quick-pick `digits`
- **THEN** remote `listen` consumers receive that agent with the same `terminal` descriptor

#### Scenario: Agent stops
- **WHEN** a later update removes the `terminal` descriptor
- **THEN** the hub persists the view without it and the pushed update lists `terminal` among the changed field paths

### Requirement: Hub forgets stopped agents with tombstones
The hub SHALL provide `sessiontap-hub forget <source_id> <invocation_id>`, served by the running service, that deletes a stopped agent from the merged state, records a tombstone for that source and invocation pair, increments the hub revision, and re-baselines every live listener with a fresh snapshot. The hub SHALL refuse to forget an agent that does not exist or whose status is not `stopped`. While a tombstone exists, the hub SHALL acknowledge updates for that pair without persisting them, publishing them, or evaluating subscriptions, and SHALL leave that pair out when it materializes a source snapshot. The hub SHALL delete tombstones older than twice `retention_days`.

#### Scenario: Forget a stopped agent
- **WHEN** the user runs `sessiontap-hub forget host 7f3c` for a stopped agent
- **THEN** the agent leaves the merged state and each `sessiontap-hub listen` consumer receives a new snapshot without it

#### Scenario: Forgotten agent is redelivered
- **WHEN** a source later sends an update or a complete snapshot that includes the forgotten invocation
- **THEN** the hub acknowledges the delivery, the agent stays absent, and no subscription command runs

#### Scenario: Forget a non-stopped agent
- **WHEN** the user forgets an agent whose status is `blocked`
- **THEN** the command reports that only stopped agents can be forgotten and exits non-zero without changing state

#### Scenario: New run of the same project
- **WHEN** the user starts the agent again, producing a new invocation ID
- **THEN** the hub ingests the new invocation normally

### Requirement: Hub binds ingestion tokens to source identities
The hub configuration SHALL support a `sources` map from source ID to a private token file. A token SHALL authorize writes only for the source IDs whose configured token file yields that token. The hub SHALL read token files at request time with the same private, non-symlink file rules used for other SessionTap credentials and SHALL compare tokens in constant time. A token file that is missing, a symlink, or readable by group or others SHALL authorize nothing. When `sources` is configured, every ingestion envelope SHALL name a configured source ID.

#### Scenario: Source writes its own state
- **WHEN** the `sandbox` source posts a snapshot with source ID `sandbox` and the bearer read from `sources.sandbox.token_file`
- **THEN** the hub applies the snapshot

#### Scenario: Unconfigured source ID
- **WHEN** a request presents a valid token for `sandbox` with an envelope whose source ID is `rogue`, which has no `sources` entry
- **THEN** the hub responds 403 with error code `source_not_permitted` and changes no state

#### Scenario: Token file loses its private mode
- **WHEN** a configured source's token file becomes group- or world-readable
- **THEN** requests presenting that token are rejected with 401 until the file is private again

#### Scenario: Token is rotated
- **WHEN** the operator replaces the contents of a source's token file while the hub runs
- **THEN** subsequent requests are authenticated against the new token without restarting the hub

### Requirement: Hub refuses unauthenticated non-loopback ingestion
The hub SHALL reject a configuration whose ingestion `listen` address is not a loopback address, including the wildcard addresses `0.0.0.0` and `::`, unless the `sources` map configures at least one source token. The validation error SHALL name the offending address, and the hub SHALL NOT bind that address. A loopback `listen` address without `sources` SHALL remain valid, and ingestion on it SHALL accept envelopes without a bearer token.

#### Scenario: Wildcard ingestion without tokens
- **WHEN** the hub configuration sets `listen: "0.0.0.0:8931"` and no `sources`
- **THEN** configuration validation fails with an error naming `0.0.0.0:8931` and the hub does not accept ingestion on that address

#### Scenario: Wildcard ingestion with tokens
- **WHEN** the hub configuration sets `listen: "0.0.0.0:8931"` and a `sources` map with token files for `host` and `sandbox`
- **THEN** the configuration is valid and every ingestion request must authenticate

#### Scenario: Loopback ingestion without tokens
- **WHEN** the hub configuration sets `listen: "127.0.0.1:8931"` and no `sources`
- **THEN** the configuration is valid and local sources deliver without a bearer token

### Requirement: Hub health probe reveals no state
The hub SHALL answer `GET /health` on the ingestion address with status 200 and the body `{"status":"ok"}` without requiring authentication, and the response SHALL NOT include the hub revision or any other stored state.

#### Scenario: Unauthenticated health probe
- **WHEN** any client sends `GET /health` to the ingestion address
- **THEN** the hub responds 200 with exactly `{"status":"ok"}`
