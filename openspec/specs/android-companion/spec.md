# Android Companion

## Purpose

A sideloaded Android app that pairs with SessionTap hubs, shows their merged agent sessions, and raises native notifications on the phone when agents need attention or finish.

## Requirements

### Requirement: App targets Android 16 and builds from the repository
The app SHALL live in `android/`, target API level 36, support API level 31 and newer, and build a debug APK with `./gradlew assembleDebug` inside the `nix develop .#android` shell without Android Studio. The shell SHALL provide the Android SDK platform and build tools 36, JDK 21, and `adb`.

#### Scenario: Build on NixOS
- **WHEN** the user runs `nix develop .#android -c ./gradlew assembleDebug` in `android/`
- **THEN** the build produces an installable debug APK

#### Scenario: Install on the phone
- **WHEN** the user runs `adb install` with the built APK on an Android 16 phone
- **THEN** the app installs and starts at onboarding

### Requirement: App pairs with a hub by scanning its QR code
The app SHALL scan a `sessiontap-hub pair` QR code with the camera and reject payloads with an unknown version or a past expiry. It SHALL create or reuse a non-exportable P-256 device key in AndroidKeyStore and connect to the payload's endpoint hints. It SHALL pin the server certificate to the payload's hub ID and complete the pairing exchange. While waiting for operator approval it SHALL show the same device fingerprint groups the hub shows. It SHALL then store the hub ID, name, endpoint hints, and granted scopes. A paired hub is identified only by its authenticated hub ID. Pairing a hub ID that is already stored SHALL update that hub's record in place: name, scopes, and revoked state. Its endpoint hints SHALL be the endpoint that completed pairing, then the QR endpoint hints, then the previously stored hints, deduplicated and capped at 8 entries. Pairing a hub ID that is not stored SHALL add a new hub, even when its endpoints or name match a stored hub. Expired, rejected, failed, and unreachable outcomes SHALL each have a distinct message and a retry path.

#### Scenario: Successful pairing
- **WHEN** the user scans a fresh QR code and approves in the host terminal
- **THEN** the app shows the paired state and the hub appears in the Hubs screen and the session list

#### Scenario: Certificate mismatch
- **WHEN** an endpoint presents a certificate whose SPKI hash differs from the QR hub ID
- **THEN** the app sends no pairing message to it and, if no endpoint matches, reports the hub as unreachable

#### Scenario: Expired QR code
- **WHEN** the scanned payload's expiry is in the past
- **THEN** the app shows the pairing expired state and tells the user to run `sessiontap-hub pair` again

#### Scenario: Hub already paired
- **WHEN** the user scans a QR code for a hub ID that is already paired
- **THEN** the app re-pairs and updates that hub's name, scopes, and revoked state in place, keeps its stored agents, and does not add a duplicate

#### Scenario: Re-pair after the hub's address changed
- **WHEN** a hub stored with endpoint hints `["192.168.1.20:8932"]` is re-paired from a QR code whose hints are `["192.168.1.37:8932"]` and pairing completes on `192.168.1.37:8932`
- **THEN** the hub keeps a single record whose endpoint hints are `["192.168.1.37:8932", "192.168.1.20:8932"]`

#### Scenario: Different hub at a known address
- **WHEN** the user pairs a QR code whose hub ID differs from every stored hub but whose endpoint hints match a stored hub's hints
- **THEN** the app adds it as a new hub and leaves the stored hub unchanged

### Requirement: App supports any number of paired hubs
The app SHALL keep agents keyed by hub ID, source ID, and invocation ID. The user SHALL be able to pair additional hubs and unpair any hub after a confirmation. Unpairing SHALL close that hub's connection, delete its stored state and notifications, and delete its pinned identity. With exactly one hub paired, the session list SHALL hide hub chips and hub section headers.

#### Scenario: Second hub paired
- **WHEN** the user pairs a second hub
- **THEN** the session list shows hub filter chips with connection dots and one section per hub

#### Scenario: Back to one hub
- **WHEN** the user unpairs one of two hubs
- **THEN** the hub chips and hub section headers disappear and the remaining hub's name appears in the top bar

### Requirement: Background connection stays open while hubs are paired
While at least one hub is paired, the app SHALL run a foreground service of type `specialUse` with an ongoing low-priority notification that summarizes hub and attention counts. The service SHALL keep one authenticated connection per hub, call `listen`, try endpoint hints in parallel and prefer the last endpoint that worked, and reconnect with exponential backoff capped at 60 seconds. It SHALL restart after device boot and SHALL stop when no hubs remain. A connection refused as unauthorized SHALL mark the hub as revoked and stop retrying it.

#### Scenario: Wi-Fi turns off
- **WHEN** the phone loses Wi-Fi and stays on the tailnet over mobile data
- **THEN** the service reconnects through the tailnet endpoint hint without re-pairing

#### Scenario: Hub revokes the phone
- **WHEN** the hub answers `unauthorized` because the device was revoked
- **THEN** the app shows the hub as revoked with a re-pair action and stops reconnecting to it

#### Scenario: Phone reboots
- **WHEN** the phone finishes booting with hubs paired
- **THEN** the service starts and reconnects without opening the app

### Requirement: Session state follows the hub stream
For each hub the app SHALL replace its agent set with every received snapshot and apply each update as the complete view for its agent. It SHALL persist the last-known agent set per hub across process restarts and SHALL NOT use hub revisions as a resume cursor. Each hub SHALL show a connection state of live, reconnecting with countdown, offline, or revoked, plus its last sync time.

#### Scenario: Hub re-baselines
- **WHEN** a hub sends a new snapshot mid-stream
- **THEN** agents absent from it disappear from the app and present agents show their snapshot views

#### Scenario: App process restarts
- **WHEN** Android kills and restarts the app while a hub is offline
- **THEN** the session list shows the persisted agents with that hub marked offline

### Requirement: Effective status drives display
The app SHALL compute each agent's effective status: `blocked` if the agent or any child is blocked, else `running` if the agent or any child is running, else the agent's own status. An agent whose `updated_at` is more than 24 hours old SHALL be shown as stale, desaturated, and grouped into a collapsed Stale section, whatever its status. Agents with an effective status of blocked SHALL be listed in a Needs attention section above the hub sections. When a child causes the blocked status, the row SHALL say which child agent type is waiting.

#### Scenario: Child agent waits for approval
- **WHEN** a root agent is running and one child has status `blocked` with an approval reason
- **THEN** the row shows the blocked dot, sits in Needs attention, and names the child agent type

#### Scenario: Idle session from yesterday
- **WHEN** an agent's `updated_at` is 30 hours ago
- **THEN** it appears grayed in the Stale section

### Requirement: Session list and detail present hub data
Each session row SHALL show the effective status, session name (or the provider name when there is none), a provider mark, repository branch, a one-line reason or summary, a relative update time, and a child count when children exist. Expanding a row SHALL list its children with status, agent type, summary, and elapsed time. The detail screen SHALL show provider metadata (model, effort, permission mode), the hub and source, the status reason, cwd shortened against the home directory, branch, dirty flag, short head, context window percent, humanized input and output tokens, children, and created and updated times. Filter chips SHALL offer All, Needs attention, Running, and Stale.

#### Scenario: Open detail
- **WHEN** the user taps a session row
- **THEN** the detail screen shows that agent's metadata, usage, repository, and children

#### Scenario: Filter with no matches
- **WHEN** the user selects Stale and no agent is stale
- **THEN** the list shows an empty filter state

### Requirement: Stopped sessions can be forgotten
For a hub where the device has the `manage` scope, the app SHALL offer forget on stopped sessions by swiping a row and from the detail screen. It SHALL show an undo snackbar for a few seconds and send the hub `forget` only when the undo window ends. Non-stopped sessions SHALL NOT offer forget. A hub error SHALL restore the row and show the error.

#### Scenario: Forget with undo
- **WHEN** the user swipes a stopped row and taps Undo
- **THEN** the row returns and no `forget` request is sent

#### Scenario: Forget completes
- **WHEN** the user swipes a stopped row and lets the snackbar expire
- **THEN** the app sends `forget` and the agent stays gone after the hub's next snapshot

### Requirement: App raises notifications from effective status transitions
The app SHALL compare each agent's previous persisted effective status with its new one, for updates and snapshots alike, and notify as follows:
- entering `blocked`: "<provider> needs your permission", "needs your input", or "needs your attention", by the reason kind of the blocking child or root, on a high-importance channel;
- entering `stopped` with root reason `completed`: "<provider> finished", on a default-importance channel;
- any other transition: nothing.

The notification title SHALL be the session name or the provider name. The text SHALL include the hub name, the event, the shortened cwd, and the branch. The expanded body SHALL hold the reason summary, and a footer line SHALL hold context percent and humanized tokens. A notification SHALL be cancelled when its agent leaves `blocked`, so a later re-block notifies again. The same transition SHALL NOT notify twice. Notifications SHALL be grouped per hub, SHALL hide the reason on a secure lock screen, and SHALL offer Open (to the session detail) and Mute hub for one hour, with no action that changes agent state.

#### Scenario: Approval requested
- **WHEN** an update moves an agent from running to blocked with reason `approval`
- **THEN** a heads-up notification says the provider needs your permission

#### Scenario: Block happened while disconnected
- **WHEN** the app reconnects and the snapshot shows an agent blocked that was running in persisted state
- **THEN** exactly one needs-attention notification is posted

#### Scenario: App restart with an agent still blocked
- **WHEN** the app restarts and the agent's persisted effective status was already blocked
- **THEN** no new notification is posted

#### Scenario: Root stops while a child still runs
- **WHEN** the root view becomes stopped with reason `completed` but a child is still running
- **THEN** no finished notification is posted until the effective status becomes stopped

#### Scenario: Lifecycle-only stop
- **WHEN** an agent becomes stopped without a reason
- **THEN** no notification is posted

### Requirement: Alert settings are local and trimmed
The Alerts screen SHALL provide toggles for needs permission, needs input, and finished, applied to all hubs, plus a per-hub mute with an expiry that the notification Mute action also sets. Settings SHALL be stored only on the phone. The app SHALL NOT offer per-provider filters or quiet hours.

#### Scenario: Finished alerts disabled
- **WHEN** the user turns off Finished and an agent completes
- **THEN** no notification is posted

#### Scenario: Hub muted from a notification
- **WHEN** the user taps Mute hub 1h on a notification
- **THEN** that hub posts no notifications for one hour while the session list keeps updating

### Requirement: Onboarding requests required permissions
Before or right after the first pairing, the app SHALL present a checklist that requests camera access, notification permission, and exemption from battery optimization, and that explains that off-LAN hubs need Tailscale connected on the phone. Denied permissions SHALL show how to grant them later without blocking access to the session list.

#### Scenario: Notifications denied
- **WHEN** the user denies the notification permission
- **THEN** the app keeps working, the Alerts screen shows notifications as disabled, and offers to open system settings

### Requirement: App refreshes endpoint hints from connected hubs
After each successful connection whose server certificate matches the hub's pinned identity, the app SHALL read the `endpoints` list from that hub's `hub.info` result and merge it into the hub's stored endpoint hints. The merged list SHALL be the endpoint that connected, then the hub-reported hints in the hub's order, then the previously stored hints, without duplicates and capped at 8 entries. Entries that are blank or not a `host:port` with a port from 1 to 65535 SHALL be ignored. The next reconnect attempt SHALL use the merged list. The app SHALL accept endpoint hints only from a connection pinned to that hub's identity. It SHALL NOT use endpoint hints to identify, trust, or pin a hub.

#### Scenario: Hub moved to a new address
- **WHEN** the app connects through the tailnet hint and the hub reports `["192.168.1.37:8932", "macbook.tailnet.ts.net:8932"]` while the stored hints are `["192.168.1.20:8932", "macbook.tailnet.ts.net:8932"]`
- **THEN** the stored hints become `["macbook.tailnet.ts.net:8932", "192.168.1.37:8932", "192.168.1.20:8932"]` and the next reconnect tries `192.168.1.37:8932` without re-pairing

#### Scenario: Hub reports many endpoints
- **WHEN** the merged list would hold more than 8 distinct entries
- **THEN** the app keeps the first 8 and drops the remaining previously stored hints

#### Scenario: Hub reports a malformed entry
- **WHEN** the hub-reported list contains an empty string or an entry without a valid port
- **THEN** the app ignores that entry and merges the rest

#### Scenario: Endpoint presents a different certificate
- **WHEN** an endpoint presents a certificate whose SPKI hash differs from the stored hub ID
- **THEN** the app sends it no request and stores no endpoint hints from it
