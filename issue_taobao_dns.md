# taobao.net DNS Policy

## Problem

`taobao.net` traffic is expected to go DIRECT, while DNS for the domain must be resolved through the dedicated server `30.30.30.30`.

The question was how this behavior is currently configured and where the effective configuration comes from.

## Findings

The `taobao.net` DNS override is not a source-code default. It is currently stored in the local persisted clashtui config:

```yaml
dns:
  nameserver_policy:
    +.taobao.net:
    - 30.30.30.30
```

Default config path:

```text
~/.config/clashtui/config.yaml
```

The generated mihomo runtime config converts that persisted `nameserver_policy` key into mihomo's native `nameserver-policy` key:

```yaml
dns:
  nameserver-policy:
    +.taobao.net:
    - 30.30.30.30
```

Observed generated files:

```text
~/.config/clashtui/mihomo-run.yaml
~/.config/clashtui/mihomo-active.yaml
```

The traffic routing side is separate from DNS resolution. The active profile also contains:

```yaml
- DOMAIN-SUFFIX,taobao.net,DIRECT
```

This means the current behavior is composed of two independent settings:

1. `dns.nameserver-policy` makes `*.taobao.net` resolve via `30.30.30.30`.
2. `DOMAIN-SUFFIX,taobao.net,DIRECT` makes matching traffic use DIRECT.

## Code Path

`src/dns.rs` builds the mihomo DNS patch and renders the effective policy:

```rust
"nameserver-policy": effective_nameserver_policy(config),
```

`effective_nameserver_policy()` starts from `config.dns.nameserver_policy` and then appends LAN-domain policy entries from `lan_domains` plus `lan_nameserver` when LAN DNS is configured.

The generated runtime profile includes this DNS patch through `src/runtime_profile.rs`.

## Current Interpretation

For `taobao.net`, the direct use of `30.30.30.30` is a `nameserver-policy` override, not a `direct-nameserver` setting.

`direct-nameserver` is currently empty in the observed config. `direct_nameserver_follow_policy` is also `false`, but that does not prevent the domain-specific `nameserver-policy` from applying when mihomo DNS resolves `+.taobao.net`.

## Follow-up Checks

- Confirm whether `30.30.30.30` should remain a user-local setting or become a managed/default rule in the application.
- If this should be configurable from the TUI, add an editor for arbitrary `nameserver_policy` entries; the current DNS page mainly exposes LAN-domain DNS, direct DNS, normal nameserver, fallback, and fake-IP filter fields.
- If DIRECT traffic should always use domain-specific DNS policy, verify the exact mihomo behavior with `direct-nameserver-follow-policy` in the intended runtime mode.
