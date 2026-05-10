# DNS Design

`clashtui` uses mihomo built-in DNS. In TUN mode, system DNS can be routed to the mihomo TUN link, while mihomo listens on a local DNS port.

The default local DNS listen address is:

```text
127.0.0.1:10553
```

This avoids common conflicts with systemd-resolved on port `53` and Clash Verge on `1053`.

## LAN DNS

LAN domains often need a router, company DNS, or the OS resolver. The TUI exposes a simplified model:

```yaml
dns:
  lan_domains:
    - +.lan
    - +.local
    - +.corp.local
  lan_nameserver:
    - system
    - 192.168.0.1
```

At runtime, `clashtui` converts this to mihomo:

```yaml
dns:
  nameserver-policy:
    +.lan:
      - system
      - 192.168.0.1
    +.local:
      - system
      - 192.168.0.1
    +.corp.local:
      - system
      - 192.168.0.1
```

The same LAN domain list is also appended to `fake-ip-filter` so those names return real LAN IPs instead of fake IPs.

## Direct DNS

DIRECT traffic may need system or LAN DNS:

```yaml
dns:
  direct-nameserver:
    - system
    - 192.168.0.1
  direct-nameserver-follow-policy: true
```

When `direct-nameserver-follow-policy` is enabled, DIRECT DNS also respects `nameserver-policy`.

## Default And Fallback

- `nameserver`: normal DNS path.
- `fallback`: backup DNS path for polluted or foreign results.
- `proxy-server-nameserver`: DNS used for resolving proxy node hostnames.

LAN domains should normally use `nameserver-policy`, not fallback.
