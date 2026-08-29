# Prompt: update SessionTap from a hooks/lifecycle report

Use this prompt from the root of the SessionTap repository. It consumes the
Markdown report produced by `extract-agent-hooks-api.md` and implements only
well-supported, privacy-preserving adapter improvements.

## Inputs

```text
HOOK_REPORT: REQUIRED_PATH_TO_MARKDOWN_FILE
TARGET_PROVIDER: AUTO
IMPLEMENT_CHANGES: YES
ALLOW_PUBLIC_SCHEMA_CHANGE: NO
ALLOW_NEW_DEPENDENCIES: NO
```

## Prompt

Use `HOOK_REPORT` as an evidence report to audit and, when justified, improve
SessionTap's provider integration. The goal is correct provider-hook setup and
defensive normalization into SessionTap's existing provider-neutral event,
state, reason, metadata, usage, and session model.

Read all applicable repository instructions before doing anything else. Then
read `HOOK_REPORT` completely. Treat it as an evidence inventory, not as an
automatically trusted specification. Note its product, version/revision,
license, confidence labels, evidence classes, unresolved unknowns, and any diff
from prior documentation.

This repository has a clean-room provenance policy. Read `docs/clean-room.md`
before planning changes and obey it strictly. Public provider documentation,
public CLI/version output, and independently observed behavior are acceptable
evidence. Do not copy, translate, or mechanically transform external source,
tests, fixtures, generated scripts, or implementation-specific trust logic from
a non-MIT or otherwise incompatible repository. Do not implement a claim whose
only provenance violates that policy. Instead, record it as blocked pending an
acceptable public contract or independent sanitized observation. Never paste
external source into this repository.

Also inspect, at minimum:

- `crates/sessiontap-adapters/src/lib.rs`;
- the target provider module under `crates/sessiontap-adapters/src/`;
- `crates/sessiontap-core/src/domain.rs`;
- target-provider fixtures and adapter tests;
- `docs/providers/<provider>.md`, `docs/compatibility.md`,
  `docs/security-review.md`, and relevant OpenSpec specs/changes;
- reducer/storage behavior when a proposed mapping could affect ordering,
  terminal-state deduplication, session generations, active reasons, or public
  projection.

Preserve unrelated user changes in the working tree. Do not rewrite files just
to match personal formatting preferences. Follow the repository's established
OpenSpec workflow if the proposed behavior changes an existing contract or
requires a new one.

Build an internal gap matrix before editing:

```text
provider event/field
-> evidence and confidence
-> current hook registration
-> current normalizer behavior
-> normalized EventKind/field/reason
-> reducer/public effect
-> discrepancy
-> proposed action
```

Evaluate at least these discrepancy classes:

- missing, obsolete, aliased, or incorrectly installed hook events;
- wrong event classification (`NewTurn`, `Working`, `Idle`, `WaitingInput`,
  `WaitingApproval`, `Completed`, `Failed`, `ProviderSessionStarted`,
  `ProviderSessionEnded`, `SessionEnded`, or `Enrichment`);
- confusion between turn completion, provider-session end, and process exit;
- missing interrupt/failure/idle handling;
- unsafe handling of delayed, duplicate, or out-of-order events;
- wrong root-agent/subagent filtering or parent/child correlation;
- missing or invalid session, turn, event, model, effort, permission, usage, or
  context fields;
- field aliases, optional/null behavior, malformed values, and bounds;
- status-reason selection for questions, approvals, completion, and failure;
- registration/config merge, trust, removal, and version compatibility issues;
- documentation or fixtures that no longer match supported behavior.

Apply these decision rules:

1. Implement `documented` facts supported by acceptable public-contract
   evidence when they improve correctness.
2. Implement `tested` or independently `observed` facts only when their
   provenance satisfies `docs/clean-room.md`; label compatibility assumptions.
3. Do not implement `inferred`, `unknown`, contradicted, or provenance-blocked
   behavior as fact. Feature-detect or parse defensively only when that cannot
   create false state transitions.
4. Prefer the smallest provider-specific change. Do not weaken shared behavior
   to accommodate one provider.
5. Keep absence as absence. Never turn unknown usage, identifiers, timestamps,
   or metadata into zero, empty, or fabricated values.
6. Do not change public wire/schema semantics when
   `ALLOW_PUBLIC_SCHEMA_CHANGE` is `NO`. If a justified fix requires one, stop
   that part and explain the required decision while completing independent
   safe work.
7. Do not add dependencies when `ALLOW_NEW_DEPENDENCIES` is `NO`.

Maintain SessionTap's privacy boundary. Raw hooks are transient. Do not retain
or publish complete prompts, assistant messages, transcripts, transcript paths,
arbitrary tool inputs, credentials, account data, process/control identities,
or raw payload objects. Only use the repository's explicitly selected bounded,
sanitized fields. Preserve escape/control-character removal, Unicode-safe
bounds, allowed failure categories, and optional-field behavior. Treat cwd,
repository paths, session names, model metadata, usage, and bounded reasons as
potentially sensitive even when allowed by the public schema.

Be conservative with lifecycle meaning:

- a completed or failed turn is not automatically a process exit;
- provider-session end is not automatically wrapper invocation end;
- an approval request and a user question are different waiting states;
- subagent events must not regress or block root-agent state unless SessionTap
  explicitly models that behavior;
- delayed work must not resurrect a terminal turn without reliable new-turn
  evidence;
- an event that only enriches metadata must not manufacture activity;
- provider timestamps and event IDs are used only when their format and
  semantics are supported; otherwise retain safe local receipt behavior.

When `IMPLEMENT_CHANGES` is `YES`, implement every safe, justified discrepancy
that fits the existing architecture. Add or update focused tests before calling
the task complete. Prefer small hand-authored synthetic fixtures that model the
public contract. Fixtures must use invented IDs and harmless values and must not
contain real prompts, paths, transcripts, commands, credentials, accounts, or
raw captured payloads. Cover positive mappings plus important negative cases:
missing/nullable fields, malformed types, aliases, subagent filtering,
unrecognized events, and privacy/bounding behavior where relevant.

Update provider documentation, compatibility/version notes, provenance notes,
and security documentation only where behavior or evidence changed. Do not
claim support merely because a parser accepts a shape; distinguish documented,
capture-backed, compatibility-only, and unverified behavior. If the input
report includes a change section, explicitly decide the disposition of each
confirmed API change: implemented, already supported, intentionally ignored,
deferred, or blocked by provenance/evidence.

Use repository-prescribed commands (including the required `rtk` prefix) for
searches and verification. At minimum, run formatting checks and the focused
adapter tests. Run broader workspace tests and lints in proportion to the scope
of the change. Do not hide pre-existing failures; separate them from failures
caused by your edits. Review the final diff for accidental public-schema
changes, leaked raw data, over-broad normalization, and unrelated edits.

If `IMPLEMENT_CHANGES` is `NO`, make no repository changes and return the gap
matrix and recommended patch/test plan. If it is `YES` but no safe justified
change exists, leave the repository unchanged and explain why.

Finish with a concise implementation report containing:

- provider and report version/revision;
- files changed;
- mappings or behaviors fixed;
- tests and validation run with outcomes;
- every report change classified as implemented, already supported,
  intentionally ignored, deferred, or provenance/evidence blocked;
- public-schema impact (`none` unless explicitly allowed);
- remaining unknowns and the exact evidence needed to resolve them.

