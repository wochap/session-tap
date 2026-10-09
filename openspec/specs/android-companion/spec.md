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
Each session row SHALL show the effective status, session name (or the provider name when there is none), a provider mark, repository branch, a one-line reason or summary, a relative update time, and a child count when children exist. Expanding a row SHALL list its children with status, agent type, summary, and elapsed time. The detail screen SHALL show provider metadata (model, effort, permission mode), the hub and source, the status reason, cwd shortened against the home directory, branch, dirty flag, short head, context window percent, humanized input and output tokens, children, and created and updated times. Filter chips SHALL offer All, Needs attention, Running, and Stale. When the session list is scrolled to its very top and its content changes, it SHALL stay scrolled to the top so items added or moved above the old first item are visible. When the user has scrolled away from the top, content changes SHALL NOT move the list.

#### Scenario: Open detail
- **WHEN** the user taps a session row
- **THEN** the detail screen shows that agent's metadata, usage, repository, and children

#### Scenario: Filter with no matches
- **WHEN** the user selects Stale and no agent is stale
- **THEN** the list shows an empty filter state

#### Scenario: Agent moves to the top while at the top
- **WHEN** the list is scrolled to the top and an update moves another agent above the first row
- **THEN** the list stays at the top and that agent's row is visible

#### Scenario: Agent moves to the top while scrolled down
- **WHEN** the user has scrolled down the list and an update moves an agent to the top
- **THEN** the rows on screen stay where they are

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

The notification title SHALL be the session name or the provider name. The text SHALL include the hub name, the event, the shortened cwd, and the branch. The expanded body SHALL hold the reason summary, and a footer line SHALL hold context percent and humanized tokens. A notification SHALL be cancelled when its agent leaves `blocked`, so a later re-block notifies again. The same transition SHALL NOT notify twice. Notifications SHALL be grouped per hub, SHALL hide the reason on a secure lock screen, and SHALL offer Open (to the session detail) and Mute hub for one hour, with no action that changes agent state. A needs-attention notification for an agent whose terminal this device can open with the `control` scope SHALL also offer "Open terminal", which opens the terminal screen for that agent after the device is unlocked.

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

#### Scenario: Open terminal from a notification
- **WHEN** the device has `control` and the user taps "Open terminal" on a needs-permission notification
- **THEN** the app opens that agent's terminal screen

#### Scenario: No control scope
- **WHEN** the device has only `read` and `manage`
- **THEN** needs-attention notifications offer only Open and Mute hub

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

### Requirement: App shows requested and granted scopes
While waiting for operator approval during pairing, the app SHALL list the scopes requested by the QR payload as chips labelled Read, Manage, Watch terminal, and Control terminal. The Control terminal chip SHALL use the amber warning style, and the screen SHALL show the line "Control terminal can type into agents, which can run commands on <hub name>." when `control` is requested. Each hub card on the Hubs screen SHALL show the hub's granted scopes, as last reported by `hub.info`, as compact chips in the order `read`, `manage`, `watch`, `control`, with `control` in the warning style. A scope name the app does not know SHALL be shown as its raw name. Onboarding copy SHALL describe the app as following sessions and, on hubs that allow it, answering agents in their live terminal.

#### Scenario: Pairing requests control
- **WHEN** the user scans a QR code whose requested scopes are `read`, `watch`, and `control`
- **THEN** the waiting screen shows Read, Watch terminal, and an amber Control terminal chip, plus the warning line naming the hub

#### Scenario: Default pairing
- **WHEN** the user scans a QR code whose requested scopes are `read` and `manage`
- **THEN** the waiting screen shows Read and Manage chips and no warning line

#### Scenario: Hub card after pairing with terminal scopes
- **WHEN** a hub reports the scopes `read`, `manage`, `watch`, and `control`
- **THEN** its Hubs screen card shows four access chips in that order with `control` in the warning style

### Requirement: App follows scope changes from the hub
The app SHALL store the scopes from every `hub.info` result as the hub's granted scopes, so access shown in the app follows the hub's effective scopes. When the hub closes the connection with code `4403` (scope withdrawn), the app SHALL reconnect at once without backoff and take the new scopes from `hub.info`. When the granted scopes do not include `read`, the app SHALL NOT call `listen` for that hub. It SHALL then show the hub as connected without session access, with a re-pair action, and keep the connection open.

#### Scenario: Scope withdrawn while listening
- **WHEN** the hub closes a live connection with code `4403` after the device was re-paired with `read` only
- **THEN** the app reconnects immediately, its Hubs card no longer shows Manage, and the session list keeps updating

#### Scenario: Read scope removed
- **WHEN** after a `4403` close the hub reports scopes without `read`
- **THEN** the app does not call `listen`, keeps the connection, and shows the hub as having no session access with a re-pair action

### Requirement: Session detail offers the agent terminal by scope
The session detail screen SHALL offer "Open terminal" when the hub's effective scopes for this device include `control` and the agent's public view carries a `terminal` descriptor, and SHALL offer "View terminal" when the effective scopes include `watch` but not `control`. When the terminal is available but the device has neither scope, the screen SHALL show a muted line saying the hub did not grant terminal access and how to re-pair with it, and SHALL NOT show a button that would fail. When the agent's public view carries no `terminal` descriptor (not in a multiplexer, headless, or its process has exited), the screen SHALL show no terminal entry. An agent that is `stopped` after a finished turn while its process keeps running SHALL keep its terminal entry. Session rows SHALL show a terminal icon when the device can open that agent's terminal.

#### Scenario: Control scope
- **WHEN** the device has `control` on the hub and the agent's terminal is available
- **THEN** the detail screen shows "Open terminal" and the session row shows the terminal icon

#### Scenario: Watch scope only
- **WHEN** the device has `watch` but not `control`
- **THEN** the detail screen shows "View terminal" with an eye icon

#### Scenario: No terminal scope
- **WHEN** the device has only `read` and `manage` and the agent's terminal is available
- **THEN** the detail screen shows the no-access line with the re-pair hint and no terminal button

#### Scenario: Headless agent
- **WHEN** the agent's public view has no `terminal` descriptor
- **THEN** the detail screen shows no terminal entry and the row shows no terminal icon

#### Scenario: Stopped after a finished turn
- **WHEN** the device has `control`, the agent's status is `stopped` with reason `completed`, and its process is still running in tmux
- **THEN** the detail screen shows "Open terminal" and the session row shows the terminal icon

### Requirement: Terminal screen shows only the agent's pane
The terminal screen SHALL show exactly one agent's pane, opened with `terminal.open` for that source and invocation and closed with `terminal.close` when the user leaves. It SHALL NOT offer tabs, a new terminal, or any way to open a shell. It SHALL render each `snapshot` frame by resetting the view to its contents, cursor, and size, SHALL append `output` frames, SHALL render the pane at the desktop pane's size, SHALL show that size in a chip that updates when a new `snapshot` reports a new size, and SHALL NOT ask the hub to resize the pane. The top bar SHALL show the session name, the effective status, the branch and hub, and the connection state (live, connecting, reconnecting, input paused, ended, offline, closed).

#### Scenario: Open from detail
- **WHEN** the user taps "Open terminal"
- **THEN** the app opens the agent's terminal stream and renders the hub's snapshot followed by live output

#### Scenario: Desktop pane resizes
- **WHEN** the desktop pane changes from 120x40 to 132x38 while the screen is open
- **THEN** the terminal re-renders at 132x38 and the size chip reads 132x38, and the app sends no resize

### Requirement: Terminal view is readable on a phone
In portrait the terminal SHALL open at a 9sp font and SHALL pan horizontally to keep the cursor in view when the pane is wider than the screen. Tapping the size chip or double-tapping the terminal SHALL toggle between that size and fit-to-width. Pinch SHALL zoom between fit-to-width and 200%. In landscape the terminal SHALL open at fit-to-width. The app SHALL keep the last 500 lines of scrollback; while the user is scrolled up, new output SHALL NOT move the view and a "Jump to live" pill with the count of new lines SHALL return to the live bottom. The terminal surface SHALL stay dark in both the light and dark app themes and SHALL map the 16 ANSI colors to the app's terminal palette, while 256-color and 24-bit colors render as sent. The terminal font SHALL include the Nerd Font glyph set (Powerline symbols and icons), and each such glyph SHALL render within one cell. Pane content SHALL draw only inside the terminal surface and SHALL NOT draw over the top bar, banners, key bar, or input row at any zoom level, scroll position, or keyboard state.

#### Scenario: Wide pane in portrait
- **WHEN** a 160-column pane opens in portrait
- **THEN** text renders at 9sp and the view pans so the cursor column is visible

#### Scenario: Scrolled up while output arrives
- **WHEN** the user scrolls up and the agent prints 12 lines
- **THEN** the view stays put and a "Jump to live" pill shows 12 new lines

#### Scenario: Light theme
- **WHEN** the app uses the light theme
- **THEN** the top bar, key bar, and reply field are light and the terminal surface stays dark

#### Scenario: Nerd Font icons in a prompt
- **WHEN** the pane shows a prompt with a Powerline separator and a Nerd Font folder icon
- **THEN** both glyphs render as their icons, each one cell wide, and the following text stays aligned to the column grid

#### Scenario: Pinch zoom on a tall pane
- **WHEN** the user pinches the terminal up to 200% so the pane is taller than the terminal surface
- **THEN** rows beyond the surface's top edge are hidden and the top bar stays fully visible

#### Scenario: Keyboard opens over a full pane
- **WHEN** the keyboard opens and the pane no longer fits the shortened terminal surface
- **THEN** no pane text draws over the top bar or the input row

### Requirement: Control scope sends input to the agent
With the `control` scope the terminal screen SHALL show a key bar laid out as rows of keys, a keyboard toggle, and a reply field with a Send button. The default key bar SHALL have two rows of seven keys: Esc, Tab, Shift+Tab, Up, Ctrl+C, Paste, Backspace, then Ctrl, Alt, Left, Down, Right, Space, Enter. Each named key SHALL send that key to the agent as `terminal.input` `keys`, and each character key SHALL send that character. Ctrl and Alt SHALL be modifiers: a tap latches the modifier for the next key only, a long-press locks it until tapped again, and the next key from the key bar or the soft keyboard SHALL be sent with the latched or locked modifiers. Ctrl+C SHALL send nothing on the first tap and SHALL send Ctrl+C only on a second tap within 2.5 seconds. Paste SHALL insert the phone clipboard into the reply field and SHALL NOT send it. Send SHALL send the reply text as a `paste` with `enter` true and clear the field; long-pressing Send SHALL send it with `enter` false. A hub error for a sent input SHALL keep the reply text and show the error. Tapping the reply field SHALL focus it and open the soft keyboard, and the field SHALL keep focus while the controls rearrange around the keyboard. Keys and keystrokes SHALL reach the agent in the order the user pressed them.

#### Scenario: Answer a menu with arrows
- **WHEN** the agent shows an approval menu and the user taps Down then Enter
- **THEN** the app sends the Down key and then the Enter key to the agent

#### Scenario: Ctrl+C needs a second tap
- **WHEN** the user taps Ctrl+C once and waits 3 seconds
- **THEN** nothing is sent and the key returns to its normal state

#### Scenario: Paste from clipboard
- **WHEN** the user taps Paste with text on the clipboard
- **THEN** the text appears in the reply field and nothing is sent until the user taps Send

#### Scenario: Send without Enter
- **WHEN** the user long-presses Send with "see CI run 4821" in the field
- **THEN** the app sends that text without Enter

#### Scenario: Keyboard stays open in the reply field
- **WHEN** the user taps "Reply to agent…" in portrait
- **THEN** the soft keyboard opens, the reply field moves above the key bar, the field keeps focus, and typed text appears in it

#### Scenario: One-shot Ctrl
- **WHEN** the user taps Ctrl and then Left
- **THEN** the app sends `ctrl+left`, and Ctrl returns to its normal state

#### Scenario: Locked Ctrl
- **WHEN** the user long-presses Ctrl and then taps Up twice
- **THEN** the app sends `ctrl+up` twice, and Ctrl stays locked until tapped again

### Requirement: Direct keyboard mode sends keystrokes to the agent
With the `control` scope and input enabled, the keyboard toggle SHALL open the soft keyboard in direct mode. In direct mode the reply field SHALL be replaced by a "Typing to agent" strip with the toggle, and every key the soft keyboard or a hardware keyboard produces SHALL be sent to the agent at once as `terminal.input` `keys`: printable characters as character keys, and Enter, Backspace, Tab, Escape, the arrows, and the other supported named keys as named keys. Text the keyboard commits as a word (swipe typing, suggestions) SHALL be sent as its characters in order. The keyboard SHALL use no autocorrect and no suggestions. A latched or locked Ctrl or Alt SHALL apply to the next keystroke, and the strip SHALL show the active modifiers and briefly show a sent combination (for example "Sent Ctrl+R"). Tapping the toggle again, or hiding the keyboard, SHALL leave direct mode and show the reply field again. The toggle SHALL be disabled when input is paused or the device has only `watch`.

#### Scenario: Vim motion
- **WHEN** direct mode is on and the user types `w`, `i`, `x`, then taps Esc on the key bar
- **THEN** the app sends `w`, `i`, `x`, `escape` in that order and the reply field stays hidden

#### Scenario: Ctrl from the key bar with a typed letter
- **WHEN** direct mode is on, the user taps Ctrl, then types `r` on the soft keyboard
- **THEN** the app sends `ctrl+r` and the strip briefly reads "Sent Ctrl+R"

#### Scenario: Leave direct mode
- **WHEN** the user taps the toggle while direct mode is on
- **THEN** the keyboard closes, the reply field returns, and nothing more is sent

#### Scenario: Input paused
- **WHEN** the agent is not in front of its pane
- **THEN** the keyboard toggle is disabled like the keys

### Requirement: Key layout is editable and global
The app SHALL let the user edit the key bar layout from "Edit keys" in the terminal top bar menu or from the popover that a long-press on any key opens. The editor SHALL show the rows with a live preview. It SHALL let the user drag keys to reorder them within and across rows, tap a key to replace or remove it, add a key from a picker (the named keys, Ctrl+C, Paste, Ctrl, Alt, and "Character…" for any single printable character), add a row, and delete a row. It SHALL allow at most 7 keys per row, at most 4 rows, and at least 1 row. "Reset to default" SHALL restore the default layout after confirmation. The layout SHALL be stored on the device, SHALL survive restarts, and SHALL apply to every agent and hub. The key bar SHALL grow upward when it has more rows.

#### Scenario: Add a character key
- **WHEN** the user adds a third row with the characters `/`, `-`, `$`, `:` and keys Home, End, PgUp
- **THEN** every terminal shows a three-row key bar with those keys on top, and tapping `$` sends `$`

#### Scenario: Reorder across rows
- **WHEN** the user drags Space from row 2 to the start of row 1
- **THEN** the preview and the terminal key bar show Space first in row 1

#### Scenario: Row limit
- **WHEN** the layout has 4 rows
- **THEN** "Add row" is disabled

#### Scenario: Reset
- **WHEN** the user taps "Reset to default" and confirms
- **THEN** the layout returns to the default two rows

### Requirement: Terminal top bar menu
The terminal top bar SHALL offer a ⋮ menu with Fit to width (the same toggle as the size chip), Copy visible screen (copies the pane's visible text to the clipboard), and Edit keys. Edit keys SHALL be shown only with the `control` scope.

#### Scenario: Copy visible screen
- **WHEN** the user picks Copy visible screen
- **THEN** the clipboard holds the visible pane text with trailing blanks trimmed

### Requirement: Agent questions get answer helpers
When the agent's effective status is blocked on approval, the device has `control`, and the agent's `terminal` descriptor has quick-pick `digits`, the terminal screen SHALL show an "Agent is asking" banner with digit chips 1 to 4 that each send that digit as a one-character key; the chips SHALL carry no option labels. When the agent waits for input, the banner SHALL say to reply below and SHALL show no chips. When the descriptor's quick-pick is `none`, no chips SHALL show. The banner SHALL include the hint "Space toggles · Enter confirms" for multi-select menus.

#### Scenario: Approval with digit answers
- **WHEN** a Claude agent waits for approval and the device has `control`
- **THEN** the banner shows chips 1 2 3 4, and tapping 1 sends the digit 1

#### Scenario: Open question
- **WHEN** the agent waits for input
- **THEN** the banner says "Agent is asking — reply below" with no chips

### Requirement: Watch-only and refused input are read-only
With `watch` but not `control`, the terminal screen SHALL show the live pane with no key bar and no reply field, and a strip saying this phone cannot type into agents on the hub with a "How to allow" link. When `terminal.input` is answered `forbidden` because the device lost `control`, the screen SHALL switch to the same watch-only layout and keep any typed reply out of the request. When `terminal.open` is answered `source_disallows_control`, or the stream ends with that reason, the screen SHALL show no pane and a message that the agent's source does not share its terminals, with "Back to session".

#### Scenario: Watch only
- **WHEN** a device with only `watch` opens the terminal
- **THEN** the pane streams live and no key bar or reply field is shown

#### Scenario: Source does not share terminals
- **WHEN** `terminal.open` is answered `source_disallows_control`
- **THEN** the screen shows the source message with "Back to session" and no pane

#### Scenario: Control withdrawn mid-stream
- **WHEN** the user sends input and the hub answers `forbidden`
- **THEN** the key bar and reply field disappear, the watch-only strip shows, and the pane keeps streaming

### Requirement: Input pauses while the agent is not in front
When an `input` frame (or a snapshot's input availability) reports input unavailable with reason `not_foreground`, or an input is answered `not_foreground`, the screen SHALL keep streaming the live pane, SHALL grey out the key bar and reply field without hiding them, SHALL keep any typed reply, and SHALL show a banner that input returns when the agent is in front again. When the reason is `pane_in_mode`, the banner SHALL say so and that input resumes when they leave scroll mode. When an `input` frame reports input available again, the controls SHALL re-enable without user action.

#### Scenario: Agent suspended on the desktop
- **WHEN** the desktop user suspends the agent
- **THEN** the pane keeps streaming, the controls are greyed, and the paused banner shows

#### Scenario: Desktop scrolling
- **WHEN** the desktop user enters scroll mode in the pane
- **THEN** the controls are greyed and the banner says the desktop is scrolling this pane

#### Scenario: Agent back in front
- **WHEN** an `input` frame reports input available again
- **THEN** the key bar and reply field re-enable with the typed reply still in the field

### Requirement: Ended terminals keep the last frame
When an `ended` frame arrives with reason `agent_exited` ("Agent exited"), `pane_closed` or `session_closed` ("Pane closed on desktop"), or `multiplexer_stopped` ("tmux server stopped"), the screen SHALL keep the last frame dimmed and scrollable, SHALL replace the key bar and reply field with an end card naming the reason and the time, and SHALL offer "Back to session" and "Copy last screen". No action on an ended terminal SHALL reopen input. When the stream ends with reason `identity_changed`, the screen SHALL hide the pane and show "Terminal closed for safety" without naming the process now in the pane.

#### Scenario: Agent exits
- **WHEN** the agent exits while the terminal is open
- **THEN** the last frame stays dimmed with an "Agent exited" end card and no input controls

#### Scenario: Copy last screen
- **WHEN** the user taps "Copy last screen" on an ended terminal
- **THEN** the visible screen text is copied to the clipboard

#### Scenario: Pane identity changed
- **WHEN** the stream ends with reason `identity_changed`
- **THEN** the screen shows "Terminal closed for safety" and no pane content

### Requirement: Terminal connection states are explicit
While the stream opens, the screen SHALL show a connecting placeholder naming the hub. When the hub connection drops, the connection closes with `4403`, or the stream ends with `source_unavailable`, the screen SHALL keep the last frame, show reconnecting, disable input, keep any typed reply, and reopen the terminal with a fresh snapshot after reconnecting; it SHALL NOT send the kept reply until the user taps Send. When a later `snapshot` arrives on an open stream, the screen SHALL show "Catching up" until it renders. When the device is revoked (close `4401`), or after reconnecting the hub's effective scopes no longer include `watch`, the screen SHALL show that terminal access was revoked; when they include `watch` but not `control`, it SHALL reopen in the watch-only layout. Access revoked, hub unreachable, source does not share terminals, and safety refusal SHALL each have distinct copy; revoked SHALL offer only a return to the session detail.

#### Scenario: Reconnect keeps the reply
- **WHEN** the connection drops with "use the backoff helper" typed and then recovers
- **THEN** the terminal reopens with a fresh snapshot, the text is still in the field, and nothing was sent

#### Scenario: Terminal scope revoked mid-stream
- **WHEN** the device is re-paired without `watch` while the terminal is open, so the hub closes the connection with `4403` and `hub.info` no longer lists `watch`
- **THEN** the screen shows "Terminal access was revoked" and offers only "Back to session"

### Requirement: App icon is the prompt caret
The launcher icon SHALL be an adaptive icon showing a prompt chevron followed by an accent-colored cursor bar, inside the adaptive icon safe zone, on a dark background with a radial accent gradient. The icon SHALL provide a monochrome layer for Android themed icons. The notification small icon SHALL use the same chevron and cursor glyph as a single-color silhouette.

#### Scenario: Launcher
- **WHEN** the app is installed on a launcher that uses circle or squircle masks
- **THEN** the full chevron and cursor bar are visible inside the mask

#### Scenario: Themed icons
- **WHEN** the user enables themed icons in Android settings
- **THEN** the launcher shows the chevron and cursor glyph in the system theme color

#### Scenario: Notification
- **WHEN** the app posts a status or background connection notification
- **THEN** the status bar shows the chevron and cursor glyph
