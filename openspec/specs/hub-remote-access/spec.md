# Hub Remote Access

## Purpose

Lets paired remote devices, such as the Android companion app, observe merged hub state and administer stopped agents over an end-to-end encrypted, mutually authenticated channel on the LAN or a tailnet.

## Requirements

### Requirement: Remote listener binds configured addresses and retries until bound
The hub SHALL serve remote access only when the configuration names one or more `remote.listen` socket addresses. Each address SHALL be either a concrete IP and port or a wildcard bind (`0.0.0.0:<port>` or `[::]:<port>`). A wildcard bind SHALL be opt-in only and SHALL be the sole `remote.listen` entry; a configuration that combines a wildcard with any other entry, names an empty list, or holds an entry that is not an IP and port SHALL be invalid, and the error SHALL name the offending entry. Without a `remote` section the hub SHALL open no remote port. Ingestion and the local unix socket SHALL behave the same with or without remote access.

When binding a configured address fails, the hub SHALL keep running, SHALL log the failure once with the address and the error, and SHALL retry that address with exponential backoff starting at 1 second and doubling up to a 30 second cap, without a retry limit. The hub SHALL log once more when the address binds. When a bound remote listener fails while accepting connections, the hub SHALL log it and return that address to the same retry loop. Failed or retrying addresses SHALL NOT affect other remote addresses, ingestion, or the local unix socket. Authorization SHALL NOT depend on which address or bind mode accepted the connection.

#### Scenario: Tailnet address configured
- **WHEN** the configuration sets `remote.listen: ["100.64.0.7:8932"]`
- **THEN** the hub accepts remote connections on that address only

#### Scenario: Wildcard address configured
- **WHEN** the configuration sets `remote.listen: ["0.0.0.0:8932"]`
- **THEN** the hub accepts remote connections on port 8932 of every IPv4 interface, and pairing and device authentication work as they do for an explicit address

#### Scenario: Wildcard combined with another address
- **WHEN** the configuration sets `remote.listen: ["0.0.0.0:8932", "100.64.0.7:8932"]`
- **THEN** the configuration is invalid, the error names the wildcard entry, and the hub opens no remote listener

#### Scenario: Address not yet assigned at start
- **WHEN** the hub starts before the interface holding `192.168.0.165` has that address, and the configuration sets `remote.listen: ["192.168.0.165:8932", "100.64.0.7:8932"]`
- **THEN** the hub logs one bind failure for `192.168.0.165:8932`, serves `100.64.0.7:8932`, ingestion, and the unix socket at once, and accepts remote connections on `192.168.0.165:8932` within 30 seconds of the address appearing, logging that it is now bound

#### Scenario: Persistent bind failure
- **WHEN** a configured address keeps failing to bind
- **THEN** the hub keeps retrying at most every 30 seconds, does not log each retry, and does not exit

#### Scenario: Bound address goes away
- **WHEN** the accept loop of a bound remote listener fails
- **THEN** the hub logs the failure and retries binding that address with the same backoff, without affecting other listeners

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
`sessiontap-hub pair` SHALL ask the running service to open a pairing window and SHALL fail with a clear message when the service is not running or remote access is not configured. The window SHALL carry a fresh random 32-byte secret, SHALL expire after 120 seconds, and SHALL allow at most one successful pairing. The command SHALL render a terminal QR code whose payload is a versioned JSON object containing the hub name, the hub ID, endpoint hints, the requested scopes, the secret, and the expiry time. Endpoint hints SHALL be those defined by "Pairing endpoint hints reflect the current bind mode". Without `--scope` the requested scopes SHALL be `read` and `manage`. One or more `--scope` options SHALL replace that default with exactly the named scopes, expanded by the implications in "Devices hold scopes from a fixed set". An unknown scope name SHALL fail before any window opens, and the error SHALL list the valid scope names.

#### Scenario: Operator starts pairing
- **WHEN** the user runs `sessiontap-hub pair` while the service runs with remote access configured
- **THEN** the terminal shows a QR code and a countdown, and the payload names the hub ID, all endpoint hints, and the scopes `read` and `manage`

#### Scenario: Operator narrows scopes
- **WHEN** the user runs `sessiontap-hub pair --scope read`
- **THEN** the payload's requested scopes are exactly `read`

#### Scenario: Unknown scope
- **WHEN** the user runs `sessiontap-hub pair --scope write`
- **THEN** the command fails naming `write` and listing `read`, `manage`, `watch`, and `control`, and no pairing window opens

#### Scenario: Window expires
- **WHEN** no device completes pairing within 120 seconds
- **THEN** the hub discards the secret, the command reports that pairing expired and exits non-zero, and a later attempt with that QR fails

#### Scenario: Service not running
- **WHEN** the user runs `sessiontap-hub pair` and no hub service is listening on the unix socket
- **THEN** the command reports that the service is not running and exits non-zero

### Requirement: Pairing proves possession of the secret and the operator confirms
A device SHALL pair over a TLS connection that presents its own client certificate and pins the server certificate to the hub ID from the QR code. The hub SHALL send a fresh nonce. The device SHALL answer with its display name and an HMAC-SHA256, keyed by the pairing secret, over a fixed protocol label, the hub SPKI, the device SPKI taken from the TLS session, and the nonce. On a valid MAC the hub SHALL show the device name and a fingerprint derived from the device SPKI in the `pair` terminal and SHALL trust the device only after the operator accepts.

The hub SHALL count invalid MACs per pairing window in three ways:

- Per client SPKI, at most 3.
- Per peer address, at most 5. Addresses are grouped the same way as for connection limits.
- In total, at most 20.

A client whose SPKI or peer address has reached its limit SHALL be locked out for the rest of the window. Its later `pair.complete` calls SHALL receive `pairing_failed` without checking the MAC, whether or not a window is open. A locked-out client SHALL NOT affect other clients. The hub SHALL burn the pairing window after 20 invalid MACs in total, after an operator rejection, or after one successful pairing. A client that is not locked out SHALL receive `pairing_closed` when no window is open.

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
- **THEN** the hub tells the device pairing failed without prompting the operator, and the window stays open for other clients

#### Scenario: Hostile host cannot burn the window
- **WHEN** one peer address sends invalid MACs under many different client certificates
- **THEN** after its fifth invalid MAC that address gets `pairing_failed` for every later attempt, and a device from another address can still pair with the open window

#### Scenario: Client key locked out
- **WHEN** one client SPKI has sent 3 invalid MACs in the current window
- **THEN** its next `pair.complete` gets `pairing_failed` even with a valid MAC, and the operator is not prompted

#### Scenario: Total failure backstop
- **WHEN** the window has received 20 invalid MACs in total from any clients
- **THEN** the hub closes the window, and the `pair` command reports too many failed attempts and exits non-zero

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

Revocation SHALL also cover requests already in flight:

- Once `revoke` has returned, no request from that device SHALL change hub state.
- Once `revoke` has returned, no request from that device SHALL receive a successful response.
- After the revocation close, the device's connections SHALL send no further stream data.

The hub SHALL check the device's stored record and scopes when it handles each request, not only when the connection opens. Re-pairing that changes a device's scopes SHALL therefore apply from the device's next request on every open connection.

#### Scenario: Revoke a connected device
- **WHEN** the user revokes a device that has a live `listen` stream
- **THEN** the hub closes that connection and refuses the device's next connection attempt

#### Scenario: Revoke races an in-flight forget
- **WHEN** a device with the `manage` scope sends `forget` and the user revokes the device while that request is being handled
- **THEN** either the forget completes before `revoke` returns, or it changes no state and the device gets no success response

#### Scenario: Re-pairing narrows scopes
- **WHEN** a connected device is re-paired with only the `read` scope and then sends `forget` on its existing connection
- **THEN** the hub answers `forbidden` and changes no state

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

### Requirement: Pre-auth connections are bounded in time, count, and size
A remote connection SHALL count as unauthenticated from accept until one of two things happens: its client certificate SPKI resolves to a stored, unrevoked device, or it completes pairing. The hub SHALL enforce these limits on remote connections:

- It SHALL close a connection whose TLS handshake does not finish within 10 seconds of accept.
- It SHALL close a connection whose WebSocket upgrade does not finish within 10 seconds of the TLS handshake.
- It SHALL close a connection that is still unauthenticated 30 seconds after accept. The exception is a connection whose valid pairing proof is waiting for the operator's decision. That connection SHALL stay open until the operator decides or the `pair` command goes away.
- It SHALL accept at most 64 concurrent remote connections across all listen addresses.
- Of those, at most 16 SHALL be unauthenticated, and at most 4 unauthenticated connections SHALL come from one peer address. An IPv6 peer address SHALL be grouped by its /64 prefix, and an IPv4-mapped IPv6 address SHALL count as its IPv4 address.
- It SHALL close a connection that exceeds a cap right after accept, without a TLS handshake.
- It SHALL reject an incoming WebSocket message or frame larger than 64 KiB by closing the connection.
- It SHALL limit `pair.begin` and `pair.complete` per peer address to a burst of 5 calls, refilled at one call every 2 seconds. A call over the limit SHALL receive the error code `rate_limited`, and it SHALL NOT count as a pairing try.

These limits SHALL NOT be configurable.

#### Scenario: Silent TCP client
- **WHEN** a client opens a TCP connection and sends nothing
- **THEN** the hub closes it within 10 seconds of accept

#### Scenario: Stalled WebSocket upgrade
- **WHEN** a client completes TLS but never sends the WebSocket upgrade request
- **THEN** the hub closes it within 10 seconds of the TLS handshake

#### Scenario: Idle unpaired connection
- **WHEN** a connection without a stored device certificate completes the WebSocket upgrade and then sends nothing, or sends only pairing calls that do not pair it
- **THEN** the hub closes it 30 seconds after accept

#### Scenario: Pairing proof awaits the operator
- **WHEN** a device's valid pairing proof is waiting for the operator's decision longer than 30 seconds after accept
- **THEN** the connection stays open and receives the pairing result once the operator decides

#### Scenario: One host floods connections
- **WHEN** one peer address holds 4 unauthenticated connections and opens a fifth
- **THEN** the hub closes the fifth right after accept, and connections from other addresses and paired devices are still accepted

#### Scenario: Unauthenticated pool full
- **WHEN** 16 unauthenticated connections are open and another unauthenticated connection arrives
- **THEN** the hub closes the new connection right after accept

#### Scenario: Oversized message
- **WHEN** a client sends a WebSocket message larger than 64 KiB
- **THEN** the hub closes the connection without processing the message

#### Scenario: Pairing calls are rate limited
- **WHEN** one peer address sends a sixth `pair.begin` or `pair.complete` within the first 2 seconds
- **THEN** the hub answers it with `rate_limited`, does not check any proof it carries, and does not count it as a try

### Requirement: Requests per connection are bounded
The hub SHALL answer at most one request at a time on an unauthenticated connection. It SHALL close an unauthenticated connection that sends a request while an earlier one is still unanswered. On an authenticated connection, the hub SHALL run at most 8 requests at a time and SHALL answer any request beyond that with the error code `busy`. An active `listen` stream SHALL NOT count toward that limit once its request has been acknowledged. The hub SHALL close any connection where an outbound write cannot finish within 30 seconds. A peer that stops reading SHALL NOT block the hub from serving other connections or from enforcing the unauthenticated deadline.

#### Scenario: Unpaired client pipelines requests
- **WHEN** an unauthenticated connection sends a second request before the first is answered
- **THEN** the hub closes the connection

#### Scenario: Paired device exceeds the in-flight limit
- **WHEN** a paired device has 8 requests running and sends a ninth
- **THEN** the hub answers the ninth with `busy` and keeps the connection open

#### Scenario: Unpaired client stops reading
- **WHEN** an unauthenticated client sends requests and never reads responses
- **THEN** the hub closes it no later than the unauthenticated deadline, and other connections keep being served

### Requirement: Remote listeners survive accept errors
When accepting a TCP connection on a remote listen address fails, the hub SHALL log the error and wait before accepting again. It SHALL keep serving that address. The wait SHALL start at 50 milliseconds, double on each consecutive failure up to 1 second, and reset after a successful accept. An accept error SHALL NOT stop any remote listener or the hub.

#### Scenario: File descriptors exhausted
- **WHEN** accepting on a remote address fails because the process is out of file descriptors
- **THEN** the hub logs the error, backs off, and accepts connections on that address again once descriptors are available

### Requirement: Pairing endpoint hints reflect the current bind mode
The hub SHALL compute the pairing QR endpoint hints (`ep`) at the time `sessiontap-hub pair` opens a window. When every `remote.listen` entry is a concrete IP, the hints SHALL be every configured listen address in configuration order, followed by every `remote.advertise` entry. When `remote.listen` is a wildcard, the hints SHALL be every address currently assigned to a host interface that is up, formatted with the wildcard's port in the order the operating system reports them, followed by every `remote.advertise` entry. For `0.0.0.0` the interface addresses SHALL be IPv4; for `[::]` they SHALL be IPv6 and IPv4. Interface addresses SHALL exclude loopback addresses, IPv4 and IPv6 link-local addresses, and addresses on interfaces whose names start with `docker`, `veth`, `virbr`, or `br-`. IPv6 hints SHALL use the bracketed `[addr]:port` form. The list SHALL contain no duplicates, keeping the first occurrence. When the computed list is empty, `sessiontap-hub pair` SHALL fail with a clear message instead of rendering a QR code the device cannot use. Endpoint hints SHALL only tell the device where to connect; they SHALL NOT grant trust, and the device SHALL still pin the hub ID from the QR code.

#### Scenario: Explicit addresses configured
- **WHEN** the configuration sets `remote.listen: ["100.64.0.7:8932", "192.168.1.20:8932"]` and `remote.advertise: ["macbook.tailnet.ts.net:8932"]`
- **THEN** the QR `ep` is `["100.64.0.7:8932", "192.168.1.20:8932", "macbook.tailnet.ts.net:8932"]`

#### Scenario: Wildcard with LAN, Tailscale, and container interfaces
- **WHEN** `remote.listen` is `["0.0.0.0:8932"]`, `remote.advertise` is `["macbook.tailnet.ts.net:8932"]`, and the host has `lo` 127.0.0.1, `wlan0` 192.168.0.165, `tailscale0` 100.64.0.7, `docker0` 172.17.0.1, and `eth1` 169.254.3.4
- **THEN** the QR `ep` is `["192.168.0.165:8932", "100.64.0.7:8932", "macbook.tailnet.ts.net:8932"]`

#### Scenario: Wildcard address changes between pairings
- **WHEN** `remote.listen` is a wildcard and the LAN address changes from 192.168.0.165 to 192.168.0.170 while the hub runs
- **THEN** the next `sessiontap-hub pair` shows `192.168.0.170:<port>` and not the old address, without restarting the hub

#### Scenario: Wildcard with no usable interface address
- **WHEN** `remote.listen` is a wildcard, no interface has a usable address, and `remote.advertise` is empty
- **THEN** `sessiontap-hub pair` opens no window, reports that no endpoint hints are available and that `remote.advertise` can name one, and exits non-zero

### Requirement: Hub reports current endpoint hints to paired devices
The `hub.info` result SHALL include an `endpoints` array of `host:port` strings. It SHALL hold the hub's current endpoint hints, computed the same way and in the same order as the endpoint hints in the pairing QR payload. The hub SHALL report endpoints only to an authenticated, paired device. An unpaired connection SHALL receive `unauthorized` for `hub.info` and no endpoint list. Endpoint hints SHALL describe reachability only. A device SHALL NOT use them to decide whether a hub is trusted, because hub identity comes only from the pinned certificate.

#### Scenario: Paired device asks for hub info
- **WHEN** a paired device sends `hub.info` to a hub configured with `remote.listen: ["192.168.1.20:8932"]` and `remote.advertise: ["macbook.tailnet.ts.net:8932"]`
- **THEN** the result's `endpoints` is `["192.168.1.20:8932", "macbook.tailnet.ts.net:8932"]`, the same list a pairing QR code from that hub would carry

#### Scenario: Unpaired connection asks for hub info
- **WHEN** a connection without a stored device certificate sends `hub.info`
- **THEN** the hub answers `unauthorized` and the response carries no endpoint list

#### Scenario: Read-only device asks for hub info
- **WHEN** a paired device with only the `read` scope sends `hub.info`
- **THEN** the result includes the `endpoints` list

### Requirement: Devices hold scopes from a fixed set
A paired device SHALL hold a subset of four scopes:

- `read`: observe merged hub state (`listen`).
- `manage`: change hub state, such as `forget`.
- `watch`: view an agent's live terminal, read-only.
- `control`: send input to an agent's live terminal.

Scope requests SHALL be expanded before they are stored or shown: `control` SHALL add `watch`, and `watch` SHALL add `read`. Scopes SHALL be stored and reported in the fixed order `read`, `manage`, `watch`, `control`. `watch` and `control` SHALL never be part of a default scope set.

#### Scenario: Control implies watch and read
- **WHEN** the user runs `sessiontap-hub pair --scope control` on a hub with `remote.control: true`
- **THEN** the payload's requested scopes are `read`, `watch`, and `control`, and the device is stored with those scopes after pairing

#### Scenario: Default pairing grants no terminal access
- **WHEN** the user runs `sessiontap-hub pair` on a hub with `remote.control: true`
- **THEN** the device is stored with `read` and `manage` only

### Requirement: Terminal scopes require remote control to be enabled
The hub configuration SHALL accept a boolean `remote.control`, defaulting to `false`. While it is `false`:

- `sessiontap-hub pair` SHALL refuse a request that includes `watch` or `control`, open no window, and name `remote.control` in its error.
- The hub SHALL treat stored `watch` and `control` scopes as not granted when it authorizes a request.

The device's effective scopes SHALL be its stored scopes, minus `watch` and `control` while `remote.control` is `false`. `hub.info` SHALL report the effective scopes. `sessiontap-hub devices` SHALL list stored scopes and SHALL mark `watch` and `control` as disabled while `remote.control` is `false`.

#### Scenario: Terminal scope requested while disabled
- **WHEN** the hub has no `remote.control` setting and the user runs `sessiontap-hub pair --scope watch`
- **THEN** the command fails with a message naming `remote.control` and exits non-zero without opening a window

#### Scenario: Remote control turned off after pairing
- **WHEN** a device was paired with `read`, `watch`, and `control`, and the hub restarts with `remote.control: false`
- **THEN** `hub.info` reports only `read` for that device, and `sessiontap-hub devices` lists `watch` and `control` as disabled

#### Scenario: Remote control turned back on
- **WHEN** the same hub restarts with `remote.control: true`
- **THEN** `hub.info` reports `read`, `watch`, and `control` for that device without re-pairing

### Requirement: Streams end when their scope is withdrawn
Each long-lived remote stream SHALL depend on one scope: `listen` depends on `read`, and terminal streams (added by `hub-terminal-relay`) depend on `watch`. When a device's effective scopes change and no longer include the scope that one of its open streams depends on, the hub SHALL close each connection carrying such a stream with WebSocket close code `4403` and reason `scope withdrawn`. After the close, that connection SHALL send no further stream data. A connection whose open streams still have their scopes SHALL stay open. Revocation SHALL keep closing connections with code `4401`.

#### Scenario: Re-pairing removes the read scope
- **WHEN** a device with a live `listen` stream is re-paired with only the `manage` scope
- **THEN** the hub closes that connection with code `4403`, and the device's next `listen` on a new connection is answered `forbidden`

#### Scenario: Re-pairing keeps the needed scope
- **WHEN** a device with a live `listen` stream is re-paired with `read` only, dropping `manage`
- **THEN** the `listen` stream stays open and a later `forget` on that connection is answered `forbidden`
