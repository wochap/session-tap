# Hub Event Routing

## Purpose

Define safe, deterministic routing of accepted SessionTap hub updates to configured local commands.

## Requirements

### Requirement: Hub loads versioned routing configuration
The hub SHALL read a versioned YAML configuration defining subscriptions as normalized match criteria, optional changed-field criteria, and one or more commands.

#### Scenario: Valid configuration loads
- **WHEN** the configuration contains supported subscription fields and commands
- **THEN** the hub activates those subscriptions

#### Scenario: Configuration is invalid
- **WHEN** the configuration contains an unsupported version, unknown field, or malformed command
- **THEN** the hub reports the error and does not silently run a partial or broadened rule set

### Requirement: Subscriptions match normalized agent data
Subscription match criteria SHALL support source ID, changed public field paths, and fields available in the canonical public envelope, including provider, public status, public reason kind, and repository. Supported public reason filters SHALL include `input`, `approval`, `completed`, and `failed`. Routing SHALL NOT depend on internal lifecycle, activity, normalized event kind, process control data, or multiplexer metadata. Different fields SHALL be combined by logical AND and values within one field by logical OR.

#### Scenario: Multi-field rule matches
- **WHEN** an update is from source `sandbox`, provider `codex`, includes changed field `status`, has status `blocked` and reason kind `input`, and all are allowed by a subscription
- **THEN** that subscription matches the update

#### Scenario: Completion-only notification matches
- **WHEN** a subscription requires status `stopped` and reason `completed` and an accepted update contains both
- **THEN** that subscription matches the response completion

#### Scenario: Lifecycle-only stop does not match completion
- **WHEN** a subscription requires reason `completed` and an accepted stopped view has no reason because only the process exited or was lost
- **THEN** that subscription does not match

#### Scenario: One field does not match
- **WHEN** every configured public criterion except source ID matches an update
- **THEN** that subscription does not run

#### Scenario: Unknown reason is configured
- **WHEN** routing configuration names a public reason other than `input`, `approval`, `completed`, or `failed`
- **THEN** configuration validation rejects it rather than silently broadening the subscription

#### Scenario: Internal criterion is configured
- **WHEN** routing configuration tries to match internal activity, event kind, multiplexer, or process metadata
- **THEN** configuration validation rejects the unsupported private criterion

### Requirement: Subscriptions filter material field changes
A subscription SHALL be able to require changes to one or more `PublicAgentView` fields, including status and reason, by comparing the previously persisted public view with the accepted complete resulting view.

#### Scenario: Reason changes while status remains blocked
- **WHEN** a subscription watches `status` and `reason` and an accepted update changes only the bounded reason
- **THEN** the subscription runs with the new canonical public envelope

#### Scenario: Unrelated enrichment arrives
- **WHEN** a subscription watches only status and reason and an update changes neither field
- **THEN** the subscription does not run

### Requirement: Commands receive canonical structured input
The hub SHALL execute configured commands directly as argument arrays without shell evaluation, SHALL provide the accepted canonical public envelope on stdin, and SHALL expose documented public scalar `SESSIONTAP_*` environment variables as conveniences rather than an alternative schema. Private source fields and internal event metadata SHALL not be available.

#### Scenario: Notification script runs
- **WHEN** a matching blocked-input public update is accepted
- **THEN** the configured script can read source, provider, changed fields, status, session identity, and bounded reason from canonical JSON on stdin

#### Scenario: Command contains shell metacharacters
- **WHEN** a configured argument contains spaces or shell metacharacters
- **THEN** the hub passes it as one literal process argument without shell interpretation

### Requirement: Routing follows accepted state exactly once per delivery identity
The hub SHALL evaluate subscriptions only after a new update is durably accepted and SHALL NOT evaluate them for rejected, stale, or transport-duplicate updates.

#### Scenario: Daemon retries an accepted event
- **WHEN** the same source and event ID are delivered more than once
- **THEN** each matching subscription is evaluated only for the first accepted delivery

### Requirement: Script failure does not retry ingestion
The hub SHALL acknowledge a valid durably accepted sink update independently of best-effort command results and SHALL report command failures locally.

#### Scenario: One command exits unsuccessfully
- **WHEN** an accepted event matches a command that exits nonzero
- **THEN** the hub logs the failure, retains the accepted state, and does not request daemon redelivery of the event
