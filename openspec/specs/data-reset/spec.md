# Data Reset

## Purpose

Provide a `sessiontap nuke` command that safely removes local SessionTap broker and hub state and runtime data, refusing while components run and confirming before deletion.

## Requirements

### Requirement: Nuke removes local SessionTap and hub data
`sessiontap nuke` SHALL remove the broker state directory (`$XDG_STATE_HOME/sessiontap`, falling back to `$HOME/.local/state/sessiontap`), the hub state directory (`$XDG_STATE_HOME/sessiontap-hub`), the broker runtime directory (`$XDG_RUNTIME_DIR/sessiontap`), and the hub runtime directory (`$XDG_RUNTIME_DIR/sessiontap-hub`). When `XDG_RUNTIME_DIR` is unset or not absolute, the runtime directories SHALL be resolved under `<state home>/runtime`, matching the daemons' own resolution. The command SHALL NOT remove configuration directories, provider hook installations, or any parent directory of the targets. Targets that do not exist SHALL be skipped.

#### Scenario: All data removed
- **WHEN** no component is running and the user runs `sessiontap nuke --yes`
- **THEN** all four target directories that existed are removed, `~/.config/sessiontap` and `~/.config/sessiontap-hub` remain, and the command exits 0

#### Scenario: Runtime fallback
- **WHEN** `XDG_RUNTIME_DIR` is unset and the user runs `sessiontap nuke --dry-run`
- **THEN** the listed runtime targets are `<state home>/runtime/sessiontap` and `<state home>/runtime/sessiontap-hub`

#### Scenario: Nothing to remove
- **WHEN** none of the targets exist
- **THEN** the command reports that there is nothing to remove and exits 0

### Requirement: Nuke refuses while components run
Before removing anything, `sessiontap nuke` SHALL probe the `sessiontapd`, hook-inspection, and `sessiontap-hub` lock files, and SHALL abort with a non-zero exit and no deletion when any is held, naming the running component and its lock path. Probing SHALL NOT create a lock file that did not exist.

#### Scenario: Broker running
- **WHEN** `sessiontapd` holds `sessiontap.lock` and the user runs `sessiontap nuke --yes`
- **THEN** the command fails with a message naming `sessiontapd`, and every target remains

#### Scenario: Hub running
- **WHEN** `sessiontap-hub` holds `sessiontap-hub.lock`
- **THEN** the command fails with a message naming `sessiontap-hub`, and every target remains

#### Scenario: Stale lock file
- **WHEN** a lock file exists but no process holds it
- **THEN** the probe succeeds and the nuke proceeds

### Requirement: Nuke lists targets and confirms
`sessiontap nuke` SHALL print every existing target path and a warning that hub pairing (TLS identity and paired devices) will be lost and that remote hubs are not affected. Without `--yes`, it SHALL proceed only after the user answers `y` or `yes` on an interactive stdin, and SHALL refuse without deletion when stdin is not a terminal. With `--dry-run`, it SHALL print the targets and exit 0 without deleting or prompting.

#### Scenario: Declined confirmation
- **WHEN** the user runs `sessiontap nuke` and answers `n`
- **THEN** nothing is removed and the command exits 0 with a note that it aborted

#### Scenario: Non-interactive without --yes
- **WHEN** `sessiontap nuke` runs with stdin not a terminal and without `--yes`
- **THEN** the command fails asking for `--yes`, and nothing is removed

#### Scenario: Dry run
- **WHEN** the user runs `sessiontap nuke --dry-run`
- **THEN** the existing targets and the pairing warning are printed, nothing is removed, and no prompt appears
