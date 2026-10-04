# hub-terminal-relay Specification

## Purpose
Relays live agent terminal streams and input between paired devices and the source daemons that own the agents' tmux panes, over an outbound control channel from each opted-in source to the hub.

## Requirements

### Requirement: Opted-in sources keep an outbound control channel to the hub
A source daemon SHALL open a control channel to the hub only for an enabled hub sink that sets `control: true`. The channel SHALL be a WebSocket connection that the daemon dials to the hub sink's host and port at the path `/control`, using the same scheme rules, trusted addresses, and bearer token as that sink's ingestion. When the channel fails to open or closes, the daemon SHALL retry with exponential backoff starting at 1 second and capped at 30 seconds, without a retry limit, and SHALL log the first failure and the next success once each. Ingestion delivery SHALL behave the same whether the control channel is open, retrying, or disabled.

#### Scenario: Control enabled
- **WHEN** the daemon's hub sink sets `control: true` and the hub is reachable
- **THEN** the daemon opens one control channel to the hub and keeps it open

#### Scenario: Control not enabled
- **WHEN** the hub sink omits `control` or sets it to `false`
- **THEN** the daemon opens no control channel and the hub answers `source_disallows_control` when a device opens a terminal of that source

#### Scenario: Hub restarts
- **WHEN** the hub stops and starts again while the daemon runs
- **THEN** the daemon reopens the control channel with backoff and ingestion continues independently

### Requirement: Hub binds each control channel to one authenticated source
The hub SHALL accept control channel upgrades on its ingestion address. When source tokens are configured, the hub SHALL authenticate the bearer token before upgrading, SHALL answer 401 with error code `unauthorized` for a missing or unknown token, and SHALL bind the channel to the source ID named in the daemon's first message only when that source ID is bound to the token; otherwise it SHALL close the channel with reason `source_not_permitted`. On a loopback ingestion address without configured tokens, the hub SHALL accept the named source ID as it does for ingestion. The hub SHALL keep at most one control channel per source ID; a newer authenticated channel for the same source SHALL replace the older one, and streams on the older channel SHALL end with reason `source_unavailable`.

#### Scenario: Token for another source
- **WHEN** a control channel presents the `sandbox` token and names source `host`
- **THEN** the hub closes the channel with reason `source_not_permitted` and relays nothing to it

#### Scenario: Daemon reconnects
- **WHEN** a source opens a second control channel while the first still looks open
- **THEN** the hub uses the new channel and ends streams that were relayed over the old one with `source_unavailable`

### Requirement: Devices open terminal streams through the hub
A device whose effective scopes include `watch` SHALL open a terminal stream with `terminal.open`, naming a source ID and an invocation ID. Effective scopes follow the `hub-remote-access` scope rules, so a device paired with `control` also holds `watch`, and a hub with `remote.control` off grants neither. The hub SHALL forward the request to that source's control channel and SHALL answer with a result holding a hub-assigned stream ID, or with an error: `forbidden` without effective `watch`, `not_found` for an agent the hub does not hold, `source_disallows_control` when the source has no control channel, `source_unavailable` when the source does not answer within 5 seconds, or the source daemon's own refusal code from the `terminal-stream` capability (`not_found`, `terminal_unavailable`, `unsupported_backend`). After a successful answer, the hub SHALL push that stream's terminal frames to the device in the order the source produced them, starting with a `snapshot` frame. The hub SHALL NOT receive, store, or forward multiplexer metadata; requests and frames carry only source ID, invocation ID, stream ID, and terminal content.

#### Scenario: Open a terminal
- **WHEN** a device with `watch` opens the terminal of a running agent on a control-enabled source
- **THEN** the device receives a stream ID, then a `snapshot` frame with the pane contents, size, cursor, and input availability, followed by `output` frames

#### Scenario: Source not opted in
- **WHEN** a device opens a terminal for an agent whose source has no control channel
- **THEN** the hub answers `source_disallows_control` and sends nothing to the source

#### Scenario: Device without watch
- **WHEN** a device with only `read` and `manage` sends `terminal.open`
- **THEN** the hub answers `forbidden` and sends nothing to the source

#### Scenario: Control-only pairing can watch
- **WHEN** a device paired with `--scope control` sends `terminal.open` on a hub with `remote.control: true`
- **THEN** the hub treats the device as holding `watch` and opens the stream

### Requirement: Input reaches the agent only through the source's local checks
A device whose effective scopes include `control` SHALL send input to an open stream with `terminal.input`, carrying either `keys` (a list of keys as defined by the `terminal-stream` capability: named keys or single printable characters) or `paste` (text with a boolean `enter`). The hub SHALL forward input only for a stream that device opened. The source daemon SHALL accept input only while its hub sink sets `control: true` at the time of each input, and SHALL apply the `terminal-stream` input rules before writing. Each input SHALL receive exactly one response: success when the daemon wrote it, or an error code: `forbidden`, `not_foreground`, `pane_in_mode`, `terminal_ended`, `bad_request`, `source_disallows_control`, or `source_unavailable`. Refused input SHALL NOT be retried or queued by the hub or the daemon.

#### Scenario: Answer an approval
- **WHEN** a device with `control` sends the key `1` to the stream of an agent waiting for approval
- **THEN** the daemon writes the key to the agent's pane and the device receives success

#### Scenario: Agent suspended
- **WHEN** a device sends input while the agent is not the terminal's foreground process group
- **THEN** the device receives `not_foreground` and nothing reaches the pane

#### Scenario: Source turns control off
- **WHEN** the operator sets `control: false` on the source's hub sink while a stream is open
- **THEN** the next input is refused with `source_disallows_control`, the daemon closes its control channel, and the stream ends with reason `source_disallows_control`

#### Scenario: Device without control
- **WHEN** a device with `watch` but not `control` sends `terminal.input`
- **THEN** the hub answers `forbidden` and forwards nothing

### Requirement: Stream frames and end reasons are relayed unchanged
The hub SHALL push each stream's frames as messages of type `terminal` carrying the stream ID and one frame from the `terminal-stream` capability: `snapshot`, `output`, `input`, or `ended`, with output bytes base64-encoded and the source's `seq`. A later `snapshot` on the same stream SHALL replace the device's view (resync after lag, multiplexer drop, or pane resize). The `input` frame SHALL report input availability and, when unavailable, the reason `not_foreground` or `pane_in_mode`. The `ended` frame SHALL carry one of the `terminal-stream` end reasons (`agent_exited`, `pane_closed`, `session_closed`, `multiplexer_stopped`, `identity_changed`) or one of the relay's own reasons: `source_unavailable`, `source_disallows_control`, or `closed`. After `ended`, the hub SHALL send no more frames for that stream ID and SHALL forward no input for it. `terminal.close` SHALL end the stream with reason `closed` and SHALL release it on the source when no other device watches the same agent.

#### Scenario: Agent exits
- **WHEN** the agent exits while a device watches its terminal
- **THEN** the device receives `ended` with reason `agent_exited` and later input for that stream is answered `terminal_ended`

#### Scenario: Control channel drops
- **WHEN** a source's control channel closes while devices watch its terminals
- **THEN** each of those streams ends with reason `source_unavailable`

#### Scenario: Device disconnects
- **WHEN** the device's connection closes without `terminal.close`
- **THEN** the hub ends that device's streams and releases them on the source

### Requirement: Slow watchers never slow the agent
The source daemon SHALL read pane output continuously regardless of how fast devices consume it. When a stream's pending output exceeds its buffer limit on the daemon or the hub, that stream's pending output SHALL be discarded and a fresh `snapshot` frame sent instead. A slow stream SHALL NOT delay other streams, ingestion, or the agent's pane.

#### Scenario: Phone on a slow link
- **WHEN** a device cannot keep up with an agent's output
- **THEN** the device receives a fresh `snapshot` after the backlog is dropped, and other devices watching the same agent keep receiving output

### Requirement: Revocation and scope changes release affected streams
Terminal streams SHALL depend on the `watch` scope for the scope-withdrawn rule of `hub-remote-access`. When a device is revoked, its connections close with code `4401`, and the hub SHALL release all its terminal streams on the sources before `revoke` returns and SHALL forward no input from that device afterwards. When a re-pair removes `watch` from the device's effective scopes, its connections carrying terminal streams close with code `4403` and the hub SHALL release those streams on the sources. When a re-pair removes only `control`, the streams SHALL stay open and further `terminal.input` SHALL be answered `forbidden`.

#### Scenario: Revoke while watching
- **WHEN** the user revokes a device that has an open terminal stream
- **THEN** the connection closes with `4401`, the source releases the stream, and no input from that device reaches the pane after `revoke` returns

#### Scenario: Re-pair drops control
- **WHEN** a device with an open stream is re-paired with `read` and `watch` only
- **THEN** its stream keeps showing output and its next `terminal.input` is answered `forbidden`

#### Scenario: Re-pair drops watch
- **WHEN** a device with an open stream is re-paired with `read` only
- **THEN** its connection closes with `4403` and the source releases the stream
