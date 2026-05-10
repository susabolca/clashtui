# Architecture

`clashtui` is split into three parts:

1. CLI entrypoint and background daemon.
2. BIOS-style TUI configuration screen.
3. mihomo runtime configuration generation.

The daemon owns the long-running workflow:

1. Load `~/.config/clashtui/config.yaml`.
2. Start mihomo when the controller is offline.
3. Download or load the active subscription profile.
4. Generate `mihomo-active.yaml` by merging the subscription profile with local runtime settings.
5. Reload mihomo through its external controller.
6. Reapply mode and proxy group selections.
7. Watch the user config and retry on changes.

`clashtui` does not proxy traffic itself. HTTP, SOCKS5, TUN, and DNS are delegated to mihomo.

## Runtime Ownership

The runtime model deliberately separates global transparent networking from local port proxy services:

- TUN is a single global mihomo service. `clashtui` writes one top-level `tun` block from local config and removes subscription-provided `listeners`.
- DNS is a single global mihomo service. `clashtui` writes one top-level `dns` block from local config, including LAN DNS policy overrides.
- Port proxy services can be multiple. `mixed_port` is always written, while `proxy_ports.http` and `proxy_ports.socks` are optional local inbounds. For per-port routing, `proxy_ports.services` generates mihomo `listeners` with local `http`, `socks`, or `mixed` inbounds and optional `proxy`/`rule` binding.

Subscription YAML is used for proxies, proxy groups, rules, providers, and related outbound behavior. Top-level inbound keys such as `port`, `socks-port`, `redir-port`, `tproxy-port`, `mixed-port`, `authentication`, and `listeners` are stripped before reload so subscriptions cannot create extra transparent services or surprise local ports.

## Key Files

- `src/main.rs`: CLI parsing and command dispatch.
- `src/daemon.rs`: background lifecycle and retry loop.
- `src/core.rs`: mihomo process start/stop and capability forwarding.
- `src/runtime_profile.rs`: final mihomo YAML generation.
- `src/config_menu.rs`: interactive TUI.
- `src/dns.rs`: mihomo DNS JSON/YAML payload generation.
- `src/tun.rs`: mihomo TUN payload generation.
- `src/privilege.rs`: Linux `setcap` and polkit installation.

## Runtime Paths

The default config root is:

```text
~/.config/clashtui
```

Important generated files:

```text
config.yaml
profiles/*.yaml
mihomo-run.yaml
mihomo-active.yaml
clashtui.log
mihomo.log
clashtui.pid
mihomo.pid
```

Generated runtime files are not part of the source repository.
