# Prompt: explore an agent's integration surfaces for SessionTap

This is the explorer prompt. It maps every integration surface of one AI agent
or harness (for example Claude Code, Codex, Qwen Code, or pi) to the data that
SessionTap's normalized schema needs. A small model can run it: follow the
steps in order and fill the fixed output template. The report it writes is the
input of `update-sessiontap-adapter-from-hooks-report.md`.

## Inputs

Set these values before running. `AUTO` means infer the value and record it.

```text
SESSIONTAP_REPO: REQUIRED_PATH_TO_SESSIONTAP_CHECKOUT
AGENT_REPO: .
TARGET_PROVIDER: AUTO
VERSION_OR_REVISION: AUTO
OUTPUT_FILE: agent-integration-report.md
PREVIOUS_REPORT: NONE
```

`TARGET_PROVIDER` is the SessionTap provider ID (`claude`, `codex`, `qwen`,
`pi`, or a new one). `PREVIOUS_REPORT` is `NONE` or the path to an earlier
report from this prompt.

## Rules

- Write only `OUTPUT_FILE`. Do not change either repository.
- Never paste source code. Describe facts and data shapes in your own words.
- Every claim needs evidence: a repository-relative path plus a symbol,
  heading, or test name.
- Label every claim with one confidence value:
  - `documented`: part of a public contract or public docs.
  - `tested`: asserted by a test or fixture in the agent repository.
  - `observed`: present in the implementation, not promised publicly.
  - `inferred`: your interpretation, still needs verification.
  - `unknown`: the repository does not answer it.
- Record the agent repository license. The implementer uses it with
  SessionTap's `docs/clean-room.md` to decide what it may use.
- Use invented values in every example. Never copy real prompts, paths,
  credentials, or captured payloads.

## Step 1: read the SessionTap schema from code

Do this before you look at the agent. Do not rely on a field list in this
prompt; the code is the source of truth, so new fields are picked up
automatically.

In `SESSIONTAP_REPO`, read these definitions and list every field they hold:

1. `crates/sessiontap-core/src/domain.rs`: `NormalizedEvent`, `EventKind`
   (every variant), `ProviderMetadata`, `Usage`, `ChildAgentRef`,
   `ToolActivityUpdate`, `ToolActivityPhase`, `StatusReasonContext`, and
   `ArtifactCollectionContext`.
2. `crates/sessiontap-adapters/src/lib.rs`: `SessionEnrichment` and the
   `AgentAdapter` trait.
3. `crates/sessiontap-adapters/src/dialect.rs`: the `HookDialect` trait. Each
   method is one thing an adapter can supply.
4. `crates/sessiontap-core/src/terminal.rs`: `TerminalPolicy`.
5. `crates/sessiontap-adapters/src/<TARGET_PROVIDER>.rs` and
   `docs/providers/<TARGET_PROVIDER>.md`, if they exist: what the current
   adapter subscribes to and maps today.

Write the result as the **field checklist**: one row per field or event kind,
with its type and one line on its meaning. Every later step refers to it.

## Step 2: inventory every integration surface

In `AGENT_REPO`, search for each surface below. Do not assume a feature is
called "hook". Also search for lifecycle, callback, listener, event, plugin,
extension, mod, middleware, telemetry, and protocol. For each surface record:
whether it exists, how it is enabled, its transport, its payload shape, and
evidence.

1. Hooks: config-file hooks, command hooks, HTTP hooks.
2. In-process extension points: function hooks, mods, plugins, extensions.
3. Transcripts and session files: location, format, record types.
4. Statusline or prompt-line commands and their input payloads.
5. SDK, RPC, JSON, or stream-JSON output modes.
6. OpenTelemetry or other telemetry exporters.
7. Environment variables and CLI flags that change any of the above, such as
   config or agent directories.
8. Side channels: log files, sockets, dual-output files, notifications.

## Step 3: build the coverage matrix

For every row of the field checklist, and for every surface from step 2, say
whether the surface can supply the field. Use the confidence labels. Note
version gates, for example "added in 2.1.251".

Pay special attention to:

- model: the selected model and the model that actually answered, and how a
  mid-session switch is reported;
- effort or thinking level, including an "off" value;
- usage and context: cumulative or per-turn, cache tokens, context window;
- turn boundaries: start, completion, failure, interrupt, idle;
- waiting states: approval versus user question;
- subagents: how a payload is recognized as a subagent, and which field links
  it to its parent.

## Step 4: rank the options per field

For each field with more than one possible surface, rank the options. Score
each option on:

- stability: public contract, versioned, or internal;
- enablement cost: none, setup writes config, user must trust or opt in;
- latency: live on the event, delayed until collection, or on exit;
- provenance: whether `docs/clean-room.md` allows its use, given the evidence
  class and license.

## Step 5: recommend a mix and diff it against the current adapter

Pick one primary source, and a fallback if useful, for each field. Then
compare the recommended mix with what the current adapter does (step 1,
item 5). List each difference as add, change, remove, or keep.

## Step 6: compare with the previous report

Skip this step when `PREVIOUS_REPORT` is `NONE`. Otherwise list added,
removed, or changed surfaces, events, and fields. Label each one `confirmed`,
`probable`, or `uncertain`. Do not carry old claims forward without checking
them again.

## Step 7: self-check

- Every field checklist row appears in the coverage matrix.
- Every surface in the catalog has evidence or says `unknown`.
- No source code is pasted and all examples use invented values.

## Output template

Write `OUTPUT_FILE` with exactly these sections, in this order. Write
`none found` in a section instead of leaving it out.

```markdown
# <product> integration report for SessionTap

## Metadata
Product, version or revision, license, target provider, SessionTap revision,
date, previous report compared (yes/no).

## Field checklist
| Field or event kind | Type | Meaning | Defined in |

## Surface catalog
| Surface | Exists | Enablement | Transport | Payload summary | Version gate | Confidence | Evidence |

## Coverage matrix
| Field | <surface 1> | <surface 2> | ... |
Each cell: `yes (<confidence>)`, `partial (<note>)`, or `no`.

## Options per field
For each field: a ranked list. Each option states surface, stability,
enablement cost, latency, provenance, and evidence.

## Subagent identity
How to tell a subagent payload from a root payload, and which field gives the
parent. Include confidence and evidence.

## Recommended mix
| Field | Primary source | Fallback | Reason |

## Diff against current adapter
| Change | Field or event | Current behavior | Recommended behavior | Evidence |
`Change` is one of add, change, remove, keep.

## Changes from previous report

## Unknowns
Each unknown with the evidence that would resolve it.

## Evidence index
| ID | Path | Symbol or heading | Class (public contract or implementation) |
```

In your final response, give the output path, the product and version, the
number of surfaces found, and the three most important unknowns.
