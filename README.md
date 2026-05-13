# clashtui

`clashtui` is a small Rust TUI and background controller for mihomo / Clash.Meta.

It does not implement a proxy core. It starts and controls mihomo, loads subscription profiles, and applies runtime settings for:

- subscriptions and proxy group selection
- Global Proxy mixed HTTP/SOCKS5 port, default `7070`
- optional Port Proxies, each backed by its own mihomo process
- system proxy
- TUN mode
- mihomo DNS, including LAN DNS policies

## Build

```bash
cargo build --release
```

The binary is written to:

```bash
target/release/clashtui
```

## Commands

```bash
clashtui config
clashtui start
clashtui stop
clashtui status
clashtui tun-install
clashtui tun-uninstall
```

During development:

```bash
cargo run -- config
cargo run -- start
cargo run -- status
```

## TUN Setup

### Linux

TUN needs `CAP_NET_ADMIN`. DNS link updates through systemd-resolved also need a small polkit rule. Run this once after building or replacing the binary:

```bash
sudo target/release/clashtui tun-install
```

After that, normal commands do not need sudo:

```bash
target/release/clashtui start
target/release/clashtui stop
```

Remove those permissions with:

```bash
sudo target/release/clashtui tun-uninstall
```

### macOS

macOS uses `utun` devices. The default generated TUN device is `utun1024`, and Linux-only `auto-redirect` is omitted from the runtime mihomo patch on macOS.

`tun-install` and `tun-uninstall` manage Linux capabilities only. On macOS, TUN requires a privileged mihomo process because creating `utun` devices and changing routes are privileged operations. If `clashtui status` reports `tun.enable=false` while `tun=true` is configured, or `ifconfig utun1024` says the interface does not exist, the current process is not privileged enough.

Port Proxy and system proxy can still work without TUN:

```bash
curl -x http://127.0.0.1:7070 -I https://google.com
```

Transparent TUN traffic requires running the Global Proxy mihomo through a privileged service/helper on macOS. Until that service path exists, a plain user-mode `clashtui start` should be treated as port/system-proxy mode even when TUN is configured.

## TUI

Open the BIOS-style setup screen:

```bash
clashtui config
```

Navigation:

- `Tab` / `Shift+Tab`: switch pages
- `Up` / `Down`: move in the current menu
- `Left` / `Right`: switch proxy group panes
- `Enter`: edit or apply the selected item
- `F1`, `h`, `?`: help
- `F10`, `q`: save and exit

The DNS page supports LAN-specific DNS:

```text
LAN Domains: +.lan, +.local, +.corp.local
LAN DNS: system, 192.168.0.1
Direct DNS: system, 192.168.0.1
Direct follows policy: on
```

These values are rendered to mihomo `nameserver-policy`, `direct-nameserver`, and `fake-ip-filter`.

## Runtime Model

`clashtui` uses one mihomo runtime per configured proxy entry:

- Global Proxy owns the default `mixed_port` `127.0.0.1:7070`.
- Global Proxy is the only runtime allowed to own TUN, DNS, and system proxy settings.
- Each Port Proxy owns a separate mihomo process with its own workdir, config, controller, pid file, and log file.
- Subscription profiles are treated as proxy/rule sources; subscription-provided inbound ports and listeners are removed before generated runtime configs are written.

Port proxy services are local settings. Each service exposes one HTTP, SOCKS5, or mixed listener and can use its own subscription, mode, and selected proxy. For different port-to-proxy needs, add services in `~/.config/clashtui/config.yaml`:

```yaml
proxy_ports:
  services:
    - name: hk-mixed
      kind: mixed
      listen: 127.0.0.1
      port: 7080
      subscription: oist
      mode: global
      proxy: HK-01
```

`kind` can be `http`, `socks`, or `mixed`. In `global` mode, `proxy` names a concrete proxy node from the service subscription. In `rule` mode, `rule_selections` stores group-to-node choices.

## Runtime Isolation

`clashtui` starts and manages its own mihomo processes by default. The Global Proxy controller auto-allocation range starts at `http://127.0.0.1:19090` to avoid Clash Verge's common controller port `9097`.

Runtime write operations refuse to modify an already-online controller unless `clashtui` has a live owned mihomo pid file. This prevents accidental reloads or config patches against Clash Verge's mihomo instance.

Port Proxy controllers are assigned from a separate stable range and are checked for conflicts before startup.

## Port Management

Ports can be auto-managed or fixed. User-specified ports are fixed and are never changed automatically; if a fixed port is occupied, startup fails with a clear error.

Default user-facing proxy ports are stable:

- Global Proxy mixed port: `127.0.0.1:7070`
- New Port Proxy listeners: start at `127.0.0.1:7071`

Set the listen host to `0.0.0.0`, for example `0.0.0.0:7070`, to make a listener reachable from the LAN.

Other auto-managed ports are assigned from stable per-config ranges and then saved:

- controller: `19090-19989`
- Port Proxy controllers: `20090-20989`
- DNS listen: `15053-15952`
- extra listeners: `7071-7970`

Set a port proxy service `port` to `0` to let `clashtui` assign it. Set a non-zero port to keep it fixed.

## Runtime Files

User config lives outside the repository:

```text
~/.config/clashtui/config.yaml
~/.config/clashtui/profiles/
~/.config/clashtui/mihomo-run.yaml
~/.config/clashtui/mihomo-active.yaml
~/.config/clashtui/*.log
~/.config/clashtui/runtimes/port-proxy-N/
```

Each Port Proxy runtime directory contains its own `mihomo-run.yaml`, `mihomo-active.yaml`, `mihomo.pid`, and `mihomo.log`. Child mihomo stdout/stderr is redirected to those log files and is never written into the TUI terminal.

## Documentation

Design notes are in [`doc/`](doc/):

- [`doc/architecture.md`](doc/architecture.md)
- [`doc/dns-design.md`](doc/dns-design.md)
- [`doc/system-proxy-tun-dns-modes.md`](doc/system-proxy-tun-dns-modes.md)
