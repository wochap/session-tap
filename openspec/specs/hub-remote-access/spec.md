# Hub Remote Access

## Purpose

Lets paired remote devices, such as the Android companion app, observe merged hub state and administer stopped agents over an end-to-end encrypted, mutually authenticated channel on the LAN or a tailnet.

## Requirements

### Requirement: Remote listener binds only explicit addresses
The hub SHALL serve remote access only when the configuration names one or more `remote.listen` socket addresses. Each address SHALL be a concrete IP and port. The hub SHALL refuse to start the remote listener for an unspecified address (`0.0.0.0` or `::`) and SHALL report the offending address. Without a `remote` section the hub SHALL open no remote port. Ingestion and the local unix socket SHALL behave the same with or without remote access.

#### Scenario: Tailnet address configured
- **WHEN** the configuration sets `remote.listen: ["100.64.0.7:8932"]`
- **THEN** the hub accepts remote connections on that address only

#### Scenario: Wildcard address configured
- **WHEN** the configuration sets `remote.listen: ["0.0.0.0:8932"]`
- **THEN** the hub reports that wildcard addresses are not allowed and does not open the remote listener

#### Scenario: Remote access not configured
- **WHEN** the configuration has no `remote` section
- **THEN** the hub opens no remote port and ingestion and `sessiontap-hub listen` work unchanged

### Requirement: Hub owns a persistent remote identity
The hub SHALL generate a private key and self-signed certificate on first remote start, store them in its private state directory readable only by the owner, and reuse them across restarts. The hub ID SHALL be the lowercase hex SHA-256 of the identity's SubjectPublicKeyInfo. Remote connections SHALL use TLS 1.3 only.

#### Scenario: Hub restarts
- **WHEN** the hub restarts with an existing identity
- **THEN** it presents the same certificate and hub ID, and paired devices reconnect without re-pairing

#### Scenario: Identity file is deleted
- **WHEN** the identity is missing at start
- **THEN** the hub generates a new identity with a new hub ID, and previously paired devices fail certificate pinning until they pair again

### Requirement: Pairing opens a short single-use window
`sessiontap-hub pair` SHALL ask the running service to open a pairing window and SHALL fail with a clear message when the service is not running or remote access is not configured. The window SHALL carry a fresh random 32-byte secret, SHALL expire after 120 seconds, and SHALL allow at most one successful pairing. The command SHALL render a terminal QR code whose payload is a versioned JSON object containing the hub name, the hub ID, endpoint hints, the requested scopes, the secret, and the expiry time. Endpoint hints SHALL list every configured remote listen address plus every configured `remote.advertise` entry. Scopes SHALL default to `read` and `manage` and MAY be narrowed with `--scope`.

#### Scenario: Operator starts pairing
- **WHEN** the user runs `sessiontap-hub pair` while the service runs with remote access configured
- **THEN** the terminal shows a QR code and a countdown, and the payload names the hub ID and all endpoint hints

#### Scenario: Window expires
- **WHEN** no device completes pairing within 120 seconds
- **THEN** the hub discards the secret, the command reports that pairing expired and exits non-zero, and a later attempt with that QR fails

#### Scenario: Service not running
- **WHEN** the user runs `sessiontap-hub pair` and no hub service is listening on the unix socket
- **THEN** the command reports that the service is not running and exits non-zero

### Requirement: Pairing proves possession of the secret and the operator confirms
A device SHALL pair over a TLS connection that presents its own client certificate and pins the server certificate to the hub ID from the QR code. The hub SHALL send a fresh nonce. The device SHALL answer with its display name and an HMAC-SHA256, keyed by the pairing secret, over a fixed protocol label, the hub SPKI, the device SPKI taken from the TLS session, and the nonce. On a valid MAC the hub SHALL show the device name and a fingerprint derived from the device SPKI in the `pair` terminal and SHALL trust the device only after the operator accepts. The hub SHALL burn the pairing window after three invalid MACs, after an operator rejection, or after one successful pairing.

#### Scenario: Successful pairing
- **WHEN** a device presents a valid MAC within the window and the operator accepts the shown fingerprint
- **THEN** the hub stores the device with the requested scopes, returns the device ID and hub name to the device, and the command exits successfully

#### Scenario: Already paired device pairs again
- **WHEN** a device whose SPKI is already stored completes pairing again
- **THEN** the hub replaces that device's name and scopes instead of storing a second entry

#### Scenario: Operator rejects
- **WHEN** the operator declines the prompt
- **THEN** the hub stores nothing, tells the device pairing was rejected, and closes the window

#### Scenario: Wrong secret
- **WHEN** a device sends a MAC that does not verify
- **THEN** the hub tells the device pairing failed without prompting the operator, and after the third failure the window closes

#### Scenario: Server certificate does not match the QR code
- **WHEN** the certificate presented by an endpoint does not hash to the QR hub ID
- **THEN** the device aborts before sending any pairing message

### Requirement: Paired devices authenticate with pinned client certificates
After pairing, the hub SHALL accept a remote request other than pairing only from a connection whose client certificate SPKI matches a stored, unrevoked device. A connection with an unknown or absent client certificate SHALL be limited to the pairing methods and SHALL receive an `unauthorized` error for anything else. Device identity SHALL NOT depend on the device's IP address.

#### Scenario: Paired device reconnects from a new IP address
- **WHEN** a paired device connects from a different address than before
- **THEN** the hub authenticates it by its certificate and serves it normally

#### Scenario: Unknown device requests state
- **WHEN** a connection without a stored device certificate sends `listen`
- **THEN** the hub answers `unauthorized` and sends no hub state

### Requirement: Devices can be listed and revoked
`sessiontap-hub devices` SHALL list each paired device with its device ID, name, scopes, pairing time, and last-seen time. `sessiontap-hub revoke <device>` SHALL accept a device ID or unique ID prefix, delete the device, and close the device's open remote connections immediately. An ambiguous or unknown prefix SHALL fail without revoking anything.

#### Scenario: Revoke a connected device
- **WHEN** the user revokes a device that has a live `listen` stream
- **THEN** the hub closes that connection and refuses the device's next connection attempt

#### Scenario: Ambiguous prefix
- **WHEN** the given prefix matches two devices
- **THEN** the command lists the matches, revokes nothing, and exits non-zero

### Requirement: Remote protocol carries requests and pushed events
The remote channel SHALL exchange JSON text messages. A request SHALL carry a client-chosen `id`, a `method`, and optional `params`. Each request SHALL receive exactly one response with the same `id` holding either `result` or an `error` with a stable `code` and a message. Pushed stream messages SHALL carry no `id`. An unknown method SHALL receive the error code `unknown_method`, and a malformed message SHALL receive `bad_request`. The hub SHALL support at least the methods `hub.info`, `listen`, and `forget`, and SHALL enforce device scopes per method, answering `forbidden` when a scope is missing.

#### Scenario: Hub info
- **WHEN** a paired device sends `{"id":1,"method":"hub.info"}`
- **THEN** the hub responds with the same `id` and a result holding the hub ID, hub name, protocol version, and the device's scopes

#### Scenario: Unknown method
- **WHEN** a paired device sends a method the hub does not implement
- **THEN** the hub responds with error code `unknown_method` and keeps the connection open

#### Scenario: Read-only device calls forget
- **WHEN** a device with only the `read` scope sends `forget`
- **THEN** the hub responds with error code `forbidden` and changes no state

### Requirement: Remote listen streams the merged state without gaps
The `listen` method (scope `read`) SHALL be acknowledged and then followed by pushed messages with the same snapshot and update envelopes as `sessiontap-hub listen`, under the same gap-free baseline rules: a complete snapshot first, then each accepted update after that snapshot's revision, and a new complete snapshot whenever the hub re-baselines (a source snapshot, a forget, or a lagging consumer). The hub SHALL send WebSocket pings at least every 60 seconds and SHALL close a connection that does not answer.

#### Scenario: Device subscribes
- **WHEN** a paired device sends `listen`
- **THEN** it receives a result, then one complete snapshot of all agents from all sources, then live updates

#### Scenario: Mid-stream re-baseline
- **WHEN** a source snapshot is applied while a device is listening
- **THEN** the device receives a new complete snapshot that replaces its prior view

### Requirement: Remote forget removes a stopped agent
The `forget` method (scope `manage`) SHALL take a source ID and an invocation ID and apply the hub forget operation. It SHALL answer `not_found` for an unknown agent and `not_stopped` for an agent whose status is not `stopped`.

#### Scenario: Forget a stopped agent remotely
- **WHEN** a device with the `manage` scope forgets a stopped agent
- **THEN** the response succeeds and every listener, remote or local, receives a snapshot without that agent

#### Scenario: Forget a running agent
- **WHEN** a device forgets an agent whose status is `running`
- **THEN** the hub answers `not_stopped` and changes no state
