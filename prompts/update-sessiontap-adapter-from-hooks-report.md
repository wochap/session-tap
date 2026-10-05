# Prompt: update a SessionTap provider adapter from an integration report

This is the implementer prompt. Run it from the root of the SessionTap
repository. It consumes the report written by `extract-agent-hooks-api.md`
and changes only the target provider's adapter.

## Inputs

```text
INTEGRATION_REPORT: REQUIRED_PATH_TO_MARKDOWN_FILE
TARGET_PROVIDER: AUTO
IMPLEMENT_CHANGES: YES
ALLOW_NEW_DEPENDENCIES: NO
SUBAGENT_POLICY: AUTO
```

`TARGET_PROVIDER` defaults to the provider named in the report's `Metadata`
section.

`SUBAGENT_POLICY` selects how subagent payloads are treated:

- `IGNORE`: reject subagent payloads before normalization;
- `RECORD_LINKED`: normalize subagent payloads as child-agent events through
  the existing `ChildAgentRef` field, so root state stays unaffected;
- `AUTO`: use `RECORD_LINKED` when the report's `Subagent identity` section
  gives a `documented` or acceptable `observed` identity, otherwise `IGNORE`.

## Scope

You may change only:

- `crates/sessiontap-adapters/src/<provider>.rs`;
- the provider's assets under `crates/sessiontap-adapters/assets/`;
- the provider's fixtures and snapshots under
  `crates/sessiontap-adapters/tests/`;
- `docs/providers/<provider>.md`, and the provider's row in
  `docs/compatibility.md`.

Any change outside that scope is a stop-and-ask decision. This includes
`crates/sessiontap-core`, shared code in `crates/sessiontap-adapters/src/lib.rs`,
`dialect.rs`, `driver.rs`, `artifact.rs`, the daemon, the public schema, and
OpenSpec specs. When a recommended change needs one, do not make it. Finish
the independent in-scope work, then describe the required core or schema
change and the decision the user must take.

## Steps

1. Read every repository instruction that applies, then `docs/clean-room.md`.
2. Read `INTEGRATION_REPORT` completely. Treat it as an evidence inventory,
   not a trusted specification. Note its version, license, confidence labels,
   and unknowns.
3. Check the report's `Field checklist` against the code it names. If the
   schema has changed since the report was written, list the difference and
   treat new fields as not covered by the report.
4. Read the current adapter module, its assets, fixtures, and provider doc.
5. Take each row of `Diff against current adapter`, and use `Options per
   field` and `Recommended mix` for context. Decide each row:
   implement, already supported, intentionally ignored, deferred, or blocked
   (by provenance, evidence, or scope).
6. Implement the rows decided as implement, with focused tests.
7. Update the provider doc and compatibility row where behavior changed.
8. Run the verification commands below and review the diff.

## Decision rules

1. Implement `documented` facts with acceptable public-contract evidence.
2. Implement `tested` or `observed` facts only when their provenance satisfies
   `docs/clean-room.md`. Label compatibility assumptions in the provider doc.
3. Never implement `inferred`, `unknown`, contradicted, or
   provenance-blocked behavior as fact. Parse defensively only when that
   cannot create false state transitions.
4. Prefer the report's primary source. Use a fallback only when the primary
   source has a version gate or enablement cost that the report flags.
5. Keep absence as absence. Never turn unknown usage, identifiers,
   timestamps, or metadata into zero, empty, or invented values.
6. Do not add dependencies when `ALLOW_NEW_DEPENDENCIES` is `NO`.

## Clean-room

Public provider documentation, public CLI or version output, and
independently observed behavior are acceptable evidence. Do not copy,
translate, or mechanically transform external source, tests, fixtures, or
scripts from a repository whose license is incompatible. A claim whose only
provenance violates `docs/clean-room.md` stays blocked. Never paste external
source into this repository.

## Subagents

Read the report's `Subagent identity` section before you touch filtering code.

- Compare the adapter's current subagent detection with the report's identity
  fields. A stale or over-broad check, for example treating an agent type
  alone as a subagent marker, is a discrepancy.
- Use the existing `ChildAgentRef`. Never invent a second parent-link field.
- With `RECORD_LINKED`, child events never change root status, reason, turn,
  session, metadata, or usage.
- With `IGNORE`, reject subagent payloads by their real identity fields and
  keep root payloads that only carry an agent type or name.
- Add fixtures for a root payload, a subagent start, a subagent event, and a
  root payload with an agent type but no subagent identity.

## Privacy and lifecycle

Raw hook payloads are transient. Do not keep or publish prompts, assistant
messages, transcripts, transcript paths, arbitrary tool input, credentials,
or raw payload objects. Use only bounded, sanitized fields, and keep the
escape removal and Unicode-safe bounds of the shared helpers.

- A completed or failed turn is not a process exit.
- A provider-session end is not the end of the wrapper invocation.
- An approval request and a user question are different waiting states.
- An event that only enriches metadata must not create activity.
- Late work must not reopen a finished turn without reliable new-turn
  evidence.

## Tests

Use small synthetic fixtures with invented IDs and harmless values. Cover
positive mappings and negative cases: missing or null fields, wrong types,
unknown events, subagent filtering, and bounding. Refresh fixture snapshots
with `UPDATE_SNAPSHOTS=1` only after you check the new output.

## Verification

Prefix every command with `rtk`.

```bash
rtk cargo fmt --all --check
rtk cargo clippy --workspace --all-targets --all-features -- -D warnings
rtk cargo test --workspace --all-features
```

Separate failures that existed before your edits from failures your edits
caused. Review the final diff for changes outside the scope, leaked raw data,
and over-broad normalization.

If `IMPLEMENT_CHANGES` is `NO`, change nothing and return the decision for
each diff row plus a patch and test plan.

## Final report

- Provider, and the report's product and version.
- Files changed.
- Each row of `Diff against current adapter` with its decision.
- The resolved `SUBAGENT_POLICY` and the identity field used.
- Required core or schema changes, as stop-and-ask decisions.
- Tests and verification run, with results.
- Remaining unknowns from the report's `Unknowns` section, and the evidence
  needed to resolve them.
