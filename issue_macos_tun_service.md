# macOS TUN Service

## Problem

macOS TUN uses `utun` devices and route changes. A normal user-mode `clashtui start` can start mihomo listeners, but it cannot create the TUN interface or install routes.

Observed failure:

```text
Start TUN listening error: configure tun interface: Connect: operation not permitted
```

Runtime symptoms:

- `clashtui status` shows configured `tun=true`.
- Mihomo `/configs` reports `tun.enable=false`.
- `ifconfig utun1024` reports the interface does not exist.
- `curl -x http://127.0.0.1:7070 https://google.com` works.
- `curl --noproxy '*' https://google.com` still follows the normal network path and may fail.

## Current Behavior

- Global Proxy and Port Proxy listeners still work.
- System proxy can point GUI apps at Global Proxy.
- TUN is reported as unavailable in `status` when the process is not privileged.
- The daemon logs a warning instead of repeatedly failing the whole runtime apply.
- In `runtime_backend=service`, an installed and reachable service starts one
  privileged mihomo runtime that owns TUN, DNS, Global Proxy, and all enabled
  Port Proxy listeners.
- If the service is missing or unreachable, `clashtui start` falls back to a
  user-mode single runtime for that run. Proxy listeners still work, but TUN is
  disabled in the runtime copy.

## Required Design

macOS TUN needs a privileged execution path for the Global Proxy mihomo runtime:

- LaunchDaemon owns a small privileged `clashtui` service.
- The service starts one service-owned mihomo process from an authenticated IPC
  request.
- Service-owned/root-owned mihomo uses a root-owned persistent work directory:
  `/Library/Application Support/clashtui/service/<uid>/`.
- `/var/run` is reserved for volatile IPC and pid/state files.
- Port Proxy listeners run inside the same mihomo process as TUN; the old hybrid
  model with root TUN mihomo plus user-mode Port Proxy mihomo is not supported.
- TUI should expose this as setup, not as a hidden requirement.

## Implemented

- `service-install`, `service-uninstall`, and `service-status` are implemented.
- `tun-install` / `tun-uninstall` and the helper-fd path have been removed.
- `service-install` prints a short sudo/elevation explanation before requesting
  root installation.
- Service install/upgrade stops stale service-owned mihomo before replacing the
  LaunchDaemon.
- Service start cleans stale old-architecture user-mode cores before validating
  listener ports.

## Follow-Up

- Surface service install/uninstall from config as settings/actions rather than
  adding more top-level CLI commands.
- Consider a "Start without TUN" confirmation when TUN is enabled but privileges
  are missing.
