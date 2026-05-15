# TUN

TUN is mihomo virtual network interface mode. It captures traffic at the IP
layer and usually needs privilege.

clashtui behavior:

- `runtime_backend: service` is the default.
- If the privileged service is reachable, service-owned mihomo can run TUN.
- If service mode falls back to user-mode runtime, TUN is disabled.
- TUN is global. It is not per Port Proxy.
- TUN often depends on DNS hijack so domain routing works.

Common diagnosis:

1. Check `runtime_backend`.
2. Check service installed/reachable status.
3. Check mihomo controller status.
4. Check generated runtime `tun` block.
5. Check logs for permission, device, route, or DNS hijack errors.

Platform notes:

- macOS uses `utun*` device names.
- Linux supports `auto_redirect`; macOS does not.
