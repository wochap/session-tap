# Shell Completions

## Purpose

Provide installable zsh completions for SessionTap commands and providers, including a CLI command that emits the packaged completion script.

## Requirements

### Requirement: Completion script emission subcommand

The `sessiontap` binary SHALL provide a `completions zsh` subcommand that
writes the zsh completion script to standard output. The emitted script SHALL
be byte-identical to the completion file shipped in the repository. An
unsupported or missing shell argument SHALL produce an error and a non-zero
exit status.

#### Scenario: Emit the zsh script

- **WHEN** the user runs `sessiontap completions zsh`
- **THEN** the command exits successfully and stdout contains a zsh
  completion function whose first line is `#compdef sessiontap`

#### Scenario: Unsupported shell

- **WHEN** the user runs `sessiontap completions fish`
- **THEN** the command exits with a non-zero status and reports that the
  shell is unsupported

#### Scenario: Emitted script matches the repository file

- **WHEN** the embedded script printed by `sessiontap completions zsh` is
  compared with `completions/zsh/_sessiontap`
- **THEN** the two are byte-identical

### Requirement: Subcommand completion coverage

The `_sessiontap` completion function SHALL offer every user-facing
subcommand as a candidate at the first argument position: `setup`, `doctor`,
`hooks`, `status`, `listen`, and `completions`. After `hooks`, it SHALL offer
`remove`. The internal `hook emit` entry point SHALL NOT appear as a
candidate at any position.

#### Scenario: First-position candidates

- **WHEN** zsh completes the first argument of `sessiontap`
- **THEN** the candidates include `setup`, `doctor`, `hooks`, `status`,
  `listen`, and `completions`

#### Scenario: hooks subcommand chain

- **WHEN** zsh completes the second argument of `sessiontap hooks`
- **THEN** the candidates include `remove`

#### Scenario: Internal entry point hidden

- **WHEN** zsh completes any argument position of `sessiontap`
- **THEN** `hook` and `emit` are not offered as candidates

### Requirement: Provider name completion

When the argument position accepts a provider name (`setup`, `doctor`,
`hooks remove`, or the launch position), the completion function SHALL offer
the built-in providers `claude`, `codex`, and `qwen`, plus the name of every
adapter declared as an `[adapters.<name>]` section in the SessionTap config
file.

#### Scenario: Built-in providers

- **WHEN** zsh completes the provider argument of `sessiontap setup` and no
  config file declares custom adapters
- **THEN** the candidates are exactly `claude`, `codex`, and `qwen`

#### Scenario: Custom adapters included

- **WHEN** the SessionTap config file contains an `[adapters.company]`
  section
- **THEN** `company` appears among the provider candidates alongside the
  built-ins

#### Scenario: Provider launch position

- **WHEN** zsh completes the first argument of `sessiontap` and the word
  under the cursor does not match a subcommand
- **THEN** provider names are offered as candidates

### Requirement: Completion config discovery

The completion function SHALL locate the config file at
`$XDG_CONFIG_HOME/sessiontap/config.toml`, falling back to
`$HOME/.config/sessiontap/config.toml` when `XDG_CONFIG_HOME` is unset or
empty. The completion function SHALL NOT execute `sessiontap`, `sessiontapd`,
or any other external process. A missing, unreadable, or malformed config
file SHALL degrade completion to built-in providers without error output.

#### Scenario: XDG_CONFIG_HOME honored

- **WHEN** `XDG_CONFIG_HOME` points to a directory containing
  `sessiontap/config.toml` with an `[adapters.alpha]` section
- **THEN** `alpha` appears among the provider candidates

#### Scenario: Missing config degrades gracefully

- **WHEN** no config file exists at either location
- **THEN** completion still offers the built-in providers and produces no
  error output

#### Scenario: No process execution

- **WHEN** zsh evaluates the `_sessiontap` function for candidates
- **THEN** no `sessiontap`, `sessiontapd`, or other external process is
  spawned

### Requirement: Packaged completion files

The repository SHALL ship completion functions at `completions/zsh/_sessiontap`
and `completions/zsh/_sessiontapd`. The Nix package SHALL install both files
under `share/zsh/site-functions` in the package output. `_sessiontapd` SHALL
be a valid zsh completion function that offers no candidates, reflecting that
the daemon accepts no arguments.

#### Scenario: Nix package contains completion files

- **WHEN** the default Nix package is built
- **THEN** `$out/share/zsh/site-functions/_sessiontap` and
  `$out/share/zsh/site-functions/_sessiontapd` exist

#### Scenario: Daemon completion is inert

- **WHEN** zsh loads `_sessiontapd` and completes arguments of `sessiontapd`
- **THEN** completion succeeds without error and offers no candidates
