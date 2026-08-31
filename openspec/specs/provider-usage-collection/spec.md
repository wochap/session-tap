# provider-usage-collection Specification

## Requirements

### Requirement: Superseded collection design is retired
The invocation-keyed, cross-provider usage collector and Claude statusline
integration formerly described by this capability SHALL NOT be implemented or
applied independently. Provider artifact enrichment SHALL instead follow the
`provider-session-collection` capability introduced by
`redesign-provider-session-collection`.

#### Scenario: An implementation references the old collection capability
- **WHEN** implementation or planning work encounters this retired capability
- **THEN** it uses provider-owned collectors, provider-qualified agent-session coordination, and hook/artifact-only evidence from the replacement capability

#### Scenario: A user has a legacy development wrapper
- **WHEN** a development installation still contains the superseded managed Claude wrapper
- **THEN** restoration is a documented one-time manual operation and no current runtime inspects or executes statusline configuration
