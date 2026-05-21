# Runtime Generation

clashtui starts from the active subscription profile, then applies local runtime
overrides. Subscription YAML supplies proxies, proxy groups, rules, providers,
and outbound behavior.

clashtui owns local inbounds. Subscription-provided inbound/listener fields are
removed or ignored so subscriptions cannot create surprise local ports or
transparent services.

The default backend is `service`. When the privileged service is reachable,
service mode runs one service-owned mihomo runtime. If the service is missing or
unreachable, clashtui falls back to user-mode single runtime for that run and
disables TUN.

In the current single/service model:

- Global Proxy writes top-level mixed/http/socks listeners.
- DNS is a single global mihomo DNS service.
- TUN is a single global mihomo TUN service.
- Port Proxy services become mihomo `listeners` inside the same runtime.

Legacy `multi` and `multi-process` backends are compatibility modes.
