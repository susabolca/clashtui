# macOS TUN Helper Design

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

Linux uses `setcap` in `tun-install` to grant the `clashtui` binary the
specific capabilities needed for TUN and DNS setup:

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

Use an audited `clashtui` TUN helper installed as a root-owned LaunchDaemon.

```text
user shell
  -> clashtui start
       -> user-mode clashtui daemon
            -> IPC request to root tun-helper
                 -> create/configure utun
                 -> configure route and optional DNS state
                 -> pass TUN fd back to clashtui
            -> spawn user-mode mihomo with inherited fd
            -> generated config contains tun.file-descriptor
```

Only the helper runs as root. `mihomo` continues to run as the invoking user.

Suggested installed files:

```text
/Library/LaunchDaemons/com.clashtui.tun-helper.plist
/Library/PrivilegedHelperTools/com.clashtui.tun-helper
```

For development, the helper can initially be the same Rust binary with a hidden
entrypoint such as:

```text
clashtui __tun-helper-run
```

During `tun-install`, copy the current binary to the privileged helper location,
make it root-owned, and load it through `launchctl`. This avoids trusting a
user-writable binary path after installation.

## tun-install

On macOS, `tun-install` should:

1. Resolve the current `clashtui` executable.
2. Re-run the hidden root installer through `sudo`.
3. Copy the binary to `/Library/PrivilegedHelperTools/com.clashtui.tun-helper`.
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

On Linux, the existing `setcap` and polkit behavior remains unchanged.

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
- `teardown_tun`

`prepare_tun` input:

- owner UID
- owner PID
- desired device name, either empty or `utun*`
- IPv4 and optional IPv6 TUN addresses
- MTU
- route include and exclude ranges
- DNS hijack settings if needed

`prepare_tun` output:

- actual interface name
- TUN file descriptor passed with Unix domain socket `SCM_RIGHTS`
- applied route and DNS state summary

`teardown_tun` input:

- owner UID
- owner PID or lease id

`status` output:

- helper version
- active owner
- active interface
- routes and DNS state managed by the helper

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

Startup flow:

1. Generate normal mihomo runtime config.
2. If TUN is enabled on macOS, request `prepare_tun` from the helper.
3. Receive the TUN fd.
4. Spawn mihomo as the user process with that fd inherited, for example fd `3`.
5. Write or patch mihomo config:

```yaml
tun:
  enable: true
  file-descriptor: 3
```

6. Keep the helper lease alive while the Global Proxy mihomo process is alive.
7. On stop or failure, call `teardown_tun`.

The exact Rust implementation needs an fd-passing IPC layer and must ensure the
fd is not closed on `exec` before spawning mihomo.

## Status UX

`clashtui status` should clearly distinguish:

- TUN configured
- helper installed
- helper reachable
- active helper owner
- active TUN interface
- mihomo reported `tun.enable`
- current default route

Example status lines:

```text
tun-helper: installed reachable version=...
tun-helper: active interface=utun1024 owner_uid=501 owner_pid=...
mihomo-config: tun.enable=true desired=true device=utun1024 stack=Mixed fd=3
```

If TUN is enabled but the helper is not installed:

```text
tun-helper: missing; run sudo clashtui tun-install
tun: disabled at runtime because macOS requires the helper for utun/routes
```

## Open Questions

- Should the first version use a Unix domain socket or a Mach service?
- Should DNS restore be owned by the helper immediately, or should v1 only own
  `utun` and routes?
- Should helper installation support both a copied helper binary and a
  packaged app helper path?
- How should stale routes be identified if the helper state file is missing?
- Does the current mihomo Darwin build accept `file-descriptor` for top-level
  `tun` exactly as expected, or do we need a minimal direct test config first?

## Implementation Plan

1. Add a macOS hidden helper entrypoint, for example `__tun-helper-run`.
2. Add macOS root installer and uninstaller behind existing `tun-install` and
   `tun-uninstall`.
3. Install a root-owned LaunchDaemon and helper binary.
4. Add minimal helper IPC with `status`.
5. Add `prepare_tun` and `teardown_tun` without DNS changes.
6. Add fd passing from helper to user daemon.
7. Add `tun.file-descriptor` support in `TunConfig` or runtime-only TUN patching.
8. Spawn user-mode mihomo with inherited fd.
9. Add status diagnostics and stale cleanup.
10. Add tests for config generation, installer path validation, and helper
    request validation.

## Acceptance Criteria

- `clashtui tun-install` prompts for sudo once and installs the helper.
- After install, `clashtui start` does not prompt for sudo.
- `mihomo` runs as the normal user, not root.
- The helper runs as root and only exposes the limited TUN control protocol.
- `clashtui status` reports helper availability and actual TUN state.
- `curl --noproxy '*' https://google.com` is routed through TUN when TUN is
  enabled and helper setup succeeds.
- `clashtui stop` and `tun-uninstall` clean routes, DNS state, helper state,
  and the TUN interface.
