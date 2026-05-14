# clashtui Config Spec

This document is the editing contract for `clashtui` user config. It is intended
for humans and LLMs that need to modify `config.yaml` quickly without reading the
whole codebase.

## Edit Target

Edit only the user config file:

```text
macOS:   ~/Library/Application Support/clashtui/config.yaml
Linux:   ${XDG_CONFIG_HOME:-~/.config}/clashtui/config.yaml
Windows: %APPDATA%\clashtui\config.yaml
```

`CLASHTUI_CONFIG_DIR=/path/to/dir` overrides the directory.

Do not edit generated runtime files:

```text
mihomo-run.yaml
mihomo-active.yaml
profiles/*
cores/*
*.log
*.pid
runtimes/*
```

`profiles/` contains downloaded subscription YAML. `cores/` contains managed
mihomo binaries and metadata.

## Persistence And Apply Semantics

- `clashtui config` edits a draft. `Save` writes `config.yaml` only.
- `Save & Restart` writes `config.yaml` and restarts the owned runtime.
- `clashtui restart` applies the saved config.
- `clashtui start` starts from the saved config and downloads a managed mihomo
  core only when the selected/local core is missing.
- `clashtui reload` reloads mihomo from saved config without stopping it. Prefer
  restart for listener ports, TUN, DNS listen address, runtime backend, or core
  changes.

The config is serialized by Rust `serde` structs with defaults. Missing fields
receive defaults. Unknown fields and YAML comments are not preserved when
`clashtui` saves the file.

## Top-Level Shape

```yaml
mihomo:
  core: auto
  update: manual
core_path: null
controller:
  url: http://127.0.0.1:19090
  secret: null
proxy_host: 127.0.0.1
mixed_port: 7070
proxy_ports:
  http: null
  socks: null
  allow_lan: false
  services: []
system_proxy:
  enabled: false
  use_default_bypass: true
  bypass: ""
tun:
  enable: false
  stack: mixed
  device: utun1024
  auto_route: true
  auto_redirect: false
  auto_detect_interface: true
  dns_hijack:
    - any:53
  strict_route: false
  mtu: 1500
  route_exclude_address: []
dns:
  enable: false
  listen: 127.0.0.1:10553
  enhanced_mode: fake-ip
  fake_ip_range: 198.18.0.1/16
  fake_ip_filter_mode: blacklist
  ipv6: true
  prefer_h3: false
  respect_rules: false
  use_hosts: false
  use_system_hosts: false
  direct_nameserver_follow_policy: false
  lan_domains:
    - +.lan
    - +.local
    - +.arpa
  lan_nameserver: []
  nameserver_policy: {}
  default_nameserver:
    - system
    - 223.6.6.6
    - 8.8.8.8
  nameserver:
    - 8.8.8.8
    - https://doh.pub/dns-query
    - https://dns.alidns.com/dns-query
  fallback: []
  proxy_server_nameserver:
    - https://doh.pub/dns-query
    - https://dns.alidns.com/dns-query
    - tls://223.5.5.5
  direct_nameserver: []
  fake_ip_filter:
    - "*.lan"
    - "*.local"
    - "*.arpa"
    - time.*.com
    - ntp.*.com
    - +.market.xiaomi.com
    - localhost.ptlogin2.qq.com
    - "*.msftncsi.com"
    - www.msftconnecttest.com
autostart:
  enabled: false
port_allocation:
  seed: null
  auto_controller: true
  auto_mixed: false
  auto_dns: true
runtime_backend: service
runtime_mode: rule
proxy_selections: {}
subscriptions: []
active_profile: null
```

## Field Reference

### `mihomo`

```yaml
mihomo:
  core: auto
  update: manual
core_path: null
```

- `core`: `auto`, `verge-mihomo`, `verge-mihomo-alpha`, or `custom`.
- `auto`: use `core_path` if set, then `MIHOMO_CORE`, then managed stable core,
  then known installed cores. If no core exists, `start` downloads stable
  `verge-mihomo`.
- `verge-mihomo`: use/download managed stable MetaCubeX/mihomo release.
- `verge-mihomo-alpha`: use/download managed prerelease alpha.
- `custom`: use `core_path`; startup fails if the file is missing.
- `update`: currently informational/manual. Explicit updates are done from the
  TUI Runtime page `Update Core`.
- `core_path`: absolute path to a custom mihomo-compatible binary.

### `controller`

```yaml
controller:
  url: http://127.0.0.1:19090
  secret: null
```

The controller is owned by `clashtui` when the runtime is running. Do not point
it at another app's mihomo unless the intent is read-only inspection; runtime
write operations refuse to patch external controllers.

### Global Proxy

```yaml
proxy_host: 127.0.0.1
mixed_port: 7070
runtime_mode: rule
```

- `proxy_host`: default listen host for global generated listeners.
- `mixed_port`: global mixed HTTP/SOCKS5 port.
- `runtime_mode`: `rule`, `global`, or `direct`.

Use `0.0.0.0` only when LAN access is intended.

### Port Proxy Listeners

```yaml
proxy_ports:
  http: null
  socks: null
  allow_lan: false
  services:
    - enabled: true
      name: hk-mixed
      kind: mixed
      listen: 127.0.0.1
      port: 7071
      subscription: oist
      mode: global
      proxy: HK-01
      rule: null
      rule_selections: {}
      udp: true
```

- `http` and `socks`: optional extra global HTTP/SOCKS ports.
- `allow_lan`: maps to mihomo `allow-lan`.
- `services`: extra Port Proxy listeners.
- `services[].kind`: `mixed`, `http`, or `socks`.
- `services[].listen`: listen host, usually `127.0.0.1`.
- `services[].port`: listener port. `0` means let `clashtui` allocate a stable
  free port and save it back to config.
- `services[].subscription`: optional subscription name. `null` uses
  `active_profile`.
- `services[].mode`: `global`, `rule`, or `direct`.
- `services[].proxy`: exact proxy node/group name for `global` mode.
- `services[].rule_selections`: map of proxy-group name to proxy node for
  `rule` mode.
- `services[].udp`: enabled in config; single-runtime listeners rely on mihomo
  listener support.

In the default `runtime_backend: service`, all enabled Port Proxy services are
rendered as mihomo listeners in one generated runtime config. In `legacy`,
`multi`, or `multi-process`, Port Proxy services are separate mihomo processes.

### System Proxy

```yaml
system_proxy:
  enabled: false
  use_default_bypass: true
  bypass: ""
```

When enabled, OS system proxy points at `proxy_host:mixed_port`. If
`use_default_bypass` is true, `bypass` is appended to the platform default bypass
list.

### TUN

```yaml
tun:
  enable: false
  stack: mixed
  device: utun1024
  auto_route: true
  auto_redirect: false
  auto_detect_interface: true
  dns_hijack:
    - any:53
  strict_route: false
  mtu: 1500
  route_exclude_address: []
```

TUN requires privileged service mode for reliable operation. If the service is
missing, `clashtui start` falls back to user-mode single runtime with TUN
disabled.

Default `device` is platform-specific: `utun1024` on macOS and `Mihomo` on
Linux. macOS requires an `utun*` device name and disables `auto_redirect`; Linux
supports `auto_redirect`.

### DNS

```yaml
dns:
  enable: false
  listen: 127.0.0.1:10553
  enhanced_mode: fake-ip
  fake_ip_range: 198.18.0.1/16
  fake_ip_filter_mode: blacklist
  ipv6: true
  prefer_h3: false
  respect_rules: false
  use_hosts: false
  use_system_hosts: false
  direct_nameserver_follow_policy: false
  lan_domains: []
  lan_nameserver: []
  nameserver_policy: {}
  default_nameserver: []
  nameserver: []
  fallback: []
  proxy_server_nameserver: []
  direct_nameserver: []
  fake_ip_filter: []
```

DNS fields are rendered into mihomo DNS config. LAN DNS settings are merged into
`nameserver-policy`, `direct-nameserver`, and `fake-ip-filter`.

### Autostart

```yaml
autostart:
  enabled: false
```

When saved and applied, macOS uses a user LaunchAgent for login autostart. Linux
alignment is expected to be config-driven as well.

### Port Allocation

```yaml
port_allocation:
  seed: null
  auto_controller: true
  auto_mixed: false
  auto_dns: true
```

Port allocation is deterministic per config. User-fixed ports are not changed.
If a fixed port is occupied, startup fails. Port Proxy service `port: 0` allows
allocation.

Default ranges:

- global mixed: `7070`
- Port Proxy listeners: `7071-7970`
- controller: `19090-19989`
- legacy Port Proxy controllers: `20090-20989`
- DNS listen: `15053-15952`

### Runtime Backend

```yaml
runtime_backend: service
```

Allowed values:

- `service`: default. One root/service-owned mihomo when service is reachable;
  user-mode single runtime fallback when service is missing.
- `single`: one user-mode mihomo runtime.
- `legacy`, `multi`, `multi-process`: older multi-process model.

### Subscriptions

```yaml
subscriptions:
  - name: oist
    url: https://example.com/sub.yaml
    refresh: weekly
    updated_at: null
    last_error: null
    user_info:
      upload: null
      download: null
      total: null
      expire: null
    rule_selections: {}
active_profile: oist
proxy_selections: {}
```

- `subscriptions[].name`: stable local identifier.
- `subscriptions[].url`: subscription URL.
- `subscriptions[].refresh`: `disabled`, `daily`, or `weekly`.
- `updated_at`, `last_error`, `user_info`: runtime-maintained metadata. Preserve
  these unless intentionally resetting update state.
- `rule_selections`: per-subscription proxy-group selections.
- `active_profile`: name of the main subscription.
- `proxy_selections`: global selection map used by the active runtime.

## LLM Editing Rules

1. Preserve valid YAML and existing unrelated fields.
2. Do not edit generated runtime files.
3. Prefer exact existing subscription names and proxy names.
4. If adding a Port Proxy and the desired port is not specified, set `port: 0`.
5. If changing listener ports, TUN, DNS listen address, runtime backend, or
   mihomo core, use Save & Restart or `clashtui restart`.
6. If only changing subscription URL/name/refresh, Save is enough; runtime uses
   the new profile after update/reload/restart.
7. Keep `core_path` only for `mihomo.core: custom`; otherwise prefer `null`.
8. Avoid setting `proxy_host` or listener `listen` to `0.0.0.0` unless LAN access
   is explicitly requested.
9. Preserve `updated_at`, `last_error`, and `user_info` unless the task asks to
   reset subscription metadata.
10. Use `runtime_backend: service` unless explicitly testing legacy behavior.

## Common Edits

### Select Managed Stable Mihomo

```yaml
mihomo:
  core: verge-mihomo
  update: manual
core_path: null
```

### Select Custom Mihomo Binary

```yaml
mihomo:
  core: custom
  update: manual
core_path: /absolute/path/to/mihomo
```

### Add A Subscription

```yaml
subscriptions:
  - name: oist
    url: https://example.com/sub.yaml
    refresh: weekly
    updated_at: null
    last_error: null
    user_info: {}
    rule_selections: {}
active_profile: oist
```

### Enable Global System Proxy

```yaml
system_proxy:
  enabled: true
  use_default_bypass: true
  bypass: ""
```

### Enable TUN

```yaml
runtime_backend: service
tun:
  enable: true
  stack: mixed
  device: utun1024
  auto_route: true
  auto_redirect: false
  auto_detect_interface: true
  dns_hijack:
    - any:53
  strict_route: false
  mtu: 1500
  route_exclude_address: []
```

### Add A Port Proxy

```yaml
proxy_ports:
  services:
    - enabled: true
      name: japan
      kind: mixed
      listen: 127.0.0.1
      port: 0
      subscription: oist
      mode: global
      proxy: Japan 01
      rule: null
      rule_selections: {}
      udp: true
```
