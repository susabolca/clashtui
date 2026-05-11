# clashtui

`clashtui` is a small Rust TUI and background controller for mihomo / Clash.Meta.

It does not implement a proxy core. It starts or controls mihomo, loads subscription profiles, and applies runtime settings for:

- subscriptions and proxy group selection
- mixed HTTP/SOCKS5 proxy port, default `7070`
- optional extra HTTP and SOCKS5 proxy ports
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

`tun-install` and `tun-uninstall` manage Linux capabilities only. On macOS, run mihomo with enough privileges for TUN/route changes, or use a privileged helper/service for the core.

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

`clashtui` keeps TUN and DNS global: only one mihomo process owns one top-level `tun` block and one top-level `dns` block. Subscription profiles are treated as proxy/rule sources; subscription-provided inbound ports and `listeners` are removed before runtime reload.

Port proxy services are local settings. The default `mixed_port` is `7070` and accepts both HTTP(S) and SOCKS5 clients. Optional `proxy_ports.http` and `proxy_ports.socks` can be enabled in the TUI or config file when separate HTTP/SOCKS ports are needed.

For different port-to-proxy needs, add local listener services in `~/.config/clashtui/config.yaml`:

```yaml
proxy_ports:
  services:
    - name: hk-mixed
      kind: mixed
      listen: 127.0.0.1
      port: 7080
      proxy: GLOBAL
```

`kind` can be `http`, `socks`, or `mixed`. `proxy` may name a proxy group or proxy node from the active subscription.

## Runtime Files

User config lives outside the repository:

```text
~/.config/clashtui/config.yaml
~/.config/clashtui/profiles/
~/.config/clashtui/mihomo-run.yaml
~/.config/clashtui/mihomo-active.yaml
~/.config/clashtui/*.log
```

## Documentation

Design notes are in [`doc/`](doc/):

- [`doc/architecture.md`](doc/architecture.md)
- [`doc/dns-design.md`](doc/dns-design.md)
- [`doc/system-proxy-tun-dns-modes.md`](doc/system-proxy-tun-dns-modes.md)
