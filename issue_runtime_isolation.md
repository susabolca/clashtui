# Runtime Isolation

## Problem

`clashtui` previously used `http://127.0.0.1:9097` as the default mihomo external controller. Clash Verge commonly runs `verge-mihomo` with the same controller port.

If Clash Verge is already running, `clashtui` can see the controller as online and skip starting its own mihomo process. Later runtime write operations then call:

- `PUT /configs?force=true`
- `PATCH /configs`
- `PUT /proxies/{group}`

against Clash Verge's mihomo. This reloads or patches the wrong process.

## Required Behavior

- `clashtui` should use a private default controller port.
- `clashtui` should only mutate a mihomo controller it owns.
- In the default single-runtime/service backend, Global Proxy and Port Proxy
  listeners share one mihomo controller and one generated runtime config.
- Legacy `multi` / `multi-process` compatibility backends may still use one
  controller per mihomo process.
- Read-only status may inspect a configured controller, but write operations must be guarded.
- If an online controller is found without a live `clashtui` mihomo pid file, the write must fail clearly instead of modifying it.

## Implemented

- Controller auto-allocation now starts from `http://127.0.0.1:19090`.
- Legacy default `http://127.0.0.1:9097` migrates to the new default.
- Daemon runtime apply refuses to modify an online controller when no owned mihomo pid exists.
- TUI runtime write operations are guarded before reload, mode patch, proxy selection, and delay test.
- `stop` skips mihomo cleanup when no owned mihomo core is running.
- `runtime_backend=service` and `runtime_backend=single` use one generated
  mihomo config: Global Proxy is the top-level `mixed-port`, and enabled Port
  Proxy entries are emitted as mihomo `listeners`.
- Service mode reports the mihomo pid from service IPC instead of trusting the
  old user-mode global pid file.
- If service mode is configured but the service is unreachable, `start` falls
  back to a user-mode single runtime for that run and disables TUN only in the
  runtime copy.
- The service backend cleans up stale user-mode global and Port Proxy mihomo
  processes from the old architecture before port validation and service start.
- Child mihomo stdout/stderr is redirected to runtime log files instead of the
  TUI terminal.

## Follow-Up

- Show an explicit `external/unowned` marker in the TUI runtime status when the configured controller is reachable but not owned.
- Add an explicit advanced mode if users intentionally want to control an external mihomo process.
- Move subscription delay checks to a dedicated check runtime so they do not need to load profiles into Global Proxy.
