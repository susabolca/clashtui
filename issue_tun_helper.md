# TUN Helper Design

## Problem

`clashtui` can enable TUN in its config, but on macOS the current user-mode
runtime cannot actually start the TUN interface.

Observed local state:

- `~/Library/Application Support/clashtui/clashtui.log` reports `tun=true` as
  the desired runtime state.
- `~/Library/Application Support/clashtui/mihomo.log` reports:

```text
Start TUN listening error: configure tun interface: Connect: operation not permitted
```

- `clashtui status` shows `can_start_tun=false`, `is_root=false`.
- mihomo `/configs` reports `tun.enable=false` while `clashtui` config has
  `tun.enable=true`.
- `ifconfig utun1024` fails because the expected interface does not exist.
- `curl --noproxy '*' https://google.com` times out on the normal network path.
- `curl -x http://127.0.0.1:7070 https://google.com` works, which proves the
  local proxy listener is healthy and the failure is specific to transparent
  TUN mode.

Current `curl https://google.com` can be misleading in this environment because
the shell has `HTTPS_PROXY=http://127.0.0.1:7897`, which points at Clash Verge,
not `clashtui`'s default `127.0.0.1:7070`.

## Root Cause

Earlier Linux builds used `setcap` in `tun-install` to grant the `clashtui`
binary the specific capabilities needed for TUN and DNS setup:

- `CAP_NET_ADMIN`
- `CAP_NET_BIND_SERVICE`

macOS does not provide an equivalent to Linux file capabilities. There is no
direct way to grant only "network admin" capability to a single executable.

The macOS failure is not only about opening a TUN device. A complete TUN startup
path can require privileged operations for:

- creating or connecting the `utun` interface
- setting interface addresses, MTU, and flags
- adding and removing routes for `auto-route`
- adjusting DNS behavior or restoring DNS state when TUN stops

Running the external `mihomo` binary as root would solve the permission problem
but expands the trust boundary too far. `mihomo` is an external process and
should remain a normal user process.

## Research

Clash Verge Rev uses a privileged service path for TUN. The UI process talks to
a service over IPC, and the service starts the core with elevated privileges.
Relevant local reference files:

- `../clash-verge-rev/src-tauri/src/core/service.rs`
- `../clash-verge-rev/src-tauri/src/core/manager/state.rs`
- `../clash-verge-rev/src-tauri/src/core/manager/lifecycle.rs`

That approach proves the service model works, but `clashtui` should not copy the
part where the service runs external `mihomo` as root.

mihomo has a `tun.file-descriptor` config field:

- `RawTun.FileDescriptor int yaml:"file-descriptor" json:"file-descriptor"`
- Reference: https://pkg.go.dev/github.com/metacubex/mihomo/config

The underlying `sing-tun` options also include `FileDescriptor`, and the Darwin
implementation supports file-descriptor-backed TUN devices:

- Reference: https://pkg.go.dev/github.com/sagernet/sing-tun

Apple's old `AuthorizationExecuteWithPrivileges` API is deprecated. Apple
recommends using a `launchd` helper or Service Management for privileged helper
work:

- https://developer.apple.com/documentation/security/authorizationexecutewithprivileges
- https://developer.apple.com/documentation/servicemanagement/smappservice

## Goal

Keep the external `mihomo` process unprivileged while still supporting
transparent TUN on macOS after a one-time install step:

```bash
clashtui tun-install
```

The user should enter a sudo password only during `tun-install` and
`tun-uninstall`. Normal commands should not prompt:

```bash
clashtui start
clashtui stop
clashtui status
```

## Proposed Architecture

Use an audited `clashtui` TUN helper installed as a root-owned platform service:
LaunchDaemon on macOS and systemd on Linux. The helper protocol should stay
platform-neutral so both platforms use the same security model instead of
granting capabilities to `clashtui` or passing them to `mihomo`.

```text
user shell
  -> clashtui start
       -> user-mode clashtui supervisor (`clashtui --daemon-run`)
            -> IPC prepare_tun to root tun-helper
                 -> create/configure utun
                 -> pass TUN fd back to clashtui
            -> spawn user-mode mihomo with inherited fd
            -> generated config contains tun.file-descriptor and interface-name
            -> health check mihomo controller
            -> IPC activate_routes to root tun-helper
                 -> configure helper-owned routes and optional DNS state
```

Only the helper runs as root. `mihomo` continues to run as the invoking user.

Current implementation note: `clashtui start` already launches the same
`clashtui` binary with the hidden `--daemon-run` flag. That daemon is the
current user-mode supervisor/shim. It already owns the mihomo child process,
TUN preparation, route activation, health checks, config reloads, and stop-time
cleanup. The remaining work is to harden this daemon into a full supervisor,
not to add a second overlapping user-mode process.

Suggested installed files:

```text
/Library/LaunchDaemons/com.clashtui.tun-helper.plist
/Library/PrivilegedHelperTools/com.clashtui.tun-helper
```

Target distribution should include a separate helper executable, for example
`clashtui-tun-helper`, built from audited helper-specific code. It may share
protocol and platform modules with `clashtui`, but its command surface should be
only the helper service.

For development or the current v0, the helper can temporarily be the same Rust
binary with a hidden entrypoint such as:

```text
clashtui __tun-helper-run
```

During `tun-install`, copy the selected helper artifact to the privileged helper
location, make it root-owned, and load it through the platform service manager
(`launchd` on macOS, systemd for the initial Linux helper). This avoids trusting
a user-writable binary path after installation.

## Linux Helper Feasibility

The same model is feasible on Linux. The low-level implementation differs, but
the trust boundary can be identical:

```text
user clashtui
  -> root tun-helper over Unix socket prepare_tun
       -> open /dev/net/tun
       -> ioctl TUNSETIFF with IFF_TUN | IFF_NO_PI
       -> configure link address and MTU
       -> pass TUN fd back with SCM_RIGHTS
  -> spawn user-mode mihomo with tun.file-descriptor
  -> root tun-helper over Unix socket activate_routes
       -> configure helper-owned routes, policy rules, and optional DNS state
```

Removed legacy Linux behavior was less isolated:

- `tun-install` set `cap_net_admin,cap_net_bind_service+ep` on `clashtui`.
- `clashtui` raised ambient capabilities before spawning `mihomo`.
- As a result, the external `mihomo` process received network-admin capability.

A Linux helper lets us remove that capability path for the normal runtime. The
initial implementation installs a root-owned systemd service under
`/etc/systemd/system`, listens on `/run/com.clashtui.tun-helper.sock`, and
accepts only the installing UID via `SO_PEERCRED`. The `setcap`/ambient
capability path is no longer a runtime fallback; Linux TUN should use the helper
path like macOS.

Linux-specific helper work:

- Use `/dev/net/tun` plus `TUNSETIFF` instead of the Darwin utun kernel control.
- Prefer netlink for address and route changes. Calling `/sbin/ip` is acceptable
  for an initial implementation, but netlink is easier to audit long term.
- Replace macOS `getpeereid` with Linux `SO_PEERCRED`.
- Restrict the systemd unit with `CapabilityBoundingSet=CAP_NET_ADMIN`,
  `NoNewPrivileges=yes`, `ProtectSystem=strict`, and a private runtime
  directory/socket where available.
- Move systemd-resolved DNS changes into the helper if we want to remove the
  current polkit rule too.

The main caveat is loop prevention. Linux can pass the fd just like macOS, but
if the helper installs a default route before `mihomo` has a stable outbound
path, `mihomo` can still route its own proxy or DNS connections back into TUN.
The route activation model should therefore be shared by both platforms.

## tun-install

On macOS, `tun-install` should:

1. Resolve the `clashtui-tun-helper` artifact. Current v0 may fall back to the
   current `clashtui` executable with the hidden helper entrypoint.
2. Re-run the hidden root installer through `sudo`.
3. Copy the helper binary to
   `/Library/PrivilegedHelperTools/com.clashtui.tun-helper`.
4. Set ownership and permissions:

```text
root:wheel 0755 /Library/PrivilegedHelperTools/com.clashtui.tun-helper
root:wheel 0644 /Library/LaunchDaemons/com.clashtui.tun-helper.plist
```

5. Write a LaunchDaemon plist whose `ProgramArguments` only run the helper
   entrypoint, not arbitrary `clashtui` commands.
6. `launchctl bootstrap system` and `launchctl enable system/...`.
7. Verify the helper IPC endpoint is reachable.
8. Print status and next steps.

On Linux, `tun-install` should install or repair the root-owned systemd helper.
The older `setcap`/ambient capability path should not be used as a runtime
fallback; `tun-uninstall` may still remove stale file capabilities from older
installs.

## tun-uninstall

On macOS, `tun-uninstall` should:

1. Re-run the hidden root uninstaller through `sudo`.
2. Ask the helper to clean stale TUN, route, and DNS state if it is reachable.
3. `launchctl bootout system /Library/LaunchDaemons/com.clashtui.tun-helper.plist`.
4. Remove the LaunchDaemon plist and helper binary.
5. Remove any helper runtime socket or state files owned by `clashtui`.

## IPC Contract

The helper must expose a small fixed protocol. It must not accept shell commands
or arbitrary executable paths.

Commands:

- `status`
- `prepare_tun`
- `activate_routes`
- `update_policy`
- `deactivate_routes`
- `teardown_tun`
- `heartbeat`

`prepare_tun` input:

- owner UID
- owner PID
- desired device name, either empty or `utun*`
- IPv4 and optional IPv6 TUN addresses
- MTU
- platform policy mode metadata

`prepare_tun` output:

- lease id
- actual interface name
- TUN file descriptor passed with Unix domain socket `SCM_RIGHTS`
- default route snapshot
- no traffic-capturing routes installed yet

`activate_routes` input:

- lease id
- route/policy specification
- default route snapshot to protect against stale activation

`deactivate_routes` input:

- lease id

`teardown_tun` input:

- owner UID
- owner PID or lease id

`status` output:

- helper version
- active owner and lease
- active interface
- routes, rules, DNS state, and policy state managed by the helper
- lease heartbeat age and cleanup status

## Security Controls

The helper should be deliberately narrow:

- Do not run `mihomo`.
- Do not execute arbitrary commands.
- Do not accept arbitrary paths.
- Do not proxy user traffic.
- Validate caller credentials using the Unix socket peer credentials available
  on macOS.
- Accept requests only from the installing user or an explicitly configured UID.
- Validate `device` as empty or `utun*`.
- Validate route and address ranges before applying them.
- Default TUN address ranges should stay inside reserved ranges such as
  `198.18.0.0/15`.
- Bind helper state to owner UID and owner PID.
- Clean up if the owner process exits or the helper lease expires.
- Log route and DNS changes, but never log subscription URLs, proxy secrets, or
  traffic contents.

If code signing is added later, the helper can also verify the caller's code
signature or Team ID.

## Runtime Integration

`clashtui` should use the helper only for the Global Proxy runtime. Port Proxy
runtimes do not own TUN and should remain fully user-mode.

Runtime ownership should be split:

- the root helper owns only privileged kernel state;
- the existing `clashtui --daemon-run` user daemon should be treated as the
  supervisor/shim and own the active session, the mihomo child process, the
  helper lease, heartbeats, status, and cleanup ordering;
- mihomo remains an ordinary user process and does not speak the helper IPC
  protocol.

Startup flow:

1. `clashtui start` launches or notifies the user-mode supervisor.
2. The supervisor snapshots the current default route and DNS state.
3. If TUN is enabled and helper mode is active, the supervisor requests
   `prepare_tun` from the helper.
4. The helper creates/configures the interface and returns the TUN fd without
   installing traffic-capturing routes.
5. The supervisor writes or patches mihomo config:

```yaml
interface-name: <original default interface>
tun:
  enable: true
  file-descriptor: 3
  auto-route: false
  auto-detect-interface: false
```

6. The supervisor spawns mihomo as the user process with the inherited fd.
7. After mihomo controller health is confirmed, the supervisor asks the helper
   to `activate_routes`.
8. The supervisor keeps the helper lease alive while mihomo is healthy.
9. On `clashtui stop`, failure, or supervisor exit, the supervisor calls
   `deactivate_routes`, stops mihomo, then calls `teardown_tun`.

The exact Rust implementation needs an fd-passing IPC layer and must ensure the
fd is not closed on `exec` before spawning mihomo.

## Loop Avoidance Model

The stable invariant is:

> mihomo's own outbound proxy and DNS sockets must never be selected into the
> same TUN capture path that mihomo is serving.

Static `/32` excludes for currently resolved proxy or DNS IPs are not a complete
safety model. They break when DNS answers rotate, proxy providers update, the
default gateway changes, or the active physical interface changes. After the
macOS scoped-route fix, they should not be part of the normal path.

The helper should own TUN and routes, but route activation should be staged:

1. `prepare_tun` creates and configures the TUN interface, but does not install
   the traffic-capturing default routes yet.
2. `clashtui` writes runtime config with `tun.file-descriptor`, disables mihomo
   `auto-route`, `auto-detect-interface`, and any platform route automation that
   would need elevated permissions.
3. `clashtui` starts user-mode `mihomo` and waits for the controller to become
   healthy.
4. `clashtui` verifies the platform loop-prevention primitive is in place:
   `interface-name` on macOS, or helper-owned UID/GID, cgroup, mark, or routing
   policy on Linux.
5. `clashtui` calls `activate_routes`.
6. If activation or health verification fails, `clashtui` stops mihomo and calls
   `teardown_tun`.

The current implementation uses the two-step flow on macOS. The correctness
boundary is identity/interface based loop prevention, not destination host
excludes. Linux should use helper-owned process identity policy, such as cgroup
or dedicated-UID routing, instead of destination `/32` excludes.

For local helper authorization, UID plus PID is a reliable and auditable owner
anchor: the helper can verify that the supervisor pid and mihomo pid are alive,
and cleanup can be tied to those local processes. For Linux routing, however,
PID by itself is not a stable RPDB selector. The helper should translate the
subject pid into a kernel-visible routing identity, such as a cgroup mark or a
dedicated mihomo UID, before installing capture routes.

### External helper feasibility

mihomo internally knows which sockets are its own upstream proxy and DNS
connections. An external helper does not have that semantic knowledge before the
socket is created, and observing mihomo's controller connection list is too late
for route selection.

Therefore, a helper outside mihomo should not try to reproduce mihomo's dynamic
connection bypass list. A safe external design must route by kernel-visible
identity:

- process UID/GID
- cgroup or service identity
- socket mark or firewall mark
- bound outbound interface

If we require same-user mihomo, no mihomo cooperation, no dedicated service
identity, and no platform firewall/policy-routing support, then the external
helper cannot make a fully stable bypass decision. In that constrained model the
only complete owner is privileged mihomo, because it both creates the sockets and
owns the route automation.

The preferred security model is still not privileged mihomo. Instead:

- use helper-owned TUN and routes;
- use a small non-privileged mihomo cooperation point where available, such as
  `interface-name`;
- or give mihomo a dedicated unprivileged identity so the helper can route that
  identity outside the TUN path.

### mihomo source findings

Checked upstream `MetaCubeX/mihomo` `Meta` branch at commit
`a84724665eb7f989809abe463c05f5723bd24975`.

Relevant behavior:

- `tun.file-descriptor` is a first-class config field and is copied into the TUN
  listener options.
- Top-level `interface-name` and Linux-only `routing-mark` are parsed into the
  global dialer settings.
- All normal outbound dials consult the global dialer settings. On Darwin,
  `interface-name` is applied with `IP_BOUND_IF`/`IPV6_BOUND_IF`. On Linux,
  `interface-name` is applied with `SO_BINDTODEVICE`, and `routing-mark` is
  applied with `SO_MARK`.
- When mihomo owns TUN `auto-route` or `auto-detect-interface`, it starts
  sing-tun's default interface monitor and installs a `DefaultInterfaceFinder`
  into the global dialer. If the selected interface is the TUN itself, mihomo
  returns an invalid interface name to avoid loopback.
- On default interface changes, mihomo flushes interface cache and resets DNS
  resolver connections.
- Linux TUN options include `include-uid`, `include-uid-range`, `exclude-uid`,
  and `exclude-uid-range`. sing-tun turns those into netlink rules with
  `UIDRange` and nftables rules using socket UID matching.
- In Linux fd-backed TUN mode, sing-tun does not configure the interface, route,
  or rules. That means a helper-provided fd implies the helper must own route and
  policy-rule setup.

Implication:

- We do not need privileged mihomo for correctness, but an external helper must
  reproduce the kernel-visible bypass mechanism that mihomo/sing-tun normally
  installs when it owns TUN.
- On Linux, the correct helper-owned bypass primitive is UID/cgroup/fwmark policy
  routing, not `/32` destination excludes.
- On macOS, `interface-name` is real socket binding support and is the least
  invasive cooperation point. A stronger macOS design needs either a dedicated
  mihomo UID/GID plus PF routing, or a Network Extension architecture.

Local macOS POC:

- As normal user `euid=501`, setting `IP_BOUND_IF` and `IPV6_BOUND_IF` on TCP
  sockets for the current default interface `en0` succeeded.
- This verifies that mihomo can bind its own outbound sockets through
  `interface-name` without running as root.

## Ideal Design

The ideal design is a cross-platform helper-owned TUN control plane with
unprivileged mihomo as the data-plane proxy process.

Non-goals:

- Do not run external mihomo as root.
- Do not pass `CAP_NET_ADMIN` to mihomo.
- Do not use startup-time resolved proxy/DNS IPs as the primary loop-prevention
  mechanism.
- Do not let the helper execute arbitrary user commands or arbitrary binaries.
- Do not rely on a mutable user-owned binary path after installation.

Core invariant:

- The helper owns privileged kernel state: TUN device, interface addresses,
  routes, policy rules, firewall marks, and DNS resolver integration.
- mihomo owns proxy protocol logic and packet processing only.
- mihomo's own outbound sockets must be routed outside the TUN capture path by
  a kernel-visible selector: bound interface, UID/GID, cgroup, or mark.

### Helper Packaging

Ship two executables for normal distribution:

- `clashtui`, the user-facing CLI/TUI and user-mode supervisor;
- `clashtui-tun-helper`, the privileged helper executable.

The helper can live in the same repository and share audited protocol/platform
modules, but it should have a separate binary target with no normal CLI/TUI
commands. The current same-binary hidden entrypoint is acceptable as a v0
bootstrap path, not the ideal packaging shape:

```text
clashtui __tun-helper-run
```

`tun-install` is the only command that needs sudo. It copies the selected helper
artifact into a root-owned helper location and installs the platform service
definition that invokes only the helper executable.

macOS installed form:

```text
/Library/PrivilegedHelperTools/com.clashtui.tun-helper
/Library/LaunchDaemons/com.clashtui.tun-helper.plist
ProgramArguments = [
  "/Library/PrivilegedHelperTools/com.clashtui.tun-helper"
]
```

Linux installed form:

```text
/usr/local/libexec/clashtui-tun-helper
/etc/systemd/system/clashtui-tun-helper.service
/run/com.clashtui.tun-helper.sock
ExecStart=/usr/local/libexec/clashtui-tun-helper
```

Security properties:

- the helper path is a root-owned copy, not a symlink to a user-writable path;
- permissions are `root:wheel 0755` on macOS and `root:root 0755` on Linux;
- the service definition is root-owned and calls only the helper executable;
- the helper executable exposes a fixed IPC protocol and has no normal CLI/TUI
  command surface;
- `status` reports helper version and should eventually report the installed
  binary hash;
- replacing the helper artifact requires running `tun-install` again to update
  the privileged copy.

This keeps distribution simple while avoiding the unsafe pattern of launchd or
systemd running a mutable user-owned binary as root.

### Lifecycle Model

There are two separate lifetimes.

Install lifetime:

- `clashtui tun-install` prompts for sudo, installs the root-owned helper copy,
  installs the platform service definition, starts or reloads the helper, and
  performs a stale-state cleanup check.
- The installed helper may remain available between user sessions, but it must
  not keep an active TUN interface, route, DNS override, firewall rule, or
  policy rule unless there is a live lease.
- `clashtui tun-uninstall` prompts for sudo, asks the helper to clean stale
  state, stops the service, removes the helper binary and service definition,
  and removes helper-owned runtime files.

Session lifetime:

- `clashtui start` creates one active user-mode session. If TUN is enabled, the
  session obtains a helper lease, receives the TUN fd, starts mihomo as the
  invoking user, waits for health, then activates routes.
- `clashtui stop` ends the active session. It should deactivate routes first,
  stop mihomo, tear down the helper lease, close the TUN fd, and then let the
  user-mode supervisor exit.
- Helper install/uninstall is therefore an administrative lifecycle; TUN
  prepare/activate/deactivate/teardown is a runtime lifecycle tied to
  `clashtui start` and `clashtui stop`.

### User-Mode Supervisor

`clashtui start` already hands runtime ownership to a user-mode supervisor by
spawning the same binary as `clashtui --daemon-run`. No separate user-mode shim
binary is required for the current design. The daemon should be formalized as
the session supervisor, while the root helper remains a separate privileged
binary with a narrow IPC surface.

Responsibilities:

- own the mihomo child process PID and termination policy;
- own the helper `lease_id` and send heartbeats;
- own the inherited TUN fd until mihomo is spawned;
- order startup as prepare TUN, start mihomo, verify health, activate routes;
- order shutdown as deactivate routes, stop mihomo, teardown TUN;
- watch helper reachability, mihomo exit, default route changes, config reloads,
  and explicit `clashtui stop`;
- expose session status to `clashtui status`;
- perform best-effort cleanup on SIGTERM/SIGINT and rely on helper lease expiry
  for crash cleanup.

The root helper must not manage mihomo directly. The helper has no reason to
know subscription URLs, proxy credentials, runtime config paths, or arbitrary
executable paths.

### Mihomo Runtime Contract

When helper mode is active, `clashtui` should generate runtime config like:

```yaml
interface-name: <original default interface>
tun:
  enable: true
  file-descriptor: <fd passed by helper>
  auto-route: false
  auto-detect-interface: false
  auto-redirect: false
```

Rules:

- `file-descriptor` is runtime-only and must never be persisted to
  `config.yaml`.
- `auto-route`, `auto-detect-interface`, and `auto-redirect` must be disabled
  because helper mode means mihomo no longer owns privileged route automation.
- `interface-name` is required on macOS and useful as secondary protection on
  Linux only if the runtime has permission to bind sockets to an interface. On
  macOS it binds sockets with `IP_BOUND_IF`/`IPV6_BOUND_IF` and works without
  root.
- Linux `routing-mark` should not be the default helper contract because setting
  `SO_MARK` requires elevated capability. It is acceptable only if we
  deliberately choose a model where mihomo receives that narrow capability,
  which is not the preferred security boundary.
- Linux `include-uid`/`exclude-uid` in mihomo config are not sufficient in
  helper fd mode because sing-tun skips interface/route/rule setup when a fd is
  supplied. The helper must implement equivalent policy rules itself.

### Helper IPC Contract

The helper protocol should be lease based and fixed:

- `status`
- `prepare_tun`
- `activate_routes`
- `update_policy`
- `deactivate_routes`
- `teardown_tun`
- `heartbeat`

`prepare_tun`:

- validates caller credentials;
- creates or opens the TUN device;
- configures address, MTU, and link state;
- does not install traffic-capturing default routes;
- returns `lease_id`, interface name, fd over `SCM_RIGHTS`, and the current
  default route snapshot.

`activate_routes`:

- accepts `lease_id`;
- verifies the owner process is alive;
- installs only helper-owned route and policy state;
- records every kernel object for exact cleanup;
- rolls back completely on failure.

`update_policy`:

- updates helper-owned policy state after network changes;
- must be idempotent;
- must not require the helper to know subscription secrets or proxy passwords.

`deactivate_routes`:

- removes traffic-capturing routes/rules while leaving the TUN fd alive if the
  owner is restarting mihomo.

`teardown_tun`:

- removes routes, rules, DNS state, firewall anchors, cgroups, sockets, and TUN
  interface state tied to the lease.

`heartbeat`:

- extends the active lease while the daemon and mihomo process remain alive;
- stale leases are torn down automatically.

### Startup State Machine

The supervisor-managed runtime must be two-phase:

1. Snapshot current default route: interface, gateway, DNS state, and timestamp.
2. Request `prepare_tun`.
3. Write runtime mihomo config with `tun.file-descriptor` and helper-safe route
   options.
4. Start mihomo as an unprivileged process.
5. Attach mihomo to its Linux cgroup or verify its macOS `interface-name`
   binding prerequisites.
6. Wait for mihomo controller health.
7. Verify `/configs` reports TUN enabled and expected fd/interface settings.
8. Request `activate_routes`.
9. Run a bounded health probe through TUN.
10. If any step fails before activation, stop mihomo and call `teardown_tun`.
11. If any step fails after activation, call `deactivate_routes`, stop mihomo,
    then call `teardown_tun`.

This avoids the current race where global routes are changed before mihomo's own
outbound path is stable.

### macOS Target

Primary macOS design:

- Install a root-owned LaunchDaemon helper.
- Use the helper to create `utun`, configure address/MTU/up, and pass the fd.
- Use top-level `interface-name` to bind mihomo's outbound sockets to the
  original default interface.
- Treat default route interface/gateway changes as a restart boundary:
  deactivate helper routes first, restart mihomo with the new `interface-name`,
  then reactivate routes.
- Do not use resolved `/32` proxy/DNS excludes in the normal path; scoped routes
  are the current correctness boundary.

macOS hardening option:

- Create a dedicated unprivileged mihomo UID/GID.
- The helper manages a narrow PF anchor that matches that UID/GID and routes the
  matched sockets through the original gateway with `route-to`.
- Adopt this only after proving it can be installed, enabled, disabled, and
  removed without touching unrelated user PF configuration.

macOS long-term product option:

- Implement a Network Extension Packet Tunnel or Per-App VPN design.
- This is the most platform-native model, but it requires app packaging,
  signing, entitlements, and a different distribution model.

### Linux Target

Primary Linux design:

- Install a root-owned helper as a systemd service/socket.
- Restrict the unit with `CapabilityBoundingSet=CAP_NET_ADMIN
  CAP_NET_BIND_SERVICE`, `NoNewPrivileges=yes`, `ProtectSystem=strict`, and a
  private runtime directory where available.
- The helper opens `/dev/net/tun`, performs `TUNSETIFF`, configures the link, and
  passes the fd to user-mode `clashtui`/mihomo.
- The helper owns a dedicated route table for TUN traffic.
- The helper installs RPDB/nftables policy so mihomo's own sockets bypass TUN by
  identity.

Preferred Linux policy options:

- Dedicated unprivileged mihomo UID/GID:
  - rule priority A: `uidrange <mihomo_uid>-<mihomo_uid> lookup main`
  - rule priority B: ordinary traffic lookup helper TUN table
  - no capability is passed to mihomo
- Same invoking UID with cgroup identity:
  - helper creates a dedicated cgroup for the mihomo process;
  - helper attaches the mihomo PID before route activation;
  - nftables marks sockets from that cgroup;
  - RPDB routes that mark through the original/default table;
  - ordinary traffic uses the TUN table.

The dedicated UID model is simpler and easier to audit. The cgroup model better
preserves same-user file access but is more complex.

### Network Change Handling

The helper must monitor or be driven by events for:

- default interface changes;
- default gateway changes;
- DNS resolver changes;
- suspend/resume;
- Wi-Fi changes;
- profile/provider changes.

On change:

1. `deactivate_routes` immediately removes capture routes/rules.
2. Recompute outbound interface/gateway and helper policy.
3. Restart mihomo if `interface-name` or process identity changed.
4. `activate_routes`.
5. Run a bounded health probe.

If recovery fails, TUN stays down and port/system proxy modes can continue.

### Safety Checks

Before activating routes:

- helper lease owner UID matches the installing UID;
- owner PID and mihomo PID are alive;
- fd is open and belongs to the expected TUN interface;
- mihomo controller is healthy;
- runtime config reports `tun.enable=true`;
- default route snapshot is still current;
- cleanup state was persisted enough to remove all helper-owned kernel objects
  after a crash.

After activation:

- direct `curl --noproxy '*'` reaches through TUN within a short timeout;
- proxy curl through `127.0.0.1:<mixed-port>` still reaches;
- helper status reports active interface, routes/rules count, owner UID/PID,
  and route snapshot.

### Rollout Plan

1. Split current `prepare_tun` into `prepare_tun` and `activate_routes`. Done.
2. Add a separate `clashtui-tun-helper` binary artifact. Done.
3. Refactor macOS helper protocol into platform-neutral commands with leases.
4. Add route-change detection and automatic deactivate/restart/reactivate.
5. Remove automatic `/32` proxy/DNS route excludes from the normal path. Done.
6. Implement Linux helper with fd passing and a dedicated route table. Initial
   implementation done behind guarded activation; needs Linux host validation.
7. Add Linux dedicated UID or cgroup policy routing before making helper routes
   the default Linux path.
8. Remove ambient capability passing to mihomo when helper fd mode is active.
   Done for the runtime path; remaining references are legacy cleanup/status.
9. Evaluate macOS PF-anchor hardening separately.

### Research References

- mihomo documents top-level `interface-name` as mihomo's outbound interface.
- mihomo/sing-tun expose `file-descriptor`, route include/exclude, and Linux UID
  include/exclude options.
- sing-box documents `auto_route` loop avoidance through default-interface or
  outbound interface binding.
- Linux `ip-rule(8)` documents RPDB selectors including `uidrange` and `fwmark`.
- macOS/OpenBSD `pf.conf(5)` documents `user`/`group` socket matching and
  `route-to`.
- Apple Network Extension documents included/excluded routes, route enforcement,
  and per-app VPN rules, but that path requires the Network Extension packaging
  and entitlement model.

## Status UX

`clashtui status` should clearly distinguish:

- TUN configured
- helper installed
- helper reachable
- active helper owner
- active supervisor/session
- active TUN interface
- mihomo reported `tun.enable`
- current default route

Example status lines:

```text
tun-helper: installed reachable version=...
tun-session: active lease=... supervisor_pid=... mihomo_pid=...
tun-helper: active interface=utun1024 owner_uid=501 routes=active
mihomo-config: tun.enable=true desired=true device=utun1024 stack=Mixed fd=3
```

If TUN is enabled but the helper is not installed:

```text
tun-helper: missing; run clashtui tun-install
tun: disabled at runtime because macOS requires the helper for utun/routes
```

## Open Questions

- Should the first version use a Unix domain socket or a Mach service?
- Should DNS restore be owned by the helper immediately, or should v1 only own
  `utun` and routes?
- Should helper installation support both a copied helper binary and a
  packaged app helper path?
- Should macOS hardening use only `interface-name`, or should we require a
  dedicated mihomo UID/GID plus a helper-owned PF anchor?
- On Linux, should the preferred identity selector be a dedicated mihomo UID/GID
  or a same-user cgroup/nftables mark?
- How should stale routes/rules be identified if both the helper state file and
  the user-mode supervisor state are missing?

## Implementation Plan

Current v0 started as a narrow macOS proof point and now includes the initial
Linux helper skeleton:

1. Add a macOS hidden helper entrypoint in the `clashtui` binary, for example
   `__tun-helper-run`. Done.
2. Add macOS root installer and uninstaller behind existing `tun-install` and
   `tun-uninstall`. Done.
3. Install a root-owned LaunchDaemon and helper binary. Done.
4. Add minimal helper IPC with `status`. Done.
5. Add `prepare_tun` and `teardown_tun`. Done.
6. Add fd passing from helper to user daemon. Done.
7. Add runtime-only `tun.file-descriptor` patching. Done.
8. Spawn user-mode mihomo with inherited fd. Done.
9. Set top-level `interface-name` from the original default route. Done.
10. Remove temporary `/32` proxy/DNS host-route fallback from the normal path.
    Done.
11. Split helper route setup so `prepare_tun` returns the fd first and
    `activate_routes` runs only after mihomo controller health. Done.
12. Add `deactivate_routes` and call it before stopping/restarting the global
    mihomo process. Done.
13. Add a separate `clashtui-tun-helper` binary target and make `tun-install`
    prefer that artifact when it exists next to `clashtui`. Done.
14. Add status diagnostics and stale owner cleanup. Partially done.
15. Add tests for config generation, installer path validation, and helper
    request validation. Partially done.

Target implementation work:

1. Move the current helper IPC to explicit lease ids and heartbeats.
2. Remove the same-binary helper fallback after release packaging reliably ships
   `clashtui-tun-helper`.
3. Harden the existing `clashtui --daemon-run` daemon into the full user-mode
   supervisor for mihomo, helper lease, status, and cleanup lifecycle.
4. Add default-route and DNS-change detection with deactivate/restart/reactivate.
5. Add Linux helper-owned cgroup or dedicated-UID loop-avoidance policy.
6. Add default-route and DNS-change policy updates.
7. Keep legacy Linux file-capability handling limited to stale install detection
   and uninstall cleanup.

Current implementation status:

- `clashtui tun-install` on macOS copies the selected helper artifact to
  `/Library/PrivilegedHelperTools/com.clashtui.tun-helper`.
- If a sibling `clashtui-tun-helper` artifact exists, `tun-install` installs it
  instead of the user-facing `clashtui` binary. If it is missing, development
  builds still fall back to `clashtui __tun-helper-run`.
- It writes and loads
  `/Library/LaunchDaemons/com.clashtui.tun-helper.plist`.
- If the separate helper artifact is installed, the LaunchDaemon runs the helper
  executable directly. If development fallback is used, the LaunchDaemon runs
  `clashtui __tun-helper-run` as root.
- On Linux, `tun-install` now targets a root-owned systemd helper at
  `/usr/local/libexec/clashtui-tun-helper` with service unit
  `/etc/systemd/system/clashtui-tun-helper.service` and socket
  `/run/com.clashtui.tun-helper.sock`. This still needs native Linux build and
  live validation.
- `clashtui start` launches the same `clashtui` binary as `clashtui
  --daemon-run`. This daemon is the current supervisor/shim process and is not
  root.
- The helper listens on `/var/run/com.clashtui.tun-helper.sock` on macOS and
  `/run/com.clashtui.tun-helper.sock` on Linux.
- The helper accepts the installed user's UID and root only.
- `status`, `prepare_tun`, `activate_routes`, `deactivate_routes`, and
  `teardown_tun` are implemented.
- On Linux, `prepare_tun` opens `/dev/net/tun`, performs `TUNSETIFF`,
  configures the link, and returns the fd over `SCM_RIGHTS`. `activate_routes`
  requires the daemon to pass the mihomo `subject_pid`, verifies that pid is
  alive and owned by the invoking UID, and records it for stale cleanup.
- Linux route activation is currently guarded behind
  `CLASHTUI_LINUX_TUN_EXPERIMENTAL_ROUTES=1` because the current dedicated route
  table does not yet include the cgroup/fwmark policy needed to make mihomo's
  own sockets bypass TUN. The guarded test script opts in for bounded live
  validation and always calls `clashtui stop`.
- `prepare_tun` creates the requested `utunN` through the utun kernel control,
  configures the interface, records the original default route snapshot, and
  returns the fd to user-mode `clashtui` over `SCM_RIGHTS` without installing
  traffic-capturing routes.
- `clashtui` starts mihomo with the inherited fd, waits for the controller to
  become healthy, then asks the helper to `activate_routes`.
- `deactivate_routes` removes helper-owned capture routes while leaving the TUN
  interface alive for a restart path.
- Same-UID cleanup is allowed after the recorded owner PID exits, so
  `clashtui stop` can recover routes even though the daemon that created the
  lease has already been terminated.
- The helper listener is nonblocking and checks the recorded owner PID once per
  second. If the owner disappears, it tears down helper-owned routes and brings
  the TUN interface down.
- `clashtui` clears `FD_CLOEXEC`, writes `tun.file-descriptor` into the runtime
  mihomo config/patch, and starts mihomo as the normal user with the inherited
  fd.
- Helper fd mode disables mihomo `auto-route` and `tun.auto-detect-interface`.
  clashtui records the default route before asking the helper to add split TUN
  routes, and newer helpers also return the same interface. clashtui writes
  that original interface as top-level `interface-name`. This is the macOS
  stable loop-prevention primitive and a secondary Linux hint, not the final
  Linux route policy.
- On macOS, helper route activation installs scoped split routes through the
  original outbound interface before installing unscoped split routes to the
  TUN interface. This lets normal captured traffic enter TUN while sockets bound
  by mihomo to the original interface still have a valid upstream route.
- Automatic proxy/DNS `/32` host-route generation has been removed from the
  normal macOS path after scoped routes proved sufficient in the guarded live
  test.
- `tun.file_descriptor` is runtime-only and is not persisted to
  `config.yaml`.
- When TUN is disabled or clashtui stops, clashtui deactivates helper-owned
  routes before stopping mihomo, then tears down helper-owned TUN state.
- Still pending: explicit lease ids/heartbeats, full supervisor semantics in
  the existing daemon, network-change recovery, richer status output for active
  helper state, Linux native validation, and Linux cgroup/fwmark or dedicated
  UID loop-avoidance policy.

Live test note:

- A first fd-helper run successfully started mihomo and TUN traffic reached
  mihomo, but data did not pass through. The mihomo log showed its own outbound
  DNS/proxy connections sourced from `198.18.0.1` and then rejected as loopback
  traffic.
- Root cause: the helper installed split default routes before mihomo selected
  its outbound interface, so mihomo's own outbound path could be routed back
  into the TUN interface.
- Fix: record the original default interface before `prepare_tun`, write it as
  top-level `interface-name`, and keep mihomo route management disabled in
  helper fd mode.
- The two-phase path and scoped-route fix passed a guarded 60-second macOS live
  test on 2026-05-13. During the active window, the route table contained
  unscoped `0/1` and `128.0/1` routes to `utun1024` plus scoped `0/1` and
  `128.0/1` routes through the original `en0` gateway. `route -n get
  182.140.194.244` selected `utun1024`, while `route -n get -ifscope en0
  182.140.194.244` selected `192.168.0.1` on `en0`.
- The same live test returned HTTP/2 301 for both explicit proxy curl and
  `curl --noproxy '*' https://google.com`. New mihomo logs showed Domestic
  DIRECT connections as successful info entries rather than the previous
  `network is unreachable` warnings.
- After the guarded test stopped clashtui, status showed daemon/mihomo stopped,
  system proxy disabled, `utun1024` removed, split routes removed, and the
  default route restored through `192.168.0.1` on `en0`.
- Added `scripts/tun_guarded_test.sh` for macOS and Linux live testing. It
  refuses to run if the installed helper differs from the freshly built helper
  artifact, and it always exits through `clashtui stop` with a 60-second
  watchdog.

## Remaining Implementation Review

The current helper v0 has the right process shape on macOS and an initial Linux
implementation behind guarded route activation. macOS has passed a guarded live
test, but the issue is not complete. Remaining work:

1. Lease protocol hardening.
   Keep uid/pid as the local ownership anchor and add a `lease_id` for
   idempotency, stale-state correlation, and status. Require the matching
   owner pid plus lease for `activate_routes`, `deactivate_routes`,
   `teardown_tun`, and future `update_policy`, add heartbeat/expiry, and make
   stale cleanup independent of a best-effort stop path.
2. Supervisor hardening in `clashtui --daemon-run`.
   Treat the daemon as the official user-mode supervisor. Add deterministic
   signal handling, child-exit handling, session status, last-error state,
   idempotent cleanup, and clearer restart ordering when config or health
   changes occur.
3. Network-change recovery.
   Watch default route, outbound interface, gateway, and DNS/upstream changes.
   On relevant changes, deactivate helper routes first, stop/restart mihomo with
   the new `interface-name` and fd, then reactivate routes. This is the main
   remaining loop-stability work.
4. Linux loop policy.
   Replace the guarded initial dedicated-route-table activation with cgroup or
   dedicated-UID policy so mihomo's own sockets bypass TUN without granting
   mihomo `CAP_NET_ADMIN`. PID remains an ownership/cleanup anchor; it is not
   the final Linux routing selector.
5. Status and diagnostics.
   First pass done: healthy `start`, `stop`, and stopped `status` paths no
   longer dump log tails by default. Continue redesigning output around phases
   and failures. In healthy cases, show concise process, helper, TUN, proxy, and
   route state. On failure, put the failing phase, command, last error, and
   suggested repair first. Extend helper `status` with active session/lease id,
   supervisor pid, mihomo pid, interface, route count/details, route active
   state, last activation error, and helper/version mismatch signals.
6. Packaging cleanup.
   Ensure release packaging always ships `clashtui-tun-helper` next to
   `clashtui`, then remove or gate the same-binary `__tun-helper-run` fallback.
7. Linux helper mode.
   Native-validate the Linux root helper with `/dev/net/tun` fd passing,
   systemd install/uninstall, `SO_PEERCRED`, guarded route
   activation/deactivation, subject-pid validation, and cleanup. Then add
   netlink and cgroup/dedicated-UID loop avoidance. The runtime path no longer
   depends on ambient `cap_net_admin` for mihomo; keep only stale capability
   detection/removal for older installs.
8. Broader tests.
   Add unit tests around lease validation and route command generation, helper
   IPC failure/rollback tests, supervisor restart/cleanup tests, and guarded
   live smoke tests for network-change scenarios.

## Acceptance Criteria

- `clashtui tun-install` prompts for sudo once and installs the helper.
- Normal distribution provides a separate `clashtui-tun-helper` artifact; the
  installed root-owned helper is not a mutable user-owned `clashtui` path.
- `tun-install` alone does not leave active TUN interfaces, routes, DNS state,
  firewall state, or policy rules behind.
- After install, `clashtui start` does not prompt for sudo.
- `clashtui start` creates a user-mode supervised session that owns the mihomo
  child process and helper lease.
- `mihomo` runs as the normal user, not root.
- The helper runs as root and only exposes the limited TUN control protocol.
- `clashtui status` reports helper availability, active session, lease,
  interface, route/policy state, and mihomo TUN state.
- `curl --noproxy '*' https://google.com` is routed through TUN when TUN is
  enabled and helper setup succeeds.
- `clashtui stop` deactivates routes, stops mihomo, tears down the helper lease,
  and removes the TUN interface without sudo.
- `tun-uninstall` prompts for sudo, performs final stale-state cleanup, stops
  the helper service, and removes installed helper files.
