# Port Management

## Goal

`clashtui` must avoid port conflicts when multiple local proxy tools or multiple `clashtui` instances exist on the same machine.

## Policy

- User-specified ports are fixed.
- Fixed ports must not be changed automatically.
- If a fixed port is occupied, startup should fail with a clear message.
- Non-user-facing operational ports may use a stable random allocation.
- Global Proxy uses `127.0.0.1:7070` by default and is fixed unless the user edits it.
- A listen host of `0.0.0.0`, for example `0.0.0.0:7070`, means LAN-accessible.
- Auto-managed ports are persisted after allocation so one instance does not drift between restarts.
- New Port Proxy services start at `127.0.0.1:7071`; services with `port: 0` are auto-assigned from the same listener range.
- In the default single-runtime/service backend, Port Proxy services do not get
  private mihomo controller ports; they are listeners inside the one mihomo
  runtime.
- Private Port Proxy controller ports exist only for legacy `multi` /
  `multi-process` compatibility backends.

## Current Auto Ranges

- Controller: `19090-19989`
- Port Proxy controllers: `20090-20989` in legacy multi-process mode only
- Global Proxy mixed port: fixed default `7070`
- DNS listen: `15053-15952`
- Extra listeners: `7071-7970`

## Current Implementation

- `port_allocation.seed` stores a per-config stable random seed.
- `port_allocation.auto_controller` and `auto_dns` decide which top-level operational ports are managed automatically.
- Existing custom controller or DNS values are treated as fixed when no allocation seed exists.
- `auto_mixed` is migrated off so Global Proxy keeps the user-facing `7070` default.
- TUI edits for controller, mixed port, and DNS listen mark those ports fixed.
- `start` allocates and saves auto ports before spawning the daemon.
- The daemon allocates missing listener ports when config is reloaded.
- Startup validates Global Proxy, DNS, and Port Proxy listener ports before
  spawning mihomo.
- Startup validates Port Proxy controller ports only when the configured backend
  is not a single-runtime backend.
- Port checks are validation only. They must not bind and hold user-facing
  listener ports as a readiness strategy, because a successful bind can itself
  delay the next mihomo start from acquiring the port.

## Follow-Up

- Add a TUI page showing auto/fixed status for each port.
- Add an explicit "Make Auto" action to release a fixed user port back to the allocator.
- Show the owning process for conflicts where the platform supports it.
