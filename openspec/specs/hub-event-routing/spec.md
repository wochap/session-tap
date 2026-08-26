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
Subscription match criteria SHALL support source ID, provider, event kind, public status, lifecycle, and repository, with different fields combined by logical AND and values within one field combined by logical OR.

#### Scenario: Multi-field rule matches
- **WHEN** an update is from source `sandbox`, provider `codex`, event `waiting_input`, and status `blocked`, and all are allowed by a subscription
- **THEN** that subscription matches the update

#### Scenario: One field does not match
- **WHEN** every configured criterion except source ID matches an update
- **THEN** that subscription does not run

### Requirement: Subscriptions filter material field changes
A subscription SHALL be able to require changes to one or more normalized fields, including status and attention, by comparing the previously persisted state with the accepted resulting state.

#### Scenario: Attention changes while status remains blocked
- **WHEN** a subscription watches `status` and `attention` and an accepted update changes only attention
- **THEN** the subscription runs with the new canonical envelope

#### Scenario: Unrelated enrichment arrives
- **WHEN** a subscription watches only status and attention and an update changes neither field
- **THEN** the subscription does not run

### Requirement: Commands receive canonical structured input
The hub SHALL execute configured commands directly as argument arrays without shell evaluation, SHALL provide the accepted canonical envelope on stdin, and SHALL expose documented scalar `SESSIONTAP_*` environment variables as conveniences rather than an alternative schema.

#### Scenario: Notification script runs
- **WHEN** a matching waiting-input update is accepted
- **THEN** the configured script can read source, provider, event, status, session identity, and attention from the canonical JSON on stdin

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
