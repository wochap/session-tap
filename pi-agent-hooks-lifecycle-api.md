# Pi hooks and lifecycle API

## Investigation metadata

| Item | Value |
|---|---|
| Product | Pi (monorepo `pi-monorepo`; main package `@earendil-works/pi-coding-agent`) |
| Version | 1.0.2 (`packages/coding-agent/package.json`) |
| Revision | `cd32f7725fdbddbaecdff5b1e68491563394e0ca` ("Release v1.0.2"), committed 2026-10-04 |
| License | MIT, copyright Mario Zechner (`LICENSE`) |
| Investigation date | 2026-10-04 |
| Previous documentation | NONE (no change analysis) |
| Scope | hooks, lifecycle events, notifications, callbacks, plugins, event side channels |
| Method | Static reading of docs, types, dispatch code and tests. No code was executed. No repo file other than this one was written. |

Public-contract versus implementation sources:

- Public-contract material: `packages/coding-agent/docs/{extensions,json,rpc,rpc-commands,rpc-extension-ui,sdk,session-format,message-types,security,settings,configuration,packages,environment-variables,cli}.md`, and exported TypeScript declarations in `packages/coding-agent/src/core/extensions/types.ts` (which `docs/extensions.md` names as the authoritative event/context/result reference).
- Implementation-inspected (labeled `observed`): `src/core/extensions/runner.ts`, `src/core/agent-session.ts`, `src/core/agent-session-runtime.ts`, `src/core/sdk.ts`, `src/core/nested-tool-calls.ts`, `src/core/project-trust.ts`, `src/modes/**`, `packages/agent/src/types.ts`, `packages/durable/**`, `packages/protocol/**`, `packages/telemetry/**`.
- Licensing note for clean-room consumers: the repository is MIT, so reuse of code is permitted with notice. This document nonetheless describes shapes in original language.

## Executive summary

- Pi has no shell-command hook system (no `hooks.json`, no stdin/exit-code protocol). Its hook mechanism is the **extension API**: TypeScript modules loaded in-process that subscribe with `pi.on(event, handler)`. 41 event names are declared in the `ExtensionEvent` union (`types.ts`, `ExtensionEvent`).
- Extension handlers run in-process with full process permissions. They can observe, transform (`context`, `input`, `tool_result`, `message_end`, provider payload/headers), block (`tool_call`, `session_before_*`, `user_bash`), and request one continuation (`turn_end`, `agent_before_settle`).
- Out-of-process consumers use **side channels**: `--mode json` (JSONL on stdout, one session header then session events), `--mode rpc` (JSONL commands on stdin, responses/events on stdout), the in-process SDK `session.subscribe()`, and the session JSONL files. These carry a different (overlapping) event set from extension events.
- **No subagent concept exists in core.** No payload carries an agent id, agent type, depth, or parent agent id. Only an example extension (`examples/extensions/subagent`) and an experimental durable app implement subagents. Subagent detection and parent resolution from a single payload are `unknown`/not possible (see Subagent section). The only parent-style field in core is `parentToolCallId` for **nested tool calls** (a tool invoking another tool), which is not a subagent link.
- Telemetry (`@earendil-works/pi-telemetry`) is a contract only; Pi v1.0.2 emits no spans or metrics.
- Docs are incomplete relative to types in several places (for example `docs/json.md` omits `parentToolCallId`; see Unknowns).

## Evidence and confidence model

Labels used throughout: `documented` (public docs or exported declarations), `tested` (a repo test asserts it), `observed` (implementation only), `inferred`, `unknown`.

Note: the exported TypeScript declarations in `types.ts` are treated as `documented` because `docs/extensions.md` says to use them "for exact event, context, tool, and result types". Dispatch behavior read from `runner.ts`/`agent-session.ts` is `observed` unless a test is cited. Tests were only listed by name, not read in full; "tested" is applied only where a test name clearly matches the claim.

## Hook registration and configuration

**Registration API** (documented, `docs/extensions.md` "Create and load an extension"): an extension file exports a default factory `(pi: ExtensionAPI) => void | Promise<void>`. Inside it, `pi.on(eventName, handler)` returns an unsubscribe function (`types.ts` `ExtensionAPI.on`). Handlers have signature `(event, ctx: ExtensionContext) => result | void | Promise`. Other registration points: `registerTool`, `registerCommand`, `registerShortcut`, `registerFlag`, `registerProvider`, `registerMcpServer`, `registerVirtualModel`, renderers, and `pi.events` (event bus).

**Loading and locations** (documented, `docs/extensions.md`, `docs/configuration.md`, `docs/settings.md`, `docs/packages.md`; details from a subagent reading of those docs plus `package-manager.ts`):

- Auto-discovered: `~/.pi/agent/extensions/` (or `$PI_CODING_AGENT_DIR/extensions/`) and `<cwd>/.pi/extensions/`. Direct `.ts`/`.js` files and subdirectories with `index.ts`/`index.js`.
- Explicit: `pi -e/--extension <path|source>` (loads for one run). `-ne/--no-extensions` disables discovery (explicit `-e` still loads).
- Settings `extensions` array (and `packages[]` entries with per-resource filters) in `settings.json`. Filter syntax: `!pattern` exclude, `+path` force-include, `-path` force-exclude; omitted key loads all; `[]` loads none.
- Packages: `pi install|remove|update|list|config`; sources `npm:`, `git:`, URL, local path. `--local`/`-l` writes project scope.
- Built-in extensions are named `builtin:mcp`, `builtin:llama.cpp`, `builtin:codemode`, `builtin:tool-search`; disable with `-builtin:<name>` in `extensions` setting or `--no-extensions`.
- Precedence on name collisions (observed, `package-manager.ts`): project+settings entry, project+auto-discovered, user+settings entry, user+auto-discovered, package resource, built-in. Tools/flags/commands registered twice: first wins (tested: `extensions-runner.test.ts` "keeps first tool when two extensions register the same name", "keeps first flag..."); duplicate command names get suffixes (tested: "suffixes duplicate extension commands").
- Settings merge: global then project, deep merge, arrays replace (observed, `settings-manager.ts`); packages/resources from both scopes load (observed, `package-manager.ts`).
- Loader: `jiti` TypeScript loading (documented). Factory may be async and is awaited. Do not start long-lived resources in the factory (documented).

**Trust** (documented `docs/security.md`, `docs/extensions.md`): project-local resources (`.pi/settings.json`, `.pi/extensions`, skills, prompts, themes, `.pi/mcp.json`, `SYSTEM.md`, `APPEND_SYSTEM.md`) load only after project trust is resolved.

- Resolution order (observed, `src/core/project-trust.ts`; reported by subagent from `main.ts`): CLI `-a/--approve` or `-na/--no-approve`; if nothing trust-requiring exists, trusted; first extension `project_trust` handler returning `"yes"`/`"no"` wins (`"undecided"` falls through; tested: `extensions-runner.test.ts` "continues past undecided handlers and returns the first yes/no decision"); `<agentDir>/trust.json` nearest ancestor decision; `defaultProjectTrust` setting (`always|never|ask`, default `ask`; no UI means untrusted).
- Only personal and explicit command-line extensions can handle `project_trust` (documented). Built-ins load after trust resolves.
- `trust.json`: flat object `{ "<canonical dir>": true|false|null }`; `null` deletes (observed, subagent report).

**Disable/remove**: unsubscribe via the function returned from `pi.on`; extension runtime is invalidated on reload/session replacement (documented, `docs/extensions.md`). Tools cannot be unregistered; re-register with `exposure: "hidden"` (documented).

**Modes**: extensions load in interactive (`ctx.mode === "tui"`), RPC (`"rpc"`), JSON (`"json"`), print (`"print"`) (documented). `ctx.hasUI` true in TUI and RPC.

## Delivery and control-flow semantics

All claims `observed` from `runner.ts` unless otherwise labeled.

| Aspect | Behavior |
|---|---|
| Transport | In-process function call. No stdin/stdout/exit-code protocol for extension hooks. |
| Order | Handlers run sequentially, in extension load order, then registration order within an extension (documented: "Handlers run in extension load and registration order"). |
| Snapshot | Handler list is snapshotted per dispatch (`snapshotEventHandlers`); unsubscribe/register during dispatch does not affect the in-flight dispatch (documented; tested: `extensions-runner.test.ts` "keeps removed pending handlers in the current dispatch", "defers registrations made during dispatch until the next dispatch"). |
| Awaiting | Each handler is awaited before the next. Slow handlers delay the caller (documented for `provider_stream_event`). |
| Errors | Generic `emit`: handler exceptions are caught, reported to error listeners as `ExtensionError {extensionPath, event, error, stack?}`, and dispatch continues (tested: "calls error listeners when handler throws"). In RPC mode these appear on stdout as `extension_error`; in JSON/print mode on stderr as `Extension error (<path>): <msg>` (subagent report of `print-mode.ts`). |
| Fail-closed exceptions | `tool_call`: `emitToolCall` has no try/catch; a throw propagates and blocks the tool (documented: "A `tool_call` handler failure blocks the tool as a fail-safe"; `agent-session.ts` `_beforeToolCall` rethrows). `user_bash`: handler failure blocks the command rather than falling through (documented; tested "fails closed when a user_bash handler throws"). |
| Blocking | `tool_call` returns `{block?, reason?, terminate?}`; first handler returning `block: true` short-circuits. `session_before_*` handler returning `{cancel: true}` short-circuits. |
| Transform chaining | `tool_result`: each handler sees prior changes (tested: "tool_result chaining"). `input`: transforms chain, `handled` short-circuits (tested in `extensions-input-event.test.ts`). `message_end`: replacement must keep role, else error and ignored. `context`, `before_provider_request`, `before_provider_headers` chain. `before_agent_start`: results with `message` accumulate; `systemPrompt` sets `forceSystemPrompt` for later handlers. |
| Boundary events | `turn_end` and `agent_before_settle` (`emitBoundary`): handlers share and may replace `entries` (drafts of `custom`, `custom_message`, `context_edit`, `compaction`) and `continue`. Invalid previews are reported as errors and a later handler can repair (tested: "reports invalid boundary previews and lets later handlers repair the proposal"). Continuation requests are validated against runnable context (`canContinue`). |
| Mutation | `tool_call.input` is mutable in place, no re-validation (documented). `provider_stream_event` data must be treated read-only (documented). |
| Concurrency | Tool calls of one assistant message can run in parallel; do not assume siblings (documented). |
| Timeouts/retries | No dispatch-level timeout or retry for extension handlers (`unknown` if hidden elsewhere; none seen in `runner.ts`). UI dialogs accept `timeout` option. |
| Delivery guarantee | At-most-once, in-process, no persistence of extension events. Several are explicitly "not persisted" (`provider_stream_event`). Some `emit` calls are fire-and-forget: `thinking_level_select`, `session_info_changed` use `void emit(...)` (observed, `agent-session.ts`), so ordering relative to the caller is not awaited. |

**Side-channel semantics**: JSON/RPC event delivery is a stream with no ack; stdout must be read continuously or Pi can stall on a full pipe (documented, `docs/json.md`). Records are LF-delimited JSON (documented). Extension handlers for an agent event run **before** SDK/JSON/RPC listeners of the same event (observed, `_handleAgentEvent`: "Emit to extensions first, then notify public listeners").

## Common payload envelope

There is **no common envelope** for extension events: each event is an object with a `type` discriminator (string literal equal to the event name) plus event-specific fields; there is no session id, timestamp, sequence number, or agent id on the base. Session id and cwd are reachable via the handler's `ctx` (`ctx.sessionManager`, `ctx.cwd`), not in the payload. `ExtensionContext` fields (documented, `types.ts`):

| JSON path (ctx) | type | presence | meaning | evidence | confidence | sensitivity |
|---|---|---|---|---|---|---|
| `ctx.mode` | `"tui"\|"rpc"\|"json"\|"print"` | required | Run mode | `ExtensionMode` | documented | low |
| `ctx.hasUI` | boolean | required | Dialog-capable UI | `ExtensionContext` | documented | low |
| `ctx.cwd` | string | required | Working directory | same | documented | path disclosure |
| `ctx.model` | `Model \| undefined` | optional | Current model | same | documented | low |
| `ctx.thinkingLevel` | `ThinkingLevel` | optional | Level when runtime provides | same | documented | low |
| `ctx.signal` | `AbortSignal \| undefined` | nullable | Present only during streaming | same | documented | low |
| `ctx.sessionManager` | read-only manager | required | Entries, branch, header, session name | same | documented | high (full history) |
| `ctx.getContextUsage()` | `{tokens: number\|null, contextWindow: number, percent: number\|null} \| undefined` | method | Context usage | `ContextUsage` | documented | low |
| `ctx.isProjectTrusted()`, `ctx.isIdle()`, `ctx.hasPendingMessages()`, `ctx.getSystemPrompt()` | methods | required | State queries | same | documented | system prompt may be sensitive |

For the **JSON/RPC streams** there is also no envelope: each record is `{ "type": <string>, ...fields }` (documented, `docs/json.md`). Exceptions in JSON mode: the first record is the session header `{type:"session", version, id, timestamp, cwd, parentSession?}`. RPC responses use `{id?, type:"response", command, success, data?|error}` and UI requests use `{type:"extension_ui_request", id, method, ...}` (documented, `docs/rpc*.md`).

Timestamps: extension payloads mostly lack timestamps. `turn_start` has `timestamp` (epoch ms). Messages carry `timestamp` (epoch ms). Session entries carry ISO-8601 `timestamp`. No sequence numbers except `turnIndex`.

## Event catalog

Scope column: Pi has no subagents in core, so every event is **root-only (single-agent process)**. Events from a child `pi` process (example subagent extension) are visible only in that child's own process/stdout, never injected into the parent's hooks (observed, `examples/extensions/subagent/index.ts`). "Transport" is where a consumer can receive it.

### Extension events (`pi.on`) — 41

| Event | Trigger | Transport | Scope | Lifecycle meaning | Confidence | Best evidence |
|---|---|---|---|---|---|---|
| `project_trust` | Project has trust-requiring resources and trust unresolved | extension (personal/CLI extensions only) | root | Trust decision before project resources load | documented, tested | `ProjectTrustEvent`; `extensions-runner.test.ts` project_trust |
| `resources_discover` | After `session_start` (startup, reload) | extension | root | Extension supplies skill/prompt/theme paths | documented | `ResourcesDiscoverEvent` |
| `mcp_servers_change` | Extension (un)registers MCP server after bind | extension | root | Registry change | documented | `McpServersChangeEvent` |
| `session_start` | Session started/loaded/reloaded | extension | root | Session begin; `reason` startup/reload/new/resume/fork | documented, tested | `SessionStartEvent`; `agent-session-runtime-events.test.ts` |
| `session_info_changed` | Session name set/cleared | extension, JSON/RPC/SDK | root | Metadata change | documented | `SessionInfoChangedEvent` |
| `session_before_switch` | Before new/resume | extension | root | Cancellable | documented, tested | same test file |
| `session_before_fork` | Before fork | extension | root | Cancellable | documented, tested | same |
| `session_before_compact` | Before compaction | extension | root | Cancellable/customizable | documented | `SessionBeforeCompactEvent` |
| `session_compact` | Compaction succeeded | extension | root | Compaction end | documented | `SessionCompactEvent` |
| `session_compact_failed` | Compaction failed/aborted | extension | root | Compaction failure | documented | `SessionCompactFailedEvent` |
| `session_shutdown` | Quit, reload, session replacement | extension | root | Session end; cleanup point | documented, tested | `SessionShutdownEvent`; runtime events test |
| `session_before_tree` | Before tree navigation | extension | root | Cancellable | documented | `SessionBeforeTreeEvent` |
| `session_tree` | After tree navigation | extension | root | Leaf moved | documented | `SessionTreeEvent` |
| `context` | Before each LLM call | extension | root | Transform messages (no system) | documented | `ContextEvent` |
| `context_with_system` | Before each LLM call, after `context` | extension | root | Transform full transcript | documented | `ContextWithSystemEvent` |
| `cache_warming_decision` | Before prompt-cache refresh | extension | root | Override warm/stop | documented | `cache-warmer.ts` |
| `before_provider_request` | Before provider request | extension | root | Replace payload | documented | `BeforeProviderRequestEvent` |
| `before_provider_headers` | After headers assembled | extension | root | Mutate headers in place (`null` deletes) | documented, tested | `extensions-runner.test.ts` before_provider_headers |
| `after_provider_response` | Response received, before body | extension | root | HTTP status/headers | documented | `AfterProviderResponseEvent`; `sdk.ts` |
| `provider_stream_event` | Each parsed provider stream event | extension | root | Raw-ish stream observation | documented | `ProviderStreamEvent` |
| `before_agent_start` | After prompt submitted, before loop | extension | root | Run start; modify system prompt | documented, tested | `BeforeAgentStartEvent` |
| `agent_start` | Agent loop starts | extension, JSON/RPC/SDK | root | Low-level run start | documented | `AgentStartEvent`; `docs/json.md` |
| `agent_end` | Agent loop ends | extension, JSON/RPC/SDK | root | Low-level run end; not final | documented | `AgentEndEvent` |
| `agent_before_settle` | Before final settlement | extension | root | Last actionable boundary; may request one continuation | documented | `AgentBeforeSettleEvent` |
| `agent_settled` | No more automatic work | extension, JSON/RPC/SDK | root | Final idle/"done" signal | documented | `AgentSettledEvent`; `docs/json.md` |
| `ui_prompt_start` | Blocking extension UI prompt begins | extension | root | Waiting on user (approval/question) | documented | `UIPromptStartEvent` |
| `ui_prompt_end` | Prompt resolved | extension | root | Waiting ended | documented | `UIPromptEndEvent` |
| `turn_start` | Assistant turn starts | extension, JSON/RPC/SDK | root | Turn begin | documented | `TurnStartEvent` |
| `turn_end` | Turn ends (message plus tool results) | extension, JSON/RPC/SDK | root | Turn end; actionable boundary | documented | `TurnEndEvent` |
| `message_start` | Message begins (user/assistant/toolResult) | extension, JSON/RPC/SDK | root | | documented | `MessageStartEvent` |
| `message_update` | Assistant streaming delta | extension, JSON/RPC/SDK | root | | documented | `MessageUpdateEvent` |
| `message_end` | Message final | extension, JSON/RPC/SDK | root | May replace message | documented | `MessageEndEvent` |
| `tool_execution_start` | Tool begins running | extension, JSON/RPC/SDK | root | | documented | `ToolExecutionStartEvent` |
| `tool_execution_update` | Tool partial result | same | root | | documented | `ToolExecutionUpdateEvent` |
| `tool_execution_end` | Tool finished | same | root | | documented | `ToolExecutionEndEvent` |
| `model_select` | Model changed | extension | root | | documented | `ModelSelectEvent` |
| `thinking_level_select` | Thinking level changed | extension | root | | documented | `ThinkingLevelSelectEvent` |
| `user_bash` | User runs `!`/`!!` command | extension | root | Intercept/replace execution | documented, tested | `UserBashEvent` |
| `input` | User input received | extension | root | Transform/handle input | documented, tested | `InputEvent`; `extensions-input-event.test.ts` |
| `tool_call` | Before tool executes | extension | root | Permission gate; mutate input | documented | `ToolCallEvent` |
| `tool_result` | After tool executes | extension | root | Modify result | documented, tested | `ToolResultEvent` |

### Session events delivered only via JSON/RPC/SDK (not `pi.on`) — documented, `docs/json.md`

`queue_update`, `compaction_start`, `compaction_end`, `entry_appended`, `thinking_level_changed`, `auto_retry_start`, `auto_retry_end`, `summarization_retry_scheduled`, `summarization_retry_attempt_start`, `summarization_retry_finished`, `bash_execution_update` (RPC `bash` only), and RPC-only `extension_error`. (12 names.) Scope: root. `session_info_changed`, `agent_start/end/settled`, `turn_*`, `message_*`, `tool_execution_*` are shared with the extension list.

### Other channels

| Channel | Meaning |
|---|---|
| Session JSONL file | Append-only persisted entries (message, model_change, compaction, custom, ...). `docs/session-format.md`. |
| `pi.events` event bus | User-defined channels; core emits none (observed, `event-bus.ts`, `loader.ts`). |
| Experimental server/client protocol | `PROTOCOL_VERSION = 8`, CBOR, `PI_EXPERIMENTAL=1` (observed, `packages/protocol`). |
| Telemetry | Contract only; no emission (observed). |
| Bash-tool env vars | `PI_SESSION_ID`, `PI_SESSION_FILE`, `PI_PROVIDER`, `PI_MODEL`, `PI_REASONING_LEVEL` for LLM-issued shell commands; plus `AI_AGENT=pi`, `PI_CODING_AGENT=true` process markers (documented, `docs/environment-variables.md`). |

## Event payload schemas

Notation: `type` field always present as the event name (documented). TypeScript optional (`?`) means the key may be absent; presence of `undefined` values is dropped when JSON-serialized. Nullability stated where the declaration has `| null`. Source for all rows: `packages/coding-agent/src/core/extensions/types.ts` (declarations; confidence `documented`) unless stated. Sensitivity: **H** = may contain prompts, file content, secrets, tool arguments; **M** = paths/identifiers; **L** = low.

### Session lifecycle family

| JSON path | type | presence | meaning | allowed values | sens |
|---|---|---|---|---|---|
| `session_start.reason` | string | required | Why started | `startup\|reload\|new\|resume\|fork` | L |
| `session_start.previousSessionFile` | string | present for new/resume/fork | Prior session file path | path | M |
| `session_shutdown.reason` | string | required | | `quit\|reload\|new\|resume\|fork` | L |
| `session_shutdown.targetSessionFile` | string | optional | Destination on replacement | path | M |
| `session_before_switch.reason` | string | required | | `new\|resume` | L |
| `session_before_switch.targetSessionFile` | string | optional | | path | M |
| `session_before_fork.entryId` | string | required | Fork point | entry id | L |
| `session_before_fork.position` | string | required | | `before\|at` | L |
| `session_before_compact.preparation` | `CompactionPreparation` | required | Compaction plan | object (shape in `compaction/`; not enumerated) | H |
| `session_before_compact.branchEntries` | `SessionEntry[]` | required | Entries on branch | array | H |
| `session_before_compact.customInstructions` | string | optional | | | M |
| `session_before_compact.reason` | string | required | | `manual\|threshold\|overflow` | L |
| `session_before_compact.willRetry` | boolean | required | Aborted turn retried after | | L |
| `session_before_compact.signal` | `AbortSignal` | required | In-process only | not serializable | L |
| `session_compact.compactionEntry` | `CompactionEntry` | required | Saved entry | see session entry types | H (summary) |
| `session_compact.fromExtension` | boolean | required | | | L |
| `session_compact.reason`, `.willRetry` | as above | required | | | L |
| `session_compact_failed.reason`, `.aborted`, `.willRetry`, `.fromExtension` | string/boolean | required | | | L |
| `session_compact_failed.errorMessage` | string | optional | Non-abort failure text | | M |
| `session_before_tree.preparation` | `TreePreparation` | required | `{targetId, oldLeafId: string\|null, commonAncestorId: string\|null, entriesToSummarize, userWantsSummary, customInstructions?, replaceInstructions?, label?}` | | H |
| `session_before_tree.signal` | `AbortSignal` | required | | | L |
| `session_tree.newLeafId`, `.oldLeafId` | `string \| null` | required (nullable) | | | L |
| `session_tree.summaryEntry` | `BranchSummaryEntry` | optional | | | H |
| `session_tree.fromExtension` | boolean | optional | | | L |
| `session_info_changed.name` | `string \| undefined` | key present; value may be undefined | Cleared when undefined | | M |

Results: `session_before_switch -> {cancel?}`; `session_before_fork -> {cancel?, skipConversationRestore?}`; `session_before_compact -> {cancel?, compaction?: CompactionResult}`; `session_before_tree -> {cancel?, summary?{summary, details?, usage?}, customInstructions?, replaceInstructions?, label?}`.

### Agent / turn / message family

| JSON path | type | presence | meaning | sens |
|---|---|---|---|---|
| `before_agent_start.prompt` | string | required | Expanded user prompt | H |
| `before_agent_start.images` | `ImageContent[]` | optional | | H |
| `before_agent_start.systemPrompt` | string (getter) | required | Rendered system prompt | H |
| `before_agent_start.systemPromptOptions` | `NormalizedBuildSystemPromptOptions` | required | Mutable sections | H |
| `agent_start` | no fields | | | L |
| `agent_end.messages` | `AgentMessage[]` | required | Messages generated by the run | H |
| `agent_end.willRetry` | boolean | **JSON/RPC/SDK only, not on the extension event** (extension emit passes only `messages`; `agent-session.ts` `_emitExtensionEvent`; SDK adds it via `_willRetryAfterAgentEnd`) | observed |
| `agent_before_settle` / `turn_end` shared `BoundaryState` | | | |
| `.entries` | `SessionBoundaryDraft[]` | required | Proposals so far | M |
| `.continue` | boolean | required | Continuation requested so far | L |
| `.context` | `{contextEntries, contextMessages, llmMessages, pendingMessages, canContinue}` | required | Preview | H |
| `.outcome` | string | required | `completed\|aborted\|error` | L |
| `turn_start.turnIndex` | number | required | 0-based per run (reset on `agent_start`) | L |
| `turn_start.timestamp` | number | required | epoch ms | L |
| `turn_end.turnIndex` | number | required | | L |
| `turn_end.message` | `AgentMessage` | required | Assistant message | H |
| `turn_end.toolResults` | `ToolResultMessage[]` | required | | H |
| `turn_end.messageEntryId` | string | required | Persisted entry id of assistant message | L |
| `turn_end.toolResultEntryIds` | string[] | required | | L |
| `agent_settled` | no fields | | | L |
| `message_start.message` / `message_end.message` | `AgentMessage` | required | role user/assistant/toolResult/custom/system | H |
| `message_update.message` | `AgentMessage` | required (extension/SDK); **stripped on JSON/RPC wire** | Cumulative | H |
| `message_update.assistantMessageEvent` | `AssistantMessageEvent` | required | See sub-events below | H |
| `message_update.usage` | `Usage` | **wire only** | Latest cumulative usage | L |
| `ui_prompt_start/end.reason` | `"ui_prompt"` | required | | L |
| `ui_prompt_*.kind` | string | required | `select\|confirm\|input\|editor\|custom` | L |
| `ui_prompt_*.title` | string | optional (omitted when absent) | | M |

Results: `message_end -> {message?}` (same role). `before_agent_start -> {message?: {customType, content, display, details?}, systemPrompt?}`. `turn_end`/`agent_before_settle -> {entries?, continue?}`.

`AssistantMessageEvent` types (documented `docs/json.md`): `start`; `text_start|text_delta|text_end`; `thinking_start|thinking_delta|thinking_end`; `toolcall_start` (wire adds `id`, `toolName`), `toolcall_delta`, `toolcall_end{toolCall}`; `done{reason: stop|length|toolUse|deferred, message}`; `error{reason: aborted|error, error}`. Fields `contentIndex` on block events; `delta` on deltas; `content` on `*_end` of text/thinking.

### Tool family

| JSON path | type | presence | meaning | evidence | sens |
|---|---|---|---|---|---|
| `tool_call.toolCallId` | string | required | Model-issued id, or `<parent>/<n>` for nested | `ToolCallEventBase` | L |
| `tool_call.parentToolCallId` | string | present only for nested calls (key omitted otherwise) | Tool that invoked this one | same; `_beforeToolCall` spreads conditionally | L |
| `tool_call.toolName` | string | required | built-ins: `bash, powershell, read, edit, write, grep, find, ls`; or custom | | L |
| `tool_call.input` | object | required, mutable | Tool arguments | | **H** |
| `tool_result.toolCallId`, `.parentToolCallId`, `.toolName`, `.input` | as above | required / conditional | | `ToolResultEventBase` | H |
| `tool_result.content` | `(TextContent\|ImageContent)[]` | required | | | H |
| `tool_result.structuredContent` | JSON | optional | when `outputSchema` declared | | H |
| `tool_result.details` | tool-specific or undefined | required key | | | H |
| `tool_result.isError` | boolean | required | | | L |
| `tool_result.usage` | `Usage` | optional | Own plus nested usage | | L |
| `tool_execution_start.{toolCallId, toolName, args}` | string,string,any | required | | | H |
| `tool_execution_update.{...,partialResult}` | any | required | | | H |
| `tool_execution_end.{toolCallId, toolName, result, isError}` | | required | | | H |
| `tool_execution_*.parentToolCallId` | string | optional; **set for nested calls**. For top-level calls in the extension channel the field is absent (observed, `_emitExtensionEvent` builds events without it). | | `types.ts`, `agent-session.ts` | L |

Results: `tool_call -> {block?, reason?, terminate?}`; `tool_result -> {content?, details?, structuredContent?, isError?, usage?}` (replacing `content` without `structuredContent` drops it; tested via runner behavior).

### Provider family

| Event / path | type | presence | sens |
|---|---|---|---|
| `before_provider_request.payload` | unknown | required; handler return replaces | H |
| `before_provider_headers.headers` | `Record<string, string\|null>` | mutate in place; `null` deletes; return ignored | **H (auth headers)** |
| `after_provider_response.status` | number | required | L |
| `after_provider_response.headers` | `Record<string,string>` | required | M |
| `provider_stream_event.{provider, api, model, data}` | string, string, string, unknown | required; `data` read-only | H |
| `cache_warming_decision.{warmCost, missCost, continuationProbability, action}` | number x3, `"warm"\|"stop"` (action) | required; result `{action?}` last wins | L |

### Other

| Event / path | type | presence |
|---|---|---|
| `model_select.{model, previousModel, source}` | `Model`, `Model\|undefined`, `"set"\|"cycle"\|"restore"` | required (previousModel may be undefined) |
| `thinking_level_select.{level, previousLevel}` | ThinkingLevel | required |
| `user_bash.{command, excludeFromContext, cwd}` | string, boolean, string | required; result `{operations}` or `{result}` |
| `input.{text, images?, source, streamingBehavior?}` | string, ImageContent[], `"interactive"\|"rpc"\|"extension"`, `"steer"\|"followUp"` | `streamingBehavior` undefined when idle; result `{action:"continue"}`/`{action:"transform", text, images?}`/`{action:"handled"}` |
| `context.messages`, `context_with_system.messages` | `AgentMessage[]` | required; result `{messages?}` |
| `resources_discover.{cwd, reason}` | string, `"startup"\|"reload"` | result `{skillPaths?, promptPaths?, themePaths?}` |
| `project_trust.cwd` | string | result `{trusted: "yes"\|"no"\|"undecided", remember?}` |
| `mcp_servers_change.servers` | `RegisteredMcpServer[]` | required |

### JSON/RPC-only events (documented, `docs/json.md`; types in `agent-session.ts` `AgentSessionEvent`)

| Event | Fields |
|---|---|
| `queue_update` | `steering: string[]`, `followUp: string[]` (full queues) |
| `compaction_start` | `reason: manual\|threshold\|overflow` |
| `compaction_end` | `reason`, `result` (object; **key absent when undefined**), `aborted: boolean`, `willRetry: boolean`, `errorMessage?` |
| `entry_appended` | `entry: SessionEntry` (also emitted for cache-warm usage entries and context edits, per code; docs mention only `pi.appendEntry`) |
| `thinking_level_changed` | `level` |
| `auto_retry_start` | `attempt`, `maxAttempts`, `delayMs`, `errorMessage` |
| `auto_retry_end` | `success`, `attempt`, `finalError?` |
| `summarization_retry_scheduled` | `attempt`, `maxAttempts`, `delayMs`, `errorMessage` |
| `summarization_retry_attempt_start` | `source: "compaction"` plus `reason`, or `source: "branchSummary"` (no reason) |
| `summarization_retry_finished` | none |
| `bash_execution_update` (RPC `bash` only) | `delta: string`, `id?: string` |
| `extension_error` (RPC stdout only) | `extensionPath`, `event`, `error` (strings); `stack` not documented on the wire |

### Session header and entries (JSON mode first record; session file) — documented `docs/session-format.md`, observed `session-manager.ts`

| Path | type | presence | notes |
|---|---|---|---|
| `type` | `"session"` | required | |
| `version` | number | optional (v1 files lack); current 3 | |
| `id` | string | required | uuidv7 by default (observed), docs say "UUID" |
| `timestamp` | ISO-8601 | required | |
| `cwd` | string | required | |
| `parentSession` | string | optional, key omitted when none | **Path of parent session file**, set by fork/clone/`newSession({parentSession})`; not an id |

Entries: base `{type, id (8-hex), parentId: string|null, timestamp (ISO)}`; types `message, thinking_level_change, model_change, usage, compaction, branch_summary, custom, custom_message, context_edit, label, session_info` (observed `session-manager.ts`; fields per subagent reading).

### Usage and message types (documented `docs/message-types.md`, `ai/src/types.ts`)

`Usage`: `input, output, cacheRead, cacheWrite, totalTokens` (numbers, required); `cacheWrite1h?`, `reasoning?` (included in `output`); `cost {input, output, cacheRead, cacheWrite, total}` USD. `AssistantMessage`: `role, content, api, provider, model, usage, stopReason (pending|stop|length|toolUse|error|aborted|deferred), timestamp` required; `responseId?`, `responseModel?`, `thinkingLevel?`, `errorMessage?`, `diagnostics?` etc. optional. `ToolResultMessage`: `toolCallId, toolName, content, isError, timestamp` required; `details?`, `usage?`, `nestedCalls? {calls: [{id, name, arguments?, argumentsBytes?, status: ok|error|unfinished, durationMs?, error?}], complete: boolean}`. Limits: 256 calls, 8 KiB args per call, 32 KiB args total (documented in `docs/extensions.md`).

### RPC records (documented `docs/rpc*.md`; types `src/modes/rpc/rpc-types.ts`)

- Commands (stdin): `{type, id?, ...}`; set includes `prompt, steer, follow_up, abort, clear_queue, new_session{parentSession?}, get_state, get_messages, get_commands, set_model, cycle_model, set_thinking_level, compact, set_auto_compaction, set_auto_retry, abort_retry, bash, abort_bash, get_session_stats, export_html, switch_session, fork, clone, get_entries, get_tree, set_session_name, ...`.
- Response: `{id?, type:"response", command, success: true, data?}` or `{..., success:false, error}`. Parse failure: `{type:"response", command:"parse", success:false, error}` with no `id`.
- `get_state.data`: `model?`, `thinkingLevel`, `isStreaming`, `isCompacting`, `steeringMode`, `followUpMode`, `sessionFile?`, `sessionId`, `sessionName?`, `autoCompactionEnabled`, `messageCount`, `pendingMessageCount`.
- `extension_ui_request`: `{type, id (uuid), method}`; methods `select{title, options, timeout?}`, `confirm{title, message, timeout?}`, `input{title, placeholder?, timeout?}`, `editor{title, prefill?}`, `notify{message, notifyType?}`, `setStatus{statusKey, statusText}`, `setWidget{widgetKey, widgetLines, widgetPlacement?}`, `setTitle{title}`, `set_editor_text{text}`. Reply: `{type:"extension_ui_response", id, value | confirmed | cancelled:true}`. Unknown id ignored (observed). This is the approval/question side channel for RPC clients.

### Synthetic examples

Extension event as seen by a handler (invented values):

```json
{ "type": "tool_call", "toolCallId": "call_demo1", "toolName": "bash", "input": { "command": "echo hi" } }
```

Nested tool call (a tool invoked another tool):

```json
{ "type": "tool_execution_start", "toolCallId": "call_demo1/1", "parentToolCallId": "call_demo1", "toolName": "read", "args": { "path": "demo.txt" } }
```

JSON-mode stream excerpt:

```json
{"type":"session","version":3,"id":"00000000-0000-7000-8000-000000000000","timestamp":"2026-01-01T00:00:00.000Z","cwd":"/work/demo"}
{"type":"agent_start"}
{"type":"turn_start"}
{"type":"tool_execution_start","toolCallId":"call_demo1","toolName":"bash","args":{"command":"echo hi"}}
{"type":"tool_execution_end","toolCallId":"call_demo1","toolName":"bash","result":{"content":[{"type":"text","text":"hi"}],"details":{}},"isError":false}
{"type":"turn_end","message":{"role":"assistant"},"toolResults":[]}
{"type":"agent_end","messages":[],"willRetry":false}
{"type":"agent_settled"}
```

## Correlation and lifecycle model

Identifiers:

| Identity | Where | Notes |
|---|---|---|
| Session id | JSON header `id`; RPC `get_state.sessionId`; `ctx.sessionManager`; bash env `PI_SESSION_ID` | Not on event payloads. uuidv7 default (observed). |
| Session file | `PI_SESSION_FILE`; `get_state.sessionFile`; `session_start.previousSessionFile` | Absent for ephemeral sessions. |
| Turn | `turn_start.turnIndex`/`turn_end.turnIndex` | Per-run index, reset at `agent_start`; no global turn id. |
| Message | Session entry id (`turn_end.messageEntryId`, `toolResultEntryIds`) | 8-hex, unique within a session file. |
| Tool call | `toolCallId` (provider-assigned id; `<parent>/<n>` for nested) | Correlates `tool_call`, `tool_execution_*`, `tool_result`, and the toolResult message's `toolCallId` (documented). |
| Request/event | none | No event ids or sequence numbers. |
| Fork parent | header `parentSession` | File path. |

Provider-event to neutral meaning (not a universal state machine):

| Pi event | Neutral meaning |
|---|---|
| `session_start` | session begin (`reason` distinguishes new/resume/fork/reload/startup) |
| `session_shutdown` | session end |
| `input` / `before_agent_start` | user prompt submitted |
| `agent_start` | run start |
| `turn_start` / `turn_end` | model turn start / end |
| `tool_call` | tool about to run (permission point) |
| `tool_execution_start/end`, `tool_result` | tool start / success-or-failure (`isError`) |
| `ui_prompt_start` / `ui_prompt_end` | waiting on user (approval/question) begin / end |
| `agent_end` | run ended; may retry (`willRetry` on wire) |
| `agent_settled` | idle; no automatic continuation |
| `session_before_compact` / `session_compact` / `session_compact_failed` | compaction begin / success / failure |
| `auto_retry_start` / `auto_retry_end` | transient-error retry |
| `model_select`, `thinking_level_select` | model/effort change |
| `after_provider_response` | provider HTTP status |
| `message_end` (assistant) `.message.usage` | usage per response; `tool_result.usage` per tool |
| `ctx.getContextUsage()` | context size (pull, not pushed) |

Interrupt: an abort yields assistant `stopReason: "aborted"` and `ctx.signal` abort; `turn_end.outcome` / `agent_before_settle.outcome` is `completed|aborted|error`. There is no dedicated `turn_failed` or `interrupt` event (observed/inferred).

Permission and model metadata: no permission-mode field exists in payloads. Tool-hint annotations (`readOnlyHint` etc.) are reachable via `pi.getAllTools()` (documented). Model is `ctx.model`, `model_select`, and `AssistantMessage.{provider,model,api}`.

## Subagent identity and parent correlation

**Finding (observed; absence established by search):** Pi core has no subagent feature. A search of `packages/coding-agent/src`, docs, `packages/agent/src`, `packages/ai/src` found none outside `src/experimental/durable/`. Implementations that exist:

1. `packages/coding-agent/examples/extensions/subagent/` — an **example extension** registering a `subagent` tool. It spawns a child `pi` process with `--mode json -p --no-session` (plus optional `--model`, `--thinking`, `--tools`, `--append-system-prompt <tmpfile>`), final argument `Task: <task>`, inheriting `process.env`. It sets no env var, no parent id, no depth marker. The parent reads child stdout JSONL, consumes only `message_end` (and a dead `tool_result_end` branch), and surfaces progress as `onUpdate` on the subagent tool's own `toolCallId`. Limits: 8 parallel tasks, concurrency 4, no depth limit. Confidence: observed.
2. `packages/coding-agent/src/experimental/durable/subagent.ts` — **experimental** durable app (not the normal `pi` CLI). Child conversation created with ownership by the calling task; child cannot delegate (subagent extension removed from child). Events inside durable harness carry no parent id on `tool_execution_*`; structural links `owner`, `parent`, `byTaskId` exist only in its storage model (observed, subagent reading; not in hook/JSON payloads).

### Decision procedure for a single raw payload

1. Read `type`. All Pi payloads, from every channel, are produced by one agent per process.
2. Look for any of: `agentId`, `agent_id`, `agentType`, `parentAgentId`, `depth`, `isSubagent`, `rootSessionId`. **None of these exist in any Pi payload** (observed by absence). Do not branch on them.
3. Therefore a payload cannot be classified as root versus subagent from its content. Treat every payload as belonging to the process that emitted it. If you launched that process yourself (for example a child `pi --mode json`), the process boundary is the only discriminator, and you must record the parent relation out-of-band when you spawn it.
4. `parentToolCallId` is **not** a subagent marker. It means a tool invoked another tool via `ctx.executeTool()` inside the same process and session (documented, `docs/extensions.md`). It points to the immediate calling tool call.
5. Parent identity: not available. Closest substitutes: (a) for the example subagent extension, the parent's `toolCallId` of the `subagent` tool call that owns the child stdout stream (`inferred`: derived from process ownership; nothing in the child's payloads states it); (b) for forked sessions, header `parentSession` path (`documented`, but it is a fork/clone link, not an agent-spawn link). Label: `unknown` for true parent resolution.

### Identity and parent-link fields

| Field | Present on which events | Root value | Subagent value | Points to | Confidence | Evidence |
|---|---|---|---|---|---|---|
| (agent id / type / depth) | none | n/a | n/a | n/a | unknown (absent) | grep; `types.ts` |
| `parentToolCallId` | `tool_call`, `tool_result`, `tool_execution_*` | key omitted | not applicable (nested tool call, same agent) | immediate calling tool | documented | `docs/extensions.md`; `nested-tool-calls.test.ts` |
| `toolCallId` `<parent>/<n>` | same events | provider id | nested: `<callerId>/<n>`, deeper `a/1/1` | encodes ancestry path | observed, tested | `nested-tool-calls.ts`; `nested-tool-calls.test.ts` |
| header `parentSession` | JSON-mode session header; session file | key omitted | fork/clone only | parent session file path (immediate) | documented | `docs/session-format.md` |
| `session_start.previousSessionFile` | `session_start` (new/resume/fork) | absent on startup | n/a | previous file | documented | `SessionStartEvent` |
| `PI_SESSION_ID` env | bash tool children only | current session id | stripped and replaced in nested pi (child gets own) | not a parent link | documented | `docs/environment-variables.md` |
| `AI_AGENT=pi`, `PI_CODING_AGENT=true` | process env | set | inherited by children | marker only, no parent id | documented | same |

Shared state: example child has its own process, its own (non-persisted, `--no-session`) session, loads extensions/settings normally (inferred; no `--no-extensions` passed), inherits env and uses `cwd` from the tool params or parent cwd. A child can load the same subagent extension, so recursion is possible; no guard exists (inferred, untested).

### Events by agent scope

| Events | Root only | Subagent only | Both |
|---|---|---|---|
| All 41 extension events, all JSON/RPC events | yes (per process) | none defined | none |
| In the parent process, events about a child | only `tool_call`/`tool_execution_start|update|end`/`tool_result` for the `subagent` tool itself; child's own tool events are never forwarded | | |

Interleaving: parallel subagent tasks (up to 4 concurrent) run as separate processes; the parent sees only update snapshots on one `toolCallId`. A subagent cannot outlive the parent tool call in the example (abort sends SIGTERM then SIGKILL after 5 s; observed). In the durable experiment, the child conversation can outlive the call (observed). Usage attribution: child usage is summed into the parent tool result `details` by the example; the core does not attribute it (observed). Identifier uniqueness: session ids uuidv7; entry ids 8-hex unique within a file; tool call ids provider-defined. Resume of subagents: not applicable (`--no-session`).

Synthetic example (inferred correlation, not provided by Pi):

```json
{"type":"tool_execution_start","toolCallId":"call_root7","toolName":"subagent","args":{"agent":"scout","task":"demo"}}
{"type":"tool_execution_update","toolCallId":"call_root7","toolName":"subagent","partialResult":{"details":{"results":[{"agent":"scout","task":"demo"}]}}}
```

The first line is a root event. There is no subagent-start event and no subagent event with a resolvable parent link; the second line is the only parent-side trace, keyed by `call_root7` (inference: correlation by the parent tool call, `inferred`). The child's own stream (separate process stdout) would contain ordinary events with no parent field.

## Ordering, concurrency, retries, and failure behavior

- Documented run shape (`docs/json.md`): `agent_start`, `turn_start`, user `message_start/end`, assistant `message_start`, `message_update`*, `message_end`, `turn_end`, `agent_end`, `agent_settled`. `agent_end` does not mean finished: automatic retry, overflow recovery, compaction retry, steering, or follow-up can continue; `agent_settled` is the terminal signal.
- `agent_before_settle` precedes `agent_settled`; a handler may request one continuation (documented). Unconditional continuation can loop (documented warning).
- Extension handlers run before external listeners for the same agent event (observed). `entry_appended` for persisted drafts is emitted after the handlers (observed).
- Tool calls from one assistant message may overlap (documented); `executionMode` per tool can force sequential.
- Retries: auto-retry for transient provider errors emits `auto_retry_start/end` (settings `retry.enabled/maxRetries`, observed); `agent_end.willRetry` signals pending retry on the wire. Compaction retry emits `summarization_retry_*`. Max attempts shown in payload (`maxAttempts`).
- Extension handler failures: reported, not retried; `tool_call`/`user_bash` fail closed. Compaction cancellation by extension throws "Compaction cancelled" internally (observed).
- Fire-and-forget emits (`thinking_level_select`, `session_info_changed`) can interleave with later events (observed).
- Stdout backpressure: a stalled JSON/RPC reader can stall Pi (documented).
- JSON mode exit code is 0 even when assistant stopReason is `error`/`aborted` (documented, `docs/cli-integration.md`).
- No duplicate-delivery guarantees, no sequence numbers; consumers must tolerate `message_end` replacement by extensions and an absent `result` key in `compaction_end`.

## Security and privacy notes

- Extensions run in-process with full OS permissions and can read prompts, tool args, files, credentials, session history (documented, `docs/extensions.md`, `docs/security.md`). Trust gating applies to project-local resources, not to personal ones.
- Sensitive payload fields: `before_provider_headers.headers` (API keys, auth), `before_provider_request.payload`, `provider_stream_event.data`, `before_agent_start.prompt/systemPrompt`, `tool_call.input`, `tool_result.content`, `agent_end.messages`, `session_before_compact.branchEntries`, `ctx.sessionManager`, `PI_SESSION_FILE` paths.
- `tool_result` redaction: replace both `content` and `structuredContent` (documented).
- JSON/RPC streams contain full message content; treat as sensitive. Notification examples (`examples/extensions/notify.ts`) write terminal escapes to stdout; behavior in JSON/RPC mode unverified (stdout reserved for JSONL).
- Telemetry: install ping to `pi.dev/api/report-install` (opt-out via `PI_TELEMETRY` or setting `enableInstallTelemetry`; skipped when `PI_OFFLINE`); provider attribution headers added when telemetry enabled; an `x-opencode-session` header carrying the session id is sent to opencode models whenever a session id exists (observed, subagent report, `provider-attribution.ts`).
- Nested call records omit results and cap sizes (documented).
- Payload size limits: protocol frames default 16 MiB (experimental protocol, observed); nested-call limits above; no general string limit for hook payloads (unknown).
- Env vars `PI_SESSION_*` are removed from inherited env before injection into bash commands; `exposeSessionEnvironment:false` also removes them (documented).

## Version and platform compatibility

- Version 1.0.2. Session format v3 (v1 linear, v2 tree, v3 renames `hookMessage` to `custom`; migration on load, documented).
- Extension API is TypeScript-only; no declared semver contract for the event union beyond the package version. Changelog is in `packages/coding-agent/CHANGELOG.md` (not analyzed for event history).
- Platform: Linux/macOS/Windows; `powershell` tool exists alongside `bash` (`PowerShellToolCallEvent`). Windows differences in docs `windows.md` not reviewed.
- `agent_end.willRetry` exists on the wire/SDK type but not on the extension event type (version-skew hazard for code that shares types).
- Experimental server/client protocol is version 8 and gated by `PI_EXPERIMENTAL=1`; not part of the npm package (documented in its README).
- Hooks file-based "plugins" (`examples/plugins/pi-example-plugin`) are Chord facets (dependency-injected services) for the experimental server, not an event system.

## Changes from previous documentation

Not applicable: `PREVIOUS_DOCUMENTATION` was `NONE`. No prior version was compared.

## Unknowns and verification gaps

- No subagent identity or parent fields exist; whether future versions add them is unknown. Parent resolution for the example extension is `inferred` only.
- `docs/json.md` omits `parentToolCallId` on `tool_execution_*`; whether the JSON/RPC wire carries it for nested calls is inferred from `JsonAgentSessionEvent` passthrough and `AgentSessionEvent` typing, not tested on the wire.
- `docs/message-types.md` omits `nestedCalls` and `thinkingLevel`; `docs/session-format.md` calls ids "UUID" though uuidv7 is used.
- Whether `agent_end` extension events should expose `willRetry` is unknown.
- `CompactionPreparation`, `ConversationView`, durable SQLite schema, and `Model` object extras were not enumerated.
- Test bodies were mostly not read; "tested" claims rest on test names.
- Timeouts or handler concurrency limits for extension handlers: none found; absence not proven for every code path.
- Behavior of terminal-escape notification extensions under JSON/RPC mode was not tested.
- `src/experimental/**`, Radius relay, and `packages/client/src/client.ts` were not read in depth.
- Settings schema details (`retry.*`, `compaction.*`) were not fully verified.
- Event ordering of `ui_prompt_*` (emitted via `queueMicrotask`, observed) relative to handler side effects was not tested.

## Consumer implementation checklist

Safe to rely on (documented):

- Discriminate by `type`. Use `tool_execution_*` plus `toolCallId` to correlate tool lifecycle; the toolResult message carries the same `toolCallId`.
- Treat `agent_settled` as "idle/done", `agent_end` as "run segment ended". Use `agent_end.willRetry` (wire) to detect pending retry.
- Read session id from the JSON header or RPC `get_state`; do not expect it on events.
- Split JSONL on LF only; read stdout continuously; log diagnostics from stderr.
- Use `message_end` as the authoritative message; `message_update` is delta-only on the wire.
- Approvals/questions over RPC: handle `extension_ui_request` and reply with `extension_ui_response` using the same `id`.

Needs feature detection or defensive parsing:

- `parentToolCallId`: optional; absent for top-level calls and not documented in `docs/json.md`.
- `agent_end.willRetry`: wire/SDK only.
- `compaction_end.result`: key absent when aborted or failed.
- `entry_appended`: also fires for non-extension entries; tolerate unknown `usage.kind` and unknown entry types.
- Unknown event types and fields: ignore unknown, never fail.
- Message/timestamp units: entry timestamps ISO strings, message timestamps epoch ms.
- Optional/missing keys versus `undefined`: JSON drops `undefined`; do not assume `name: null`.

Subagent detection and parent resolution:

- Do not attempt to detect subagents from payload fields; none exist. Record parent identity yourself when spawning a child `pi` (for example set your own env var or keep a process map), and key child output to the parent `toolCallId` you control.
- If you only see the parent's stream with the example `subagent` tool, correlate by `toolCallId` of the `subagent` tool and treat `tool_execution_update` snapshots as the only child trace (`inferred`).
- Do not use `parentToolCallId` or header `parentSession` as agent-parent links.

## Evidence index

| Topic | Path |
|---|---|
| Extension guide | `packages/coding-agent/docs/extensions.md` |
| Event/type declarations | `packages/coding-agent/src/core/extensions/types.ts` (`ExtensionEvent`, `ExtensionAPI.on`, `ExtensionContext`, `ToolCallEvent`, `ToolResultEvent`, `BoundaryState`) |
| Dispatch semantics | `packages/coding-agent/src/core/extensions/runner.ts` (`emit`, `emitBoundary`, `emitToolCall`, `emitToolResult`, `emitMessageEnd`, `emitInput`, `emitBeforeAgentStart`, `emitProjectTrustEvent`, `withUIPrompt`) |
| Event mapping and session events | `packages/coding-agent/src/core/agent-session.ts` (`AgentSessionEvent`, `_emitExtensionEvent`, `_handleAgentEvent`, `_beforeToolCall`, `_afterToolCall`, `_emitAgentSettled`) |
| Session lifecycle emits | `packages/coding-agent/src/core/agent-session-runtime.ts`; `packages/coding-agent/src/core/sdk.ts` (provider events) |
| JSON/RPC docs | `docs/json.md`, `docs/rpc.md`, `docs/rpc-commands.md`, `docs/rpc-extension-ui.md`, `docs/sdk.md` |
| JSON/RPC code | `packages/coding-agent/src/modes/json-event.ts`, `src/modes/rpc/rpc-mode.ts`, `rpc-types.ts`, `print-mode.ts` |
| Session format | `docs/session-format.md`; `src/core/session-manager.ts` |
| Messages/usage | `docs/message-types.md`; `packages/ai/src/types.ts` |
| Nested tool calls | `src/core/nested-tool-calls.ts`; tests `test/nested-tool-calls.test.ts`, `test/suite/agent-session-tool-orchestration.test.ts` |
| Trust | `docs/security.md`; `src/core/project-trust.ts`, `trust-manager.ts`; `test/extensions-runner.test.ts` |
| Tests cited | `test/extensions-runner.test.ts`, `test/extensions-input-event.test.ts`, `test/agent-session-runtime-events.test.ts`, `test/rpc-jsonl.test.ts` |
| Agent core events | `packages/agent/src/types.ts` (`AgentEvent`) |
| Subagent example | `packages/coding-agent/examples/extensions/subagent/{index.ts,agents.ts,README.md}`; test `test/suite/regressions/8261-subagent-project-trust.test.ts` |
| Durable subagent (experimental) | `packages/coding-agent/src/experimental/durable/subagent.ts`; `packages/durable/src/{types.ts,harness/events.ts,harness/task-graph.ts}` |
| Protocol (experimental) | `packages/protocol/src/{protocol,framing,codec}.ts` |
| Telemetry | `packages/telemetry/src/index.ts`; `src/core/telemetry.ts` |
| Env vars | `docs/environment-variables.md`; `src/core/tools/bash.ts` |
| Plugin example | `packages/coding-agent/examples/plugins/pi-example-plugin` (Chord facets, not hooks) |
