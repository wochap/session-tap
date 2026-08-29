# Prompt: extract an agent/harness hooks and lifecycle API

Use this prompt from the root of the AI agent or harness repository whose hook
contract you want to document (for example Claude Code, Codex, Qwen Code, or a
similar project).

## Inputs

Set these values before running the prompt. A value of `AUTO` means infer it
from the repository and record the result.

```text
PRODUCT_OR_HARNESS: AUTO
VERSION_OR_REVISION: AUTO
OUTPUT_FILE: agent-hooks-lifecycle-api.md
PREVIOUS_DOCUMENTATION: NONE
SCOPE: hooks, lifecycle events, notifications, callbacks, plugins, and event side channels
```

`PREVIOUS_DOCUMENTATION` may be `NONE` or the path to a Markdown file produced
by an earlier run of this prompt.

## Prompt

You are performing a contract-focused repository investigation. Determine the
externally usable hooks, lifecycle events, notifications, callbacks, plugin
events, and event side channels exposed by this AI agent/harness. Document their
registration API, delivery semantics, and payload schemas in one Markdown file.

This is an evidence-gathering and documentation task. Do not modify product
source, tests, configuration, generated files, dependencies, or lockfiles. The
only permitted write is `OUTPUT_FILE`.

First read all repository instructions that apply to the current directory.
Inspect the repository broadly enough to find both documented and implemented
contracts. Search documentation, schemas and generated types, configuration
parsers, CLI help, hook registries, event enums, dispatchers, serializers,
fixtures, tests, examples, changelogs, and release/version metadata. Do not
assume the feature is named `hook`; also search for lifecycle, callback,
notification, event, plugin, middleware, listener, telemetry, protocol, and
side-channel concepts.

Use this evidence priority:

1. Published or repository-local public API documentation and machine-readable
   schemas.
2. Public CLI help, generated public types, configuration schemas, and official
   examples.
3. Contract and integration tests that exercise public behavior.
4. Runtime implementation details, used only to resolve gaps and clearly
   labeled as implementation-observed rather than publicly guaranteed.
5. Inference, used only when unavoidable and explicitly labeled.

Do not claim that a field, event, ordering rule, or guarantee exists without
evidence. Distinguish these confidence labels throughout the report:

- `documented`: explicitly part of a public contract;
- `tested`: asserted by a repository test or fixture;
- `observed`: present in implementation but not promised publicly;
- `inferred`: a reasoned interpretation that still needs verification;
- `unknown`: the repository does not establish the answer.

For every material claim, cite a repository-relative file path plus a symbol,
heading, test name, or line number when practical. Prefer stable symbols and
headings over line numbers. Never paste substantial source code. Describe API
facts and data shapes in original language. Record the repository license and
whether each important conclusion came from public-contract material or
implementation inspection, so downstream clean-room consumers can decide what
they may use.

Investigate all of the following when available:

- how hooks are enabled, registered, configured, trusted, disabled, and removed;
- supported configuration locations and precedence;
- event names and aliases, including deprecated or version-gated events;
- process/transport model: stdin, stdout, environment, argv, HTTP, socket,
  JSONL, transcript, log, plugin API, or another channel;
- common envelope fields and event-specific fields;
- required, optional, nullable, conditional, and omitted-field behavior;
- exact scalar/object/array types, enum values, aliases, and nesting;
- identifiers and correlation rules for session, conversation, turn, tool call,
  subagent, request, and event identities;
- timestamps, sequence numbers, event ordering, concurrency, duplication,
  retries, timeouts, and delivery guarantees;
- hook exit-code, stdout, stderr, response-payload, cancellation, mutation,
  permission, or allow/deny semantics;
- root-agent versus subagent behavior and parent/child correlation;
- session start/resume/clear/compact/end and turn start/stop/failure/interrupt;
- tool start/success/failure, approval requests, user questions, idle signals,
  notifications, usage/context data, model and permission metadata;
- payload size or string limits, security-sensitive fields, secrets, prompts,
  transcript paths, arbitrary tool input, and other privacy concerns;
- platform or version differences and known gaps.

Represent schemas precisely. Start with a common-envelope table, then add one
event table per event or one explicitly factored event-family table when events
share a schema. Each field row must contain:

```text
JSON path | type | presence | meaning | example/allowed values | evidence | confidence | sensitivity
```

Use `presence` values such as `required`, `optional`, `nullable`, or a concrete
condition. Preserve the distinction between a missing key and a key containing
`null`. If the repository supports multiple payload dialects, document each
accepted spelling and identify the canonical one. If exact payload shape is not
available, say so instead of fabricating a schema.

Include small synthetic JSON examples for important event families. Examples
must contain invented identifiers and harmless placeholder content. Do not copy
credentials, account data, real prompts, transcript contents, paths, or raw
captured payloads. Mark inferred fields in examples with an adjacent note; do
not make an inferred example look authoritative.

If `PREVIOUS_DOCUMENTATION` is not `NONE`, read it after independently
establishing the current contract. Compare it with current evidence and include
a change analysis containing:

- added, removed, renamed, or deprecated events;
- added, removed, renamed, retyped, or presence-changed fields;
- changed registration, transport, control-flow, ordering, or retry semantics;
- newly documented behavior that may not be a product change;
- earlier claims that are now contradicted or no longer verifiable;
- unchanged areas that were actually re-verified;
- migration impact for hook consumers.

Do not silently carry old claims forward. Label each change as `confirmed`,
`probable`, or `uncertain`, and distinguish an API change from a documentation
or evidence-quality change. When versions are unavailable, compare the two
repository revisions and state that limitation.

Write `OUTPUT_FILE` with exactly this top-level structure:

```markdown
# <product> hooks and lifecycle API

## Investigation metadata
## Executive summary
## Evidence and confidence model
## Hook registration and configuration
## Delivery and control-flow semantics
## Common payload envelope
## Event catalog
## Event payload schemas
## Correlation and lifecycle model
## Ordering, concurrency, retries, and failure behavior
## Security and privacy notes
## Version and platform compatibility
## Changes from previous documentation
## Unknowns and verification gaps
## Consumer implementation checklist
## Evidence index
```

Under `Event catalog`, include a compact table with event name, trigger,
transport, root/subagent scope, lifecycle meaning, confidence, and best evidence.
Under `Correlation and lifecycle model`, include a provider-event-to-neutral-
meaning table, but do not invent a universal state machine. Under `Consumer
implementation checklist`, call out which facts are safe to rely on and which
need feature detection or defensive parsing.

If no hook or lifecycle API exists, still create `OUTPUT_FILE`. Explain what
was searched, document the closest available mechanisms, and state that no
supported hook contract was found.

Before finishing, audit the report for internal contradictions, unsupported
certainty, accidentally copied source, unsafe sample data, and missing event
families. Confirm that every event in the catalog has either a payload schema or
an explicit `schema unavailable` entry. In your final response, report the
output path, identified product/version/revision, event count, whether a prior
document was compared, and the most important remaining unknowns.

