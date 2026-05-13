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

## Required Design

macOS TUN needs a privileged execution path for the Global Proxy mihomo runtime:

- LaunchDaemon or privileged helper owns Global Proxy mihomo.
- Service file pins `CLASHTUI_CONFIG_DIR` to the user's config directory.
- Port Proxy runtimes can remain user-mode because they do not own TUN.
- TUI should expose this as setup, not as a hidden requirement.

## Follow-Up

- Add macOS `service-install` / `service-uninstall` or a dedicated privileged helper.
- Make TUN setup explain why user-mode start can only provide port/system proxy behavior.
- Consider a "Start without TUN" confirmation when TUN is enabled but privileges are missing.
