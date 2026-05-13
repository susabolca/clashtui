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
- Global Proxy and every Port Proxy should have separate mihomo controllers and pid files.
- Read-only status may inspect a configured controller, but write operations must be guarded.
- If an online controller is found without a live `clashtui` mihomo pid file, the write must fail clearly instead of modifying it.

## Implemented

- Controller auto-allocation now starts from `http://127.0.0.1:19090`.
- Legacy default `http://127.0.0.1:9097` migrates to the new default.
- Daemon runtime apply refuses to modify an online controller when no owned mihomo pid exists.
- TUI runtime write operations are guarded before reload, mode patch, proxy selection, and delay test.
- `stop` skips mihomo cleanup when no owned mihomo core is running.
- Global Proxy now uses its own mihomo process for `7070`, TUN, DNS, and system proxy.
- Each Port Proxy now uses its own mihomo process under `runtimes/port-proxy-N/`, with a private controller, pid file, generated config, and log file.
- Child mihomo stdout/stderr is redirected to runtime log files instead of the TUI terminal.

## Follow-Up

- Show an explicit `external/unowned` marker in the TUI runtime status when the configured controller is reachable but not owned.
- Add an explicit advanced mode if users intentionally want to control an external mihomo process.
- Move subscription delay checks to a dedicated check runtime so they do not need to load profiles into Global Proxy.
