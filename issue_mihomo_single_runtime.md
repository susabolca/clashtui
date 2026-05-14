# Single Mihomo Runtime for TUN and Port Proxy

## Problem

`clashtui` originally separated responsibilities across multiple mihomo-related
runtimes:

- Global Proxy owns normal local proxy, optional TUN, DNS, and system proxy.
- Each Port Proxy owns its own mihomo process, controller, generated config,
  log file, and runtime directory.
- The old macOS TUN path used a privileged helper to prepare a TUN file
  descriptor and activate routes for a user-mode mihomo process.

This works for simple local proxy use, but it becomes fragile around TUN,
sleep/wake, and multi-runtime interaction.

Observed local failure after macOS sleep/wake:

- The machine slept on `2026-05-13 19:11:34` and woke on
  `2026-05-14 06:20:18`.
- mihomo logs after wake contained repeated `network is unreachable` errors.
- `clashtui` supervisor restarted parts of the runtime, but status could still
  report stale process state from inside the sandboxed execution environment.
- `utun1024` was still visible with `198.18.0.1/30`.
- Routes still contained split default routes through `utun1024`:
  `0/1 -> utun1024` and `128.0/1 -> utun1024`.
- The helper log contained owner-pid cleanup messages and route delete failures,
  for example `route: bad address: utun1024`.

The core problem is not that a TUN device cannot survive sleep. The problem is
that transparent proxying depends on a coherent state graph:

```text
system routes
  -> utun interface
  -> mihomo TUN fd
  -> mihomo routing/DNS state
  -> outbound sockets
  -> physical default interface and gateway
```

Sleep/wake can change the physical interface, default route, DNS state, socket
state, and reachability while leaving the `utun` interface itself present. If
only part of the graph is refreshed, traffic may either blackhole through a
stale TUN path or leak through the physical interface depending on the order of
cleanup and restart.

## Current Supervisor Gap

The current `clashtui` supervisor loop is health-check based. It can notice that
a controller is unhealthy and re-apply runtime state, but it does not yet own a
complete TUN recovery protocol.

Missing pieces:

- explicit sleep/wake event handling
- default route and network-change handling
- child-process exit handling as a first-class event
- TUN lease/session ownership
- route transaction rollback
- fail-open versus fail-closed policy
- make-before-break TUN replacement

This is why "restart mihomo after wake" is not enough. If mihomo is stopped
while split routes still point to the old TUN, traffic blackholes. If TUN routes
are removed first, traffic can leak directly through the physical interface.

The helper-fd model therefore needs much more than a restart hook. It needs a
state machine that can safely coordinate route ownership, TUN fd ownership, and
mihomo process ownership.

## Mihomo Source Findings

Local source inspected at `../mihomo`.

### TUN lifecycle

mihomo parses `tun` config into `listener/config.Tun`, then creates or recreates
the TUN listener through:

```text
config.RawTun
  -> listener/config.Tun
  -> listener.ReCreateTun()
  -> listener/sing_tun.New()
  -> github.com/metacubex/sing-tun
```

Important behavior:

- If `tun.file-descriptor` is set, mihomo uses the inherited TUN fd instead of
  opening the device itself.
- On macOS it discovers the actual utun name from the fd with
  `UTUN_OPT_IFNAME`.
- If `auto-route` or `auto-detect-interface` is enabled, mihomo starts network
  and default-interface monitors.
- On default-interface changes, mihomo flushes interface cache and resets
  resolver connections.
- The outbound dialer can bind sockets to the selected real interface to avoid
  routing its own outbound traffic back into the TUN.

This means mihomo is already designed to handle TUN and default-interface
changes when it owns the relevant runtime state. The removed `clashtui`
helper-fd design split that ownership: the helper owned privileged route/TUN
setup, while user-mode mihomo owned the fd and routing logic after startup.

### Multiple port listeners

mihomo supports multiple local inbound listeners in a single process through
`listeners`.

Each listener has independent:

- `name`
- `type`
- `listen`
- `port`
- `proxy`
- `rule`

The `proxy` field routes all traffic from that listener directly to a named
proxy or proxy group. The `rule` field selects a sub-rule set for that listener.
Global rules can also match listener identity through `IN-NAME`.

Example:

```yaml
listeners:
  - name: us-port
    type: mixed
    listen: 127.0.0.1
    port: 7071
    proxy: US-GROUP

  - name: jp-port
    type: mixed
    listen: 127.0.0.1
    port: 7072
    proxy: JP-GROUP
```

Therefore a separate mihomo process is not required just to provide multiple
port proxy listeners.

### Multiple subscriptions

A single mihomo process can use multiple subscription node sources through
`proxy-providers`.

Example:

```yaml
proxy-providers:
  sub-us:
    type: http
    url: "https://example.com/us.yaml"
    interval: 3600
    path: ./providers/sub-us.yaml

  sub-jp:
    type: http
    url: "https://example.com/jp.yaml"
    interval: 3600
    path: ./providers/sub-jp.yaml

proxy-groups:
  - name: US-GROUP
    type: select
    use:
      - sub-us

  - name: JP-GROUP
    type: select
    use:
      - sub-jp
```

The important boundary is that mihomo still has only one final runtime config.
Multiple `proxy-providers` are supported, but multiple complete independent
profiles are not isolated automatically. If two subscriptions are full Clash or
mihomo profiles with their own groups, rules, DNS, and mode, `clashtui` must
merge them into one generated config and namespace their objects.

## Architecture Options

### Option A: Keep helper-fd TUN and harden supervisor

Keep the current direction:

```text
user clashtui daemon
  -> root helper prepares utun fd
  -> user mihomo inherits fd
  -> root helper installs routes
```

Required additional work:

- wake/sleep event listener
- network/default route watcher
- session lease between helper and mihomo pid
- explicit TUN route transaction model
- safe restart order
- stale utun cleanup
- make-before-break support or explicit fail-closed behavior

This preserves the "mihomo is not root" security boundary, but the recovery
protocol is complex. The current helper supports one active TUN lease, so
make-before-break is not available yet.

### Option B: Root TUN mihomo plus user-mode Port Proxy mihomo processes

Run a privileged/root mihomo for global TUN, and keep current user-mode mihomo
processes for Port Proxy.

This looks attractive because it isolates Port Proxy profiles, but it has a
serious routing problem:

```text
application
  -> 127.0.0.1:port-proxy
  -> user-mode mihomo outbound socket
  -> system route
  -> root TUN mihomo captures it again
```

Without special bypass handling, the root TUN mihomo can capture the outbound
traffic of user-mode Port Proxy mihomo processes. This can cause double proxy,
rule confusion, loops, or unexpected policy behavior.

Possible mitigations:

- force user-mode Port Proxy mihomo outbound sockets to bind the physical
  interface with `interface-name`
- add route exemptions
- use process/uid based exclusion where supported
- update all bypass state on network changes

Those mitigations are fragile because they depend on default-interface tracking
outside the process that owns TUN.

### Option C: Single root/service mihomo runtime for TUN and all port listeners

Run one mihomo runtime that owns:

- TUN
- DNS
- system proxy target
- all local Port Proxy listeners
- all proxy providers and groups

This avoids the cross-process capture problem because Port Proxy listeners and
TUN outbound routing live inside the same mihomo process.

Example generated shape:

```yaml
tun:
  enable: true
  stack: gvisor
  auto-route: true
  auto-detect-interface: true

proxy-providers:
  port-1-sub:
    type: http
    url: "..."
    path: ./providers/port-1-sub.yaml

  port-2-sub:
    type: http
    url: "..."
    path: ./providers/port-2-sub.yaml

proxy-groups:
  - name: port-1-select
    type: select
    use:
      - port-1-sub

  - name: port-2-select
    type: select
    use:
      - port-2-sub

listeners:
  - name: port-proxy-1
    type: mixed
    listen: 127.0.0.1
    port: 7071
    proxy: port-1-select

  - name: port-proxy-2
    type: mixed
    listen: 127.0.0.1
    port: 7072
    proxy: port-2-select
```

Tradeoffs:

- Lower process and controller management complexity.
- No root-TUN-captures-user-mihomo loop.
- mihomo gets to own TUN, route, default-interface, DNS, and listeners together.
- Config reload blast radius is larger: changing one Port Proxy reloads the
  single runtime.
- Full profile isolation is lost unless `clashtui` generates namespaced groups,
  rules, providers, and selector state.
- Local listeners run inside the privileged/service-owned mihomo process if the
  runtime is root-owned.

## Recommended Direction

Use Option C as the target architecture:

```text
clashtui TUI / CLI
  -> user-mode config manager
  -> privileged service
       -> starts one mihomo runtime
       -> generated config contains TUN + all listeners
       -> mihomo owns route/default-interface behavior
```

Rationale:

- It removes the most dangerous hybrid mode: root TUN process plus separate
  user-mode Port Proxy mihomo processes.
- It uses mihomo's native support for multiple listeners and proxy providers.
- It lets mihomo handle TUN and default-interface changes in the same process
  that owns outbound sockets.
- It reduces the number of controller ports, pid files, and process ownership
  edges that `clashtui` must coordinate.

This is a larger architecture change, but it is cleaner than trying to make
multiple independent mihomo processes coexist behind one global TUN.

## Privilege Model Decision

For the single-runtime target, use a Clash Verge Rev-like service mode. Do not
continue the main TUN path on `tun-install` helper-fd ownership.

The old `tun-install` implementation was a privileged service in the
OS-installation sense, but it was only a TUN helper:

- installed `com.clashtui.tun-helper` as a root LaunchDaemon or system service
- created/configured the TUN interface
- passed the TUN fd to user-mode mihomo
- installed/removed routes on behalf of the user process
- authenticated local IPC by peer credentials and configured uid

That model kept mihomo out of root, which was a smaller trust boundary, but it
also preserved the split ownership that caused the sleep/wake fragility:

```text
root helper owns utun/routes
user mihomo owns fd/DNS/outbound sockets/controller
clashtui daemon tries to coordinate both
```

The service model aligns ownership:

```text
root service owns mihomo process
mihomo owns TUN/routes/default-interface monitor/DNS/outbound sockets
user app owns config/UI/control plane
```

mihomo's local source supports this direction. When it creates TUN itself, it
can start the sing-tun network/default-interface monitors for `auto-route` and
`auto-detect-interface`, reset resolver connections on default-interface
changes, and bind outbound sockets to the detected real interface. If a TUN fd
is injected by an external helper, mihomo can use it, but route ownership is
still split outside the core.

Decision:

- Delete the old `tun-install` / `tun-uninstall` / helper-fd implementation
  instead of keeping it as a compatibility backend.
- Use `service-install`, `service-uninstall`, and `service-status` as the
  user-facing privilege commands.
- On macOS, install a root LaunchDaemon `com.clashtui.service` that runs
  `__service-run`, listens on `/var/run/com.clashtui.service.sock`, and starts
  mihomo on request.
- In service mode, start one mihomo process directly
  with a generated config containing TUN, DNS, system proxy target, and all Port
  Proxy listeners.
- Do not introduce the hybrid mode "root TUN mihomo plus user-mode Port Proxy
  mihomo" as a supported architecture.

This is a larger trust boundary than helper-fd because the selected mihomo
binary runs privileged. The service therefore needs a narrower IPC contract than
the UI process:

- authenticate IPC by peer credentials and configured uid
- require controller secret in service mode
- bind generated listeners to loopback by default
- validate `core_path`, `config_path`, `config_dir`, and log paths
- track and stop the exact child pid
- avoid `pgrep`, process-name kills, and arbitrary root command execution
- keep generated user-owned files and root-owned service files separate

Dropping privileges after TUN creation is not the first implementation target.
On macOS, later route/default-interface changes still need privileged handling;
dropping root would require a richer broker protocol and would bring back much
of the helper-fd complexity.

## Required Config Generator Changes

`clashtui` needs a new generated-config model:

- Convert each Port Proxy into a mihomo `listener`.
- Convert each Port Proxy subscription into a namespaced `proxy-provider`.
- Generate per-port `proxy-groups`.
- Prefix or otherwise namespace all generated object names.
- Prevent collisions with reserved names such as `DIRECT`, `REJECT`, `GLOBAL`,
  and mihomo provider reserved names.
- Preserve per-port selection state by using distinct group names.
- Represent per-port fixed proxy behavior with listener `proxy`.
- Represent per-port rule behavior with listener `rule` and `sub-rules`.
- Keep one external controller for the single mihomo runtime.

For full Clash/mihomo profile imports, `clashtui` must decide whether to:

- support only node-provider style subscriptions for the first version; or
- implement a full profile merger for proxies, groups, rules, rule providers,
  DNS, and provider settings.

The first version should prefer the smaller scope: multiple node subscriptions
and per-port groups/listeners.

## Source-Level Refactor Plan

This refactor should simplify the runtime ownership model before touching the
large TUI surface.

Target source ownership:

- `src/runtime_profile.rs`
  - owns generated mihomo YAML
  - adds a single-runtime config path that emits one config with global TUN/DNS
    plus all enabled Port Proxy `listeners`
  - first version imports the referenced fixed proxy/group from a Port Proxy's
    own subscription profile so existing fixed-proxy Port Proxy services can run
    in one mihomo process
- `src/core.rs`
  - service backend asks the privileged service to start the single mihomo
    process
  - legacy/multi backend keeps ordinary user-mode mihomo processes, without the
    removed helper-fd TUN path
  - service backend stops starting extra Port Proxy mihomo processes
- `src/daemon.rs`
  - replace `apply_global_runtime + apply_port_proxy_runtimes` with one
    single-runtime apply path behind a backend switch
  - runtime health becomes one controller health check
  - Port Proxy apply becomes listener config generation plus proxy/rule
    selection inside the same mihomo runtime
- `src/port_allocator.rs`
  - remove per-service controller port allocation from the single-runtime path
  - keep listener port validation/allocation
- `src/config_menu.rs`
  - keep the UI model initially, but change runtime fetch to one controller
  - represent Port Proxy status as listeners/groups inside the main runtime
  - stop showing Port Proxy as separate mihomo process/controllers in
    single-runtime mode
- `src/service.rs`
  - own service installation, removal, status, and IPC
  - authenticate requests by peer uid
  - track and stop the exact mihomo child pid

Files that may justify `.rs.bak` replacement once the backend switch is ready:

- `src/core.rs`
- `src/daemon.rs`
- `src/runtime_profile.rs`
- later, possibly `src/config_menu.rs`

Do not start with `config_menu.rs`. It has the largest blast radius and should
consume a stable runtime-status abstraction rather than drive the backend
refactor.

Initial code step:

- Add `write_single_runtime_config()` to `runtime_profile.rs`.
- Generate all enabled Port Proxy entries into one `listeners` array.
- Honor listener `proxy` and `rule`, matching mihomo's inbound `proxy`/`rule`
  support.
- Keep old `write_service_config()` so legacy multi-process mode remains
  buildable during migration.
- Merge fixed-proxy Port Proxy subscriptions by importing the selected proxy or
  group into the main generated config. Full namespaced profile merging remains
  a later feature.

Current implementation checkpoint:

- `AppConfig.runtime_backend` defaults to `service`; `service` uses the
  privileged service when reachable, `single` uses the same generated
  single-runtime config in user mode, and `legacy`, `multi`, and
  `multi-process` keep ordinary user-mode multi-process behavior.
- `service-install`, `service-uninstall`, and `service-status` are implemented.
- macOS service mode installs `/Library/PrivilegedHelperTools/com.clashtui.service`
  and `/Library/LaunchDaemons/com.clashtui.service.plist`.
- Linux service mode installs `/usr/local/libexec/clashtui-service` and
  `/etc/systemd/system/clashtui.service`, binds
  `/run/com.clashtui.service.sock`, and uses `SO_PEERCRED` to authenticate the
  allowed user uid.
- The service IPC accepts `status`, `start_core`, and `stop_core`; it validates
  peer uid and tracks the exact child pid.
- The service persists the current core pid under `/var/run` so a launchd
  service restart can still report or clean up the last service-owned mihomo.
- The old `tun-install`, `tun-uninstall`, `__tun-*`, helper binary, and
  `src/privilege/*` implementation are removed.
- The service backend stops stale Port Proxy mihomo child runtimes and applies
  one generated mihomo config through the service-owned mihomo process.
- The service copies the generated runtime config into a root-owned persistent
  mihomo work directory before starting service-owned mihomo. Root mihomo no
  longer uses the user's config directory as `-d`.
- The service backend also stops the old user-mode global mihomo pid during
  migration, so stale pre-service cores do not keep `7070` or TUN state occupied.
- `service-install` and `service-uninstall` remove the old
  `com.clashtui.tun-helper` LaunchDaemon/systemd unit, helper binary, socket,
  and log because the `tun-uninstall` command no longer exists in the new
  binary.
- `service-install` stops any service-owned mihomo before unloading/replacing
  the LaunchDaemon, avoiding orphaned root mihomo processes during service
  upgrades.
- Login autostart is config-driven through `autostart.enabled`; no additional
  CLI command is added. When `clashtui start` runs, it syncs
  `~/Library/LaunchAgents/com.clashtui.daemon.plist` for the next login. The
  LaunchAgent runs the same binary with `--daemon-run`, while the root
  LaunchDaemon continues to own only the privileged service.
- The interactive config UI now edits a draft config. Ordinary edits mark the
  draft dirty but do not write `config.yaml`, patch mihomo, sync autostart,
  delete runtime files, or stop listeners immediately. `Exit Without Saving`
  discards the draft.
- `Save & Restart` and `Save, Restart & Exit` are the boundary where the draft
  is written and the saved config is applied. They now perform a full
  `clashtui` daemon/runtime restart instead of only calling mihomo
  `reload-config`.
- The TUI executes restart through a captured child process, so restart
  stdout/stderr is shown in status/alert UI instead of being written directly
  into the ratatui alternate screen.
- Config UI runtime commands `Start`, `Stop`, `Reload`, `Restart`, and
  `Save & Restart` show a progress popup while work is running, so a slow
  restart does not look like a frozen TUI.
- A public `restart` CLI command exists for the same operation, so manual
  recovery no longer requires `stop` followed by `start`.
- `start`, `status`, `stop`, and `restart` now default to concise health
  output. `--verbose` expands paths, detailed mihomo config, network route
  inspection, and log locations. The flag is global, so both
  `clashtui --verbose status` and `clashtui status --verbose` work.
- Service runtime cleanup now treats the service-owned root work directory as
  the final ownership signal. If service in-memory child state or persisted pid
  state is lost after service replacement/restart, the service scans for mihomo
  processes running with `-d <service-work-dir> -f <service-work-dir>/mihomo-run.yaml`
  and stops them before validating ports or starting a new core.
- `start` in service mode performs a pre-start runtime cleanup before port
  validation, so stale user-mode global/Port Proxy mihomo processes from the old
  architecture do not block `7070`, `7071`, or `7072`.
- If `runtime_backend=service` but the service is not reachable, `start` falls
  back to user-mode `runtime_backend=single` for that run and disables TUN only
  in the runtime copy. Global Proxy and Port Proxy listeners remain available.
- When the service later becomes reachable again, the daemon stops the
  user-mode single mihomo before switching back to service-owned mihomo.
- `status` in service mode reports `service-mihomo-pid` from the service IPC
  instead of treating the old global pid file as the source of truth.
- The Main screen service status now reads the current service/core status
  instead of stale per-process assumptions, so a running service-owned mihomo is
  not shown as `not running`.
- Subscription/profile data used by the TUI is cached by a background refresh
  task. Foreground navigation no longer parses large subscription YAML files or
  tails logs synchronously on each refresh tick.
- The Subscription list is a maintenance list: each existing subscription uses a
  selectable first line with last-update/state and a low-attention second line
  with proxy count/traffic/expiry; the final `Add Subscription` action is a
  single-line row.
- Service-owned mihomo is verified running as one root child process, listening
  on `127.0.0.1:7070`, `127.0.0.1:7071`, and `127.0.0.1:7072`. No listener is
  present on `7890`.
- Latest proxy smoke tests through `7070`, `7071`, and `7072` all returned
  `HTTP/2 204`.
- The single/service backend uses one controller health check and no
  per-Port-Proxy controller ports.
- A hidden diagnostic command, `__write-single-runtime-config`, writes the
  generated single-runtime config for inspection/testing.
- Local verification used the current config shape:
  - Global Proxy from active profile `oist`
  - Port Proxy 1 from subscription `cnix`, fixed proxy
    `香港 A10 三网精品BGP 5倍流量计费`
  - Port Proxy 2 from subscription `amy`, fixed proxy `🇯🇵 日本 01`
- The generated config contains one `mixed-port` and two `listeners`, and
  `verge-mihomo -t` accepts it after geodata files are available.
- `cargo check` and `cargo test --no-fail-fast` pass on macOS with the service
  backend and without the old helper implementation.
- `cargo check --target x86_64-unknown-linux-gnu` was attempted, but this macOS
  machine lacks `x86_64-linux-gnu-gcc`. Retrying with clang also fails because
  there is no Linux sysroot (`stdlib.h`, `sys/types.h`, and
  `linux/random.h` are missing). Both attempts stop in `aws-lc-sys` before
  reaching clashtui's Linux code.
- `service-status` works without root and reports the service as not installed
  when `/var/run/com.clashtui.service.sock` is absent.
- After installing the service, launchd reports `com.clashtui.service` as
  running and `/var/run/com.clashtui.service.sock` is created. The Codex sandbox
  cannot connect to that socket (`Operation not permitted`), so live start/stop
  testing must be run from a normal terminal.
- A copy of the real local config generates a single-runtime config with
  `mixed-port: 7070` plus listeners on `127.0.0.1:7071` and `127.0.0.1:7072`;
  `verge-mihomo -t` accepts the generated file.

## Privilege and Security Requirements

If the single runtime is service-owned/root-owned:

- Bind local listeners to `127.0.0.1` by default.
- Require explicit user configuration for non-loopback listeners.
- Protect the external controller with a secret.
- Keep root-owned mihomo runtime files out of the ordinary user's config
  directory. Root/service mihomo must not use the user config directory as
  mihomo `-d`.
- Use `/run` or `/var/run` only for volatile service IPC/state such as Unix
  sockets, pid files, and small service-owned state files.
- Use a persistent root-owned work directory for root/service mihomo:
  `/Library/Application Support/clashtui/service/<uid>/` on macOS and
  `/var/lib/clashtui/<uid>/` on Linux.
- Copy or render the generated mihomo runtime config into that root-owned work
  directory before starting root mihomo. The root mihomo command should look
  like `mihomo -d <root-work-dir> -f <root-work-dir>/mihomo-run.yaml`.
- User-mode `runtime_backend=single` and legacy/multi-process mihomo runtimes
  continue to use the user's config directory and user-owned runtime files.
- Never rely on recursive `chown` after root mihomo exits as the primary safety
  mechanism. It can hide permission damage but does not provide a clean trust
  boundary.
- Root-owned files must not appear in the user's config directory. If they do,
  user-mode fallback may fail to update `cache.db`, provider cache files,
  geodata, pid files, or logs.
- Avoid process-name based kill logic.
- Track the exact service pid and controller secret.
- Keep helper/service IPC minimal and authenticated.

This separation is not optional. It prevents root permission pollution in user
files and avoids using a user-writable directory as the root mihomo write
surface.

## Migration Plan

1. Default to `runtime_backend = service`.
2. Build a generated config containing Global Proxy TUN/DNS plus all Port Proxy
   listeners.
3. Install/start/stop mihomo through the privileged service when service mode is
   active.
4. Move service-owned/root-owned mihomo `-d` to the root-owned persistent work
   directory and reserve `/run` or `/var/run` for sockets and pid/state files.
5. Remove helper-fd TUN and `tun-install` commands.
6. Support node-provider subscriptions first; reject or warn on full-profile
   subscriptions that cannot be safely merged.
7. Add stronger namespacing for provider, group, listener, and sub-rule names.
8. Move Port Proxy status from "process/controller per port" to "listener/group
   inside the single runtime".
9. Add controller APIs for selecting per-port groups by generated group name.
10. Keep ordinary user-mode multi-process mode behind a compatibility backend.

## Open Questions

- Is full Clash/mihomo profile merging required, or are node-provider
  subscriptions enough for Port Proxy?
- What should the default failure policy be on wake recovery: fail-closed or
  fail-open?
- How should selector state be stored per generated Port Proxy group?
- Should non-loopback Port Proxy listeners be allowed in privileged runtime mode?
- How much of Clash Verge Rev's service model should be reused conceptually
  without copying its exact trust boundary?

## Decision

The near-term implementation should not add a hybrid mode where root TUN mihomo
and user-mode Port Proxy mihomo processes run side by side without a strong
bypass mechanism. That mode is easy to start but hard to make correct after
network changes.

The safer long-term design is one mihomo runtime that owns both TUN and all
Port Proxy listeners, with `clashtui` generating a merged, namespaced config.
