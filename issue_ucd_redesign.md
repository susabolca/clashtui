# UCD Redesign

## Context

`clashtui` needs a broader UCD pass, not just privilege handling. The current project has several correct technical pieces, but the user-facing model can be simpler and more coherent.

The redesign should cover:

- user mental model
- CLI command semantics
- TUI/config/setup flow
- BIOS-style setup interaction
- AI chat-assisted configuration
- LLM-readable and editable config spec
- subscription and traffic-profile layering
- runtime modes and degradation
- service/startup behavior
- TUN/system permission setup
- mihomo workdir/core management

The goal is not to expose platform details first. Users should be able to start with a working port proxy quickly, then opt into background service and TUN only when they need those capabilities.

## Product Mental Model

Users should see `clashtui` as a mihomo setup utility with a list of Proxies.

There is always one default built-in System Proxy:

- owns the primary user-facing proxy configuration
- default local mixed port is `127.0.0.1:7070`
- can enable/disable OS system proxy
- can optionally use PAC when supported
- can optionally enable TUN for transparent system traffic
- chooses subscription, mode, proxy group, and proxy server like any other proxy config

Users can add additional Port Proxies:

- HTTP/SOCKS5/mixed listeners
- each has its own listen address and port
- each can choose its own subscription, mode, proxy group, and proxy server
- port proxies do not own OS system proxy or TUN settings

The product layers are:

1. Proxy configuration
   - default System Proxy
   - optional user-created Port Proxies
   - subscriptions and proxy selection

2. Background service
   - keeps the controller running after setup
   - optionally starts at login or boot

3. System integration
   - OS system proxy, PAC, TUN, routes, DNS/system integration
   - requires platform-specific permissions

The minimum usable product should require no privilege:

```text
download binary + configure mihomo -> port proxy works
```

Privilege should only unlock additional capabilities:

```text
service setup -> background/startup
tun setup     -> transparent proxy
```

TUN should only be configured from the default System Proxy. It is not a normal user-created Port Proxy setting.

## CLI Model

Everyday commands should stay small:

- `start`, `stop`, `status` are fast runtime commands.
- `config` is the setup/configuration surface.
- no everyday command should unexpectedly prompt for sudo.

`start` should start the full configured mihomo runtime, not just system proxy:

- default System Proxy local listener
- optional HTTP/SOCKS/listener port proxies
- subscription profile and proxy group selections
- optional system proxy integration
- optional TUN system traffic mode
- optional DNS settings

Recommended primary commands:

```text
clashtui start
clashtui stop
clashtui status
clashtui config
```

Advanced/scriptable commands may still exist:

```text
clashtui service-install
clashtui service-uninstall
clashtui service-status
clashtui restart
```

But regular users should be guided through `config`.

## BIOS-Style TUI Model

`config` should behave like a BIOS-style setup utility: mostly visible menus, submenus, selection lists, confirmation dialogs, and explicit save/restart actions. Text input should be used only where text is inherently required, such as subscription URL, proxy name, custom port, or AI chat input.

The visual design should keep the existing black terminal base and use BIOS-like structure without the blue BIOS palette:

- black background panels
- cyan or inverted selected rows
- gray secondary text
- high-contrast status blocks
- no decorative cards

The primary page is `Main`. It should show current runtime status and the list of configured proxies. Up/Down moves between proxies. Enter opens the selected proxy's configuration page.

Top navigation tabs:

```text
Main    Subscription    Runtime    Chat    Exit
```

Responsibilities:

- Main
  - runtime dashboard
  - default System Proxy and user-created Port Proxies
  - enter a proxy to configure subscription/mode/proxy server/listen settings
- Subscription
  - list subscriptions by default
  - show Add Subscription as the final selectable row
  - add subscriptions through one form with Name, URL, Refresh Interval, and OK
  - display update status, traffic, expiry, and usage
- Runtime
  - non-proxy operational settings
  - service install/uninstall/status
  - TUN permission status/install flow
  - logs and diagnostics
  - workdir/core/controller/service details when appropriate
- Chat
  - AI-assisted configuration
  - explain current setup
  - ask user intent in plain language
  - propose config changes
  - apply validated changes through the same config spec used by the TUI
- Exit
  - start, stop, reload, restart
  - save
  - save and restart
  - save, restart, and exit
  - exit without saving
  - load defaults
  - immediate exit

Main should show:

- daemon status
- mihomo status and version
- service/startup status
- workdir
- active subscriptions and refresh state
- each proxy's status
- configured ports/listen addresses
- system proxy state
- TUN state
- traffic usage when available
- current public IP and maybe outbound country/provider when available
- local/LAN IP addresses useful for LAN proxy setup

Example Main layout:

```text
clashtui Setup Utility

Runtime
  Daemon: running pid=12345
  mihomo: online v1.x
  Workdir: ~/Library/Application Support/clashtui
  Public IP: 1.2.3.4 HK

Proxies
  Status  Name             Kind     Listen/System        Mode    Subscription
  OK      System Proxy     System   127.0.0.1:7070       Rule    Airport A
          os=on pac=off tun=off dns=on
          speed up=1.2 MB/s down=5.1 MB/s total down=8.3 GB ipinfo 1.2.3.4 HK
  OK      Work HTTP        HTTP     127.0.0.1:7080       Rule    Work
          allow-lan=off
          speed up=1.2 MB/s down=5.1 MB/s total down=8.3 GB ipinfo 1.2.3.4 HK
  Off     Game SOCKS       SOCKS5   0.0.0.0:7081         Global  Game

Right side:
  Selected proxy config, especially System Proxy, PAC, and TUN.
  Details and key hints appear in the right pane.

F9 Defaults  F10 Save & Restart  Esc Back/Exit
```

Navigation rules:

- Up/Down selects rows or fields.
- Enter opens the selected submenu/action.
- Left/Right changes enum-like choices when safe.
- Esc goes back one page.
- Esc on Main must not exit immediately. It opens an exit confirmation.
- Leaving with unsaved changes must always confirm before discarding or exiting.
- Hotkeys can exist as accelerators, but visible menu actions must be the primary path.

Function keys:

- `F9`: Load setup defaults, after confirmation.
- `F10`: Save and restart runtime, after confirmation.
- `Esc`: Back; on Main, open exit confirmation.

The `Exit` section is the visible command surface for runtime and save/close
actions. It includes `Start`, `Stop`, `Reload`, `Restart`, `Save`, `Save &
Restart`, `Save, Restart & Exit`, `Exit Without Saving`, `Load Defaults`, and a
final `Exit` action that closes the config UI immediately without a popup.

Exit confirmation:

```text
Unsaved changes exist.

Exit Without Saving
Cancel
```

`config` should maintain a dirty state so the user can safely explore and back out.

## Config Sections

`config` should behave more like setup + ongoing preferences, not just a raw config editor.

Expected sections:

- Main
  - runtime dashboard
  - proxy list
  - visible status and warnings
- Proxies
  - default System Proxy
  - user-created Port Proxies
  - per-proxy subscription, mode, group, and proxy selection
- Subscriptions
  - add/update/select
- Runtime
  - service
  - TUN permission status and install action
  - logs
  - diagnostics
  - workdir
  - mihomo core path
  - controller URL/secret
- Chat
  - AI-driven setup and config edits

The setup flow should be progressive:

1. Find or configure mihomo.
2. Confirm workdir.
3. Configure default System Proxy with local mixed port `7070`.
4. Optionally install service.
5. Optionally enable OS system proxy.
6. Optionally enable TUN on the default System Proxy.
7. Optionally add user-created Port Proxies.

## Proxy Configuration Pages

The default System Proxy and user-created Port Proxies should use similar configuration pages. They share routing/subscription fields and differ only in delivery/system fields.

Common routing fields:

```text
Enabled              [ On ]
Name                 [ System Proxy ]
Subscription         [ Airport A ]
Mode                 [ Rule / Global / Direct ]
Rule Settings        [ submenu ]
Global Group         [ GLOBAL ]       # when mode is Global
Proxy Server         [ HK-01 ]        # when mode is Global and group is selected
DNS Behavior         [ submenu ]
```

Default System Proxy fields:

```text
Local Mixed Port     [ 7070 ]
OS System Proxy      [ On / Off ]
PAC                  [ Off / Generated / Custom URL ]
TUN                  [ On / Off ]
TUN Settings         [ submenu ]
Bypass               [ submenu ]
```

User-created Port Proxy fields:

```text
Type                 [ Mixed / HTTP / SOCKS5 ]
Listen               [ 127.0.0.1 / 0.0.0.0 / Custom ]
Port                 [ 7080 ]
LAN Access           [ On / Off ]
UDP                  [ On / Off ]     # hidden for HTTP
```

Complex areas use subpages:

- Rule Settings
- Proxy Group Selection
- TUN Settings
- DNS Settings
- PAC Settings
- Bypass List

The user should not need to understand mihomo `listeners`, `mixed-port`, `mode`, or generated YAML to configure common cases.

## Chat-Assisted Configuration

Chat is a first-class page, not only a help screen. It is the key differentiator of this tool: users can describe what they want in natural language and avoid learning every mihomo/system proxy/TUN term before using the app.

Example user intents:

```text
I want Safari and normal apps to use proxy, but keep TUN off.
Create a SOCKS5 proxy for my game on LAN port 7081 and use the HK node.
Use my work subscription for 127.0.0.1:7080 in rule mode.
Why is TUN not working on macOS?
Show me what will change before applying.
```

The Chat page should support:

- explain current runtime and config
- ask clarifying questions when intent is ambiguous
- propose a change set
- show the exact user-facing summary before applying
- apply changes only after confirmation
- save without applying when requested
- apply/restart runtime when requested
- report validation errors in user terms

Chat must not directly edit generated mihomo runtime YAML. It should edit a stable, documented `clashtui` configuration spec.

### LLM Config Spec

The project needs an LLM-friendly spec that is easier to reason about than raw mihomo YAML and stable enough for automated edits.

Requirements:

- explicit schema version
- semantic names, not raw mihomo implementation names where possible
- stable IDs for subscriptions, proxies, and runtime objects
- normalized enum values
- comments or separate documentation that explain each field
- validation rules that can reject invalid LLM edits
- dry-run diff before save
- ability to map spec to current runtime config

Possible structure:

```yaml
version: 1

subscriptions:
  - id: airport-a
    name: Airport A
    url: https://example.com/sub
    refresh: weekly

proxies:
  - id: system
    kind: system
    name: System Proxy
    enabled: true
    subscription: airport-a
    mode: rule
    local_mixed_port: 7070
    os_proxy: true
    pac:
      mode: off
    tun:
      enabled: false

  - id: game-socks
    kind: port
    name: Game SOCKS
    enabled: true
    subscription: airport-a
    mode: global
    global_group: GLOBAL
    proxy: HK-01
    listener:
      type: socks
      listen: 0.0.0.0
      port: 7081
      udp: true

runtime:
  service:
    mode: user
    enabled: false
  logging:
    level: info
```

This spec is not necessarily the final on-disk config format, but the code should expose a canonical editable model with:

- load current config into spec
- validate spec
- diff spec against current config
- apply spec to `AppConfig`
- write config
- apply runtime changes

LLM edit flow:

1. Build current spec from config and runtime status.
2. Give the LLM the spec plus a concise schema.
3. Ask the LLM to return structured edits, not free-form text-only instructions.
4. Validate edits.
5. Show a human-readable diff.
6. User confirms.
7. Save/apply through normal config paths.

The TUI and Chat should share the same validation and apply logic. Chat should not have a privileged back door.

### Safety Boundaries

- Privileged actions still require explicit user confirmation.
- Chat can propose service/TUN installation, but elevation follows the same TUI flow.
- Chat should never read or collect sudo passwords.
- Chat should not silently delete subscriptions or proxy configs.
- When changing subscription URLs, keep the last good cached snapshot until the new one updates successfully.

## Subscription And Routing Model

Subscriptions are not the same thing as runtime mode, selected proxy, or listener binding. The design should be layered:

```text
Subscription Source -> Traffic Profile -> Proxy Config
```

### Subscription Source

A subscription source is remote data plus the last valid local snapshot:

- display name
- URL
- enabled/disabled
- last successful update time
- last attempted update time
- update error, if any
- traffic usage metadata when available
- expiry metadata when available
- cached profile path

Subscription update failure must not destroy or replace the last good subscription data. The update flow should be atomic:

1. Download to memory or a temporary file.
2. Validate that content is non-empty and parseable enough to be usable.
3. Extract metadata, such as upload/download/total/expire when provided by common subscription headers.
4. Write a new snapshot.
5. Replace the active cached profile only after success.

If update fails:

- keep the last successful snapshot
- show failed status and error
- do not break existing runtime if a previous snapshot exists
- only mark the subscription unusable when it has never had a successful snapshot

### Refresh Policy

Subscriptions should refresh automatically.

- Default auto refresh should be on.
- The app should guarantee at least one refresh attempt per week for enabled subscriptions.
- A shorter interval can be configurable, but the weekly refresh guarantee should be the minimum baseline.
- The daemon should check refresh needs on startup and periodically while running.
- Manual update should still be available from the TUI.

Useful status fields:

```text
Status: OK / Updating / Failed / Stale / Never Updated
Last Updated: local timestamp
Next Refresh: local timestamp or interval
Traffic: used / total / remaining
Expires: date or unknown
Nodes: count, if cheaply available
Used By: proxy configs or traffic profiles referencing it
```

### Traffic Profile

A traffic profile describes how traffic should use a subscription:

- profile name
- selected subscription source
- mode: rule/global/direct
- proxy group selections for rule mode
- selected proxy/group for global mode
- optional fallback behavior if the subscription snapshot is unavailable

This allows different proxy configs to use different subscriptions and different modes without forcing the user to understand raw mihomo config.

Examples:

```text
Work       -> subscription: work-sub  -> mode: rule
Gaming     -> subscription: game-sub  -> mode: global -> proxy: HK-01
Direct LAN -> subscription: none      -> mode: direct
```

### Proxy Config

A proxy config exposes or captures traffic:

- default System Proxy
- optional HTTP/SOCKS/mixed Port Proxy
- system proxy manual mode on the default System Proxy
- system proxy PAC mode on the default System Proxy
- system proxy TUN mode on the default System Proxy

Each proxy config can bind to a traffic profile. System Proxy/TUN still belongs only to the default System Proxy; user-created Port Proxies only expose local/LAN listeners.

The current default runtime implementation has moved to the single-runtime
model: Global Proxy and enabled Port Proxy services are generated into one
mihomo config. Global Proxy is the top-level mixed listener and each Port Proxy
is a mihomo `listener` with its own subscription/mode/proxy intent. Legacy
multi-process backends remain only for compatibility. The remaining design gap
is the higher-level traffic profile/spec layer for Chat and advanced reuse.

## Subscription TUI Design

The subscription page should avoid hidden shortcut-heavy operation. It should follow a setup utility pattern: move selection, press Enter, choose from visible actions.

### List View

Use a left list and right detail panel.

Left list rows:

```text
Status  Name           Updated        Traffic          Expires      Used By
OK      Airport A      2026-05-10     18G / 200G       2026-06-01   System
Failed  Work           2026-05-03     unknown          unknown      Work Port
Stale   Backup         2026-04-20     5G / 50G         2026-12-01   -
```

The first column should use plain status words, not only symbols. The selected subscription detail panel should show:

- full name
- URL, masked or truncated by default
- status and last error
- last successful update
- last attempted update
- next scheduled refresh
- traffic used/remaining/total
- expiry
- cached file path
- which traffic profiles or proxy configs use it

### Actions

Pressing Enter on a subscription should open an action menu:

```text
Update Now
Edit
Assign To...
Set As Default Source
Disable Auto Refresh
Delete
View Cached Profile Info
```

Shortcut keys can remain as accelerators, but the primary UI should expose actions visibly.

### Add Subscription Form

Adding a subscription should be a form, not a single text prompt.

Fields:

```text
Name                 [________________]
URL                  [________________]
Auto Refresh         [ On ]
Refresh Interval     [ Weekly ]
Use After Adding     [ Choose... ]
Initial Assignment   [ None / System Proxy / New Port Proxy / New Traffic Profile ]
```

Buttons:

```text
Test & Save    Save Without Test    Cancel
```

Validation:

- Name is required and unique.
- URL is required and should look like HTTP/HTTPS.
- `Test & Save` downloads the subscription and shows metadata before final save.
- `Save Without Test` is available for offline setup but marks the subscription as never updated.

### Edit Subscription Form

Editing should reuse the same form and preserve last good data when URL changes until the new URL is successfully tested or updated.

Changing URL should show a clear prompt:

```text
Keep old cached profile until the new subscription updates successfully.
```

### Delete Flow

Delete should be explicit and reference usage:

```text
This subscription is used by:
- System Proxy
- Gaming Port

Choose a replacement or remove those bindings before deleting.
```

Options:

```text
Choose Replacement
Remove Bindings
Cancel
```

Avoid silently breaking proxy configs.

### Update Flow

Manual update should show progress:

```text
Downloading...
Validating...
Saving snapshot...
Runtime reload pending / Runtime reloaded
```

If the update fails:

```text
Update failed. Last good snapshot from 2026-05-03 is still in use.
```

The user should not lose working proxy data due to a failed refresh.

## Workdir Model

The application is distributed as a single `clashtui` binary, but it depends on a mihomo core. Both `clashtui` and mihomo need a stable work directory for config, generated runtime files, logs, pid files, profiles, and mihomo `-d` state.

Current workdir model:

```text
CLASHTUI_CONFIG_DIR
  config.yaml
  profiles/
  mihomo-run.yaml
  mihomo-active.yaml
  clashtui.pid
  mihomo.pid
  clashtui.log
  mihomo.log
  runtimes/
    port-proxy-N/
      mihomo-run.yaml
      mihomo-active.yaml
      mihomo.pid
      mihomo.log
```

The top-level mihomo files belong to Global Proxy. Each Port Proxy has an isolated runtime directory. Child mihomo stdout/stderr is collected in that runtime's `mihomo.log` and should never be written to the TUI screen.

This should become a first-class concept in UI and service installation. Service files should pin `CLASHTUI_CONFIG_DIR` explicitly so Linux system services and macOS LaunchDaemons do not accidentally use root's config.

## Capability Model

There are three operational capability levels:

- macOS TUN requires mihomo to run through the privileged service. User-mode
  start can still provide Global Proxy and Port Proxy listeners, but it cannot
  create `utun` or install transparent routes.

1. User-mode runtime
   - Starts the daemon and single mihomo runtime as the current user.
   - Provides Global Proxy and Port Proxy listeners.
   - Does not provide TUN route ownership.

2. Login autostart
   - Used for login/startup auto-run.
   - On macOS this is a user LaunchAgent.
   - It should be driven by config, not more CLI commands.

3. Privileged service
   - Used for TUN device, route changes, and DNS/system integration.
   - Optional. Without it, proxy listener mode remains available.

The product should degrade cleanly:

```text
no privilege        -> port proxy works
login autostart     -> user daemon can start after login
service privilege   -> root/service mihomo can own TUN and routes
```

## Elevation From TUI

TUI elevation is feasible in terminal/console mode, but the TUI should not collect passwords itself.

Correct terminal flow:

1. User chooses a setup action.
2. TUI clearly states what system change will happen.
3. TUI disables raw mode, leaves alternate screen, and shows the cursor.
4. The app runs the privileged helper command with inherited stdio, for example:

   ```bash
   sudo clashtui __service-install-root ...
   ```

5. The OS `sudo` prompt handles password input in the real terminal.
6. After completion, TUI restores raw mode and alternate screen.

Avoid:

- reading passwords in ratatui widgets
- piping passwords via `sudo -S`
- running sudo while still in alternate screen/raw mode

For GUI/no-TTY cases:

- Linux can use `pkexec`/polkit if an authentication agent exists.
- `sudo -A` can use an askpass helper, but only if `SUDO_ASKPASS` or sudo.conf is configured.
- macOS should use launchd/ServiceManagement for persistent privileged helpers; direct privileged execution APIs are deprecated.

## Service Design

The user login autostart path should run the daemon entrypoint directly:

```text
clashtui --daemon-run
```

It should not run `clashtui start`, because the service manager already owns
process lifecycle.

The privileged service path is separate. It installs a root-owned
`clashtui-service`/PrivilegedHelperTools copy that runs `__service-run`, listens
on a Unix socket, authenticates the configured user uid, and starts/stops the
single service-owned mihomo child on request.

The generated service must pin:

- absolute `clashtui` binary path
- explicit `CLASHTUI_CONFIG_DIR`
- working directory equal to `CLASHTUI_CONFIG_DIR`
- log paths if supported by the service manager

### Linux

User service:

- write `~/.config/systemd/user/clashtui.service`
- run `systemctl --user enable --now clashtui.service`
- optionally enable linger for boot without login:

  ```bash
  loginctl enable-linger "$USER"
  ```

System service:

- write `/etc/systemd/system/clashtui.service`
- needs root
- can potentially replace binary `setcap` with unit capabilities:
  - `AmbientCapabilities=CAP_NET_ADMIN CAP_NET_BIND_SERVICE`
  - `CapabilityBoundingSet=CAP_NET_ADMIN CAP_NET_BIND_SERVICE`
  - `User=<target-user>`

System service is better for true boot startup and TUN, but it complicates user config and system proxy/session behavior.

### macOS

LaunchAgent:

- write `~/Library/LaunchAgents/com.clashtui.daemon.plist`
- run as the logged-in user
- good default for login startup
- does not reliably solve TUN/root route permissions

LaunchDaemon:

- write `/Library/LaunchDaemons/com.clashtui.daemon.plist`
- needs root
- starts at boot before login
- better fit for TUN/route permissions
- must explicitly set `CLASHTUI_CONFIG_DIR` to the user's workdir, otherwise it may read root's config

## TUN Permission Design

The old `tun-install` helper/capability path is removed. Both macOS and Linux
now align on `service-install` / `service-uninstall` / `service-status`.

Current behavior:

- macOS installs `/Library/PrivilegedHelperTools/com.clashtui.service` and
  `/Library/LaunchDaemons/com.clashtui.service.plist`.
- Linux installs `/usr/local/libexec/clashtui-service` and
  `/etc/systemd/system/clashtui.service`.
- The service owns the privileged mihomo child and root-owned work directory.
- User-mode fallback still runs the same single-runtime config without TUN.

## Implementation Tasks

- Add platform service abstraction, for example `src/platform/service.rs`.
- Add platform elevation helper, for example `src/platform/elevation.rs`.
- Service commands in CLI:
  - `service-install`
  - `service-uninstall`
  - `service-status`
- Add setup items in TUI/config:
  - Service: installed/running/not installed
  - TUN permissions: installed/missing/not applicable
  - Proxies: default System Proxy and user-created Port Proxies
  - System Proxy fields: OS proxy, PAC, TUN, and system traffic settings
  - Chat: AI-assisted setup and validated config edits
- Add an LLM-editable config spec:
  - schema version
  - stable IDs
  - validation
  - diff/dry-run
  - shared apply path with the TUI
- Redesign `config` as a BIOS-style setup UI:
  - Main runtime dashboard
  - proxy list as the primary selection surface
  - Enter opens selected proxy config
  - Esc backs out through the actual page visit history and confirms before exit/discard
  - Proxy subscription selection stays in the Proxy page as an inline dropdown instead of jumping to the Subscription page
  - F9/F10 BIOS-like defaults/save-restart actions with confirmation
- When privileged action is selected in TUI, pause terminal UI and execute the privileged command with inherited stdio.
- Ensure `start/stop/status` never surprise-prompt for sudo.

## Open Questions

- Should `config` be renamed or aliased as `setup`?
- Should service install/uninstall be exposed only as config actions after the
  current CLI surface stabilizes?
- Should macOS user autostart remain LaunchAgent-only while privileged TUN uses
  LaunchDaemon?
- Should Linux also support a non-root user service for non-TUN autostart?
- Should initial launch automatically enter a setup wizard when no config exists?
- Should port proxy be the explicit default mode in the UI?
- How much of system service/TUN setup should be available as first-run prompts vs advanced settings?

## References

- sudo askpass behavior: https://www.sudo.ws/docs/man/sudo.man/
- polkit `pkexec` authentication agent behavior: https://www.freedesktop.org/software/polkit/docs/master/pkexec.1.html
- macOS launchd service locations: https://support.apple.com/en-kw/guide/terminal/apdc6c1077b-5d5d-4d35-9c19-60f2397b2369/mac
- Apple deprecated direct privileged execution API: https://developer.apple.com/documentation/security/1540038-authorizationexecutewithprivileg
