# Port Proxy

Port Proxy services create additional local HTTP, SOCKS, or mixed listeners.
They are useful when different local ports need different subscriptions, modes,
or proxy targets.

Config path:

- `proxy_ports.services[]`

Important fields:

- `enabled`
- `name`
- `kind`: `mixed`, `http`, or `socks`
- `listen`
- `port`
- `subscription`
- `mode`: `rule`, `global`, or `direct`
- `proxy`
- `rule`
- `rule_selections`
- `udp`

In single/service backend, enabled services are rendered as mihomo `listeners`
inside one runtime.

Use `port: 0` for deterministic automatic allocation when the user did not ask
for a specific port.
