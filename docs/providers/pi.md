# Pi compatibility

Pi has no configuration-file hooks. `sessiontap setup pi` installs one
SessionTap-owned TypeScript extension, `sessiontap.ts`, marked with
`// sessiontap-managed-extension v1` and embedding the resolved SessionTap
executable path. Each handler forwards a bounded payload keyed by `pi_event`
to `sessiontap hook emit pi`. Delivery is asynchronous and fails open, so pi
behaves the same with or without the extension.

## Extension directory

The extension lives in `$PI_CODING_AGENT_DIR/extensions/` when
`PI_CODING_AGENT_DIR` is set and non-empty, and in `~/.pi/agent/extensions/`
otherwise. Setup, doctor, and removal all use the same location. Setup refreshes
only the marked file, doctor only reads it, and removal deletes only the marked
file. Other extensions in the directory are never touched.

## Events used

| `pi_event` | Normalized kind |
| --- | --- |
| `session_start` | provider session started |
| `session_shutdown` | provider session ended |
| `session_info_changed`, `model_select`, `thinking_level_select`, `turn_end` | enrichment |
| `before_agent_start` | new turn |
| `turn_start`, `tool_execution_start`, `tool_execution_end` | working |
| `agent_settled` | completed, failed (`error`), or interrupted (`aborted`) |

Every payload carries the session ID, session name, selected model
(`provider/id`), thinking level, and run mode when pi exposes them. Any stop
reason other than `error` or `aborted`, including pi v1 `deferred`, settles as
complete. Tool events carry only a normalized tool label and the call ID.
`agent_settled` carries usage that the extension accumulates itself from
assistant messages (fresh, cache-read, and cache-write input; output), plus
pi's context usage. The adapter never reads pi session files.

## Model and effort sources

- The selected model comes from `ctx.model` on every forwarded event and from
  `model_select`.
- On `turn_end`, when the assistant message names a string `provider` and
  `model`, the extension forwards `served_model = provider/model`. The adapter
  prefers `served_model` over `model`, so a virtual selection such as
  `openai-codex/auto` shows the model that actually answered, for example
  `openai-codex/gpt-5.5`. The next `model_select` shows the selection again
  until the following turn ends.
- Effort comes from `thinking_level` on every event and from
  `thinking_level_select`. Accepted values are `off`, `minimal`, `low`,
  `medium`, `high`, `max`, and `xhigh`.

Model values are sanitized and bounded to 160 characters. The last model and
effort stay in the retained snapshot after the session ends.

Tested with pi 1.0.2. Pi launched without the SessionTap wrapper is not
tracked: the extension's emit exits silently.
