# Supervised Agent Launch

## Purpose

Interactive provider launch with exact argument forwarding, invocation identity assignment, process supervision, automatic broker startup, and clean shutdown semantics on Linux/Wayland.

## Requirements

### Requirement: First-party platform support is Linux/Wayland only
SessionTap's first-party supported environment SHALL be Linux with a Wayland session and SHALL NOT introduce compatibility-only behavior for macOS, Windows, or X11.

#### Scenario: First-party validation target
- **WHEN** upstream SessionTap builds, packages, and tests a release
- **THEN** Linux with Wayland is the sole required platform target

#### Scenario: Unsupported platform execution
- **WHEN** SessionTap is built or run on macOS, Windows, or Linux under X11
- **THEN** no compatibility behavior or support guarantee is provided by the upstream project

### Requirement: Provider arguments are forwarded exactly
The `sessiontap` CLI SHALL treat the first recognized provider name as the end of SessionTap option parsing and SHALL forward every following argument to the provider as a discrete, ordered argument without shell re-parsing.

#### Scenario: Provider help flag is not consumed
- **WHEN** the user runs `sessiontap codex --help`
- **THEN** SessionTap launches Codex with the single `--help` argument and does not display SessionTap help

#### Scenario: Argument boundaries are preserved
- **WHEN** a provider argument contains whitespace, quotes, or shell metacharacters
- **THEN** the provider receives the same argument value without splitting or shell expansion

### Requirement: Wrapped sessions remain interactive
SessionTap SHALL launch the provider with the caller's terminal streams and environment semantics intact and SHALL NOT switch an interactive provider into a headless or stdout JSON mode.

#### Scenario: Interactive TUI launch
- **WHEN** the user launches `sessiontap claude`, `sessiontap codex`, or `sessiontap qwen` from a terminal
- **THEN** the provider's native interactive TUI can read input, render output, observe terminal resize, and use terminal control sequences normally

### Requirement: The wrapper supervises provider lifecycle
The wrapper SHALL register an invocation before launch, report the child process identity after spawn, forward termination-related signals, record the child's termination, and exit with the provider's exit status or corresponding signal status.

#### Scenario: Provider exits normally
- **WHEN** the provider exits with a nonzero exit code
- **THEN** the invocation becomes stopped with that exit code and `sessiontap` exits with the same code

#### Scenario: Provider is interrupted
- **WHEN** the wrapper receives an interactive termination signal
- **THEN** it forwards the signal to the provider process group, records the resulting termination, and does not leave a live orphaned provider

#### Scenario: Wrapper disappears unexpectedly
- **WHEN** the wrapper can no longer report lifecycle but the broker detects that the registered process no longer exists
- **THEN** the broker marks the lifecycle lost and derives stopped status

### Requirement: Every launch has distinct identities
SessionTap SHALL assign an opaque invocation ID before launch and SHALL store a provider session ID separately when the provider reports one.

#### Scenario: Provider session is learned after launch
- **WHEN** a hook reports a provider session ID for a registered invocation
- **THEN** SessionTap associates it with that invocation without replacing the invocation ID

### Requirement: Broker startup is automatic and fail-open
The CLI SHALL attempt to start the per-user broker when it is unavailable, serialize concurrent startup attempts, and warn but preserve provider launch when observability initialization fails.

#### Scenario: Concurrent first launches
- **WHEN** two SessionTap wrappers start while no broker is running
- **THEN** at most one broker becomes the active owner of the runtime socket and both provider launches proceed

#### Scenario: Broker cannot start
- **WHEN** the broker fails to become ready within the startup deadline
- **THEN** SessionTap prints a diagnostic, launches the requested provider without tracking, and preserves its interactive and exit behavior

### Requirement: Persisted arguments are sanitized
SessionTap SHALL persist and forward an argument array with known credential values redacted and SHALL NOT persist an unsanitized command-line string.

#### Scenario: Secret-bearing option
- **WHEN** an adapter recognizes an API key or token option in provider arguments
- **THEN** the stored and forwarded argument value is replaced with a redaction marker while the provider receives the original value
