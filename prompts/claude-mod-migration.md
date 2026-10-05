# Plan: move the Claude adapter from hooks + transcript to a Claude Code mod

Run this from the root of the SessionTap repository. It is a plan for a later
agent. Do the stability check first and stop there if mods are not ready.

## Background

The Claude adapter (`crates/sessiontap-adapters/src/claude.rs`) observes
Claude Code through two channels:

- classic settings hooks (`HOOK_EVENTS`), installed into
  `~/.claude/settings.json` as `sessiontap hook emit` commands, for lifecycle,
  turns, tools, approval, idle, compaction, completion, failure, subagents,
  model (`SessionStart`, `PostModelSwitch`), effort, and permission mode;
- the session transcript JSONL under `~/.claude/projects`, collected in the
  background, for the session name, token usage, and a fallback model.

Claude Code 2.1.287 added function hooks ("mods"): in-process TypeScript
plugins registered with `register(on, options)`. They expose richer events
than classic hooks. As of 2026-10-05 they are early access, need
`CLAUDE_CODE_ENABLE_FUNCTION_HOOKS=1`, and may change without notice, so the
adapter stayed on hooks + transcript.

Gains a mod would bring:

| Need | Mod source |
|---|---|
| model and effort for every model request, including subagents | `turn.step` (`model`, `effort`, `agentId`) |
| context window percent, rate limits, cost | `session.measure` |
| subagent parent link | `agent.spawn` (`parentAgentId`) |
| turn completion with reason and usage | `turn.complete` |
| approvals | `tool.check` |
| everything else | `classic.<Event>` wrappers |

Today `context_window_percent` is always absent for Claude, and classic hooks
carry no subagent parent link.

## Step 1: stability gate (stop here if it fails)

Check the current Claude Code public distribution (a local checkout of the
public `claude-code` repository, or the installed CLI) and record the results:

1. The version that removed the early-access label from function hooks
   (`CHANGELOG.md`, `mods/README.md`, the banner in the public mod typings).
2. Whether mods still need `CLAUDE_CODE_ENABLE_FUNCTION_HOOKS` or another
   opt-in.
3. Whether `turn.step`, `turn.complete`, `session.measure`, `agent.spawn`,
   `tool.check`, `session.start`, `session.end`, and the `classic.*` wrappers
   still exist with the fields listed above.
4. Whether `$.process.run` and `$.http.fetch` with `socketPath` are still
   available to user-tier plugins, and which managed policies can refuse them.
5. How a plugin is installed and enabled for the user (plugin directory,
   marketplace, `claude plugin` commands), and how removal works.

If mods are still early access or still need an opt-in flag, write a short
note with the findings to `docs/providers/claude.md` and stop. Do not change
code.

To refresh the full surface map, run `prompts/extract-agent-hooks-api.md`
against the Claude Code distribution first and use its report.

## Step 2: design (OpenSpec change)

Create an OpenSpec change (for example `claude-mod-adapter`) with
`/opsx:propose`. Settle these decisions in its design:

- **Adapter boundary.** The mod is a new input channel inside the Claude
  adapter. It sends raw payloads that the Rust adapter normalizes into the same
  `NormalizedEvent` / `ProviderMetadata` / `Usage` values. Core, storage, the
  hub, and the Android app must not change. If the mod exposes data the schema
  cannot hold (cost, rate limits), propose that as a separate schema change.
- **Transport.** Either `$.process.run` with the existing
  `sessiontap hook emit claude` command, or `$.http.fetch` to the daemon's Unix
  socket. Prefer the existing emit path unless measured latency is a problem.
  Keep every send fail-open and asynchronous, and stay well inside the 10 s
  per-hook budget.
- **Evidence channel.** Mod payloads are managed provider evidence, as hooks
  are. Decide whether they need a distinct evidence label.
- **Cutover.** Run mod and hooks side by side behind a setup option, compare
  the normalized output on real sessions, then remove the classic hooks and
  the transcript collector. Keep the transcript collector only for data the
  mod cannot give (check the session name).
- **Fallback.** Decide what happens when the user's Claude Code is older than
  the minimum mod version, or a managed policy refuses the mod: keep hooks as
  a fallback, or report degraded observability in `sessiontap doctor`.
- **Setup, doctor, removal.** Install the plugin as SessionTap-owned files,
  verify it in doctor, and remove only SessionTap's files. Users' own plugins
  and settings must stay byte-identical.

## Step 3: implementation rules

- Clean-room (`docs/clean-room.md`): write SessionTap's own `register.ts`
  against the published public mod interface. Do not copy or adapt code,
  tests, or fixtures from the `mods/` examples, and do not vendor the
  generated typings file. Generate types locally if needed and keep them out of
  the repository. Add a line to `docs/clean-room.md` naming the public mod
  typings and docs as contract evidence.
- Ship the mod source as an adapter asset (as `assets/pi-extension.ts` is for
  pi), rendered with the resolved `sessiontap` executable path.
- Keep absence as absence: never invent model, usage, or context values.
- Bound and sanitize every string before it leaves the adapter.
- Tests: synthetic mod payload fixtures for each event used, a setup, doctor,
  and removal test in a temporary home, and the existing Claude adapter tests
  kept green until hooks are removed.
- Update `docs/providers/claude.md` with the minimum Claude Code version, the
  enablement steps, and the events used.

## Gates

```
cargo fmt --all --check && cargo clippy --workspace --all-targets --all-features -- -D warnings && cargo test --workspace --all-features
```

Before removing hooks, also test by hand on a real Claude Code session: start,
prompt, tool use, approval, `/model` switch, subagent, compaction, interrupt,
and exit. The snapshot fields must match what the hooks produced.
