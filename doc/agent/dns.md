# DNS

clashtui uses mihomo built-in DNS.

User config fields include:

- `dns.enable`
- `dns.listen`
- `dns.lan_domains`
- `dns.lan_nameserver`
- `dns.nameserver_policy`
- `dns.nameserver`
- `dns.fallback`
- `dns.direct_nameserver`
- `dns.direct_nameserver_follow_policy`
- `dns.fake_ip_filter`

Runtime mapping:

- `nameserver_policy` becomes mihomo `dns.nameserver-policy`.
- `lan_domains` plus `lan_nameserver` are merged into `nameserver-policy`.
- `lan_domains` are also appended to `fake-ip-filter`.
- `direct_nameserver` becomes `direct-nameserver`.
- `direct_nameserver_follow_policy` becomes
  `direct-nameserver-follow-policy`.

Important reasoning:

- DNS resolution and traffic routing are separate.
- `DOMAIN-SUFFIX,example.com,DIRECT` controls traffic routing.
- `nameserver-policy` controls which DNS server resolves a domain.
- `direct-nameserver` is not the same as `nameserver-policy`.

Common fixes:

- Domain-specific DNS: add `dns.nameserver_policy["+.domain"] = ["server"]`.
- LAN DNS: add domains to `lan_domains` and servers to `lan_nameserver`.
- Fake-IP issues for LAN domains: add the domain to `lan_domains` or
  `fake_ip_filter`.
