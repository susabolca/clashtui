# System Proxy

System Proxy changes operating system proxy settings. It points applications to
clashtui's local mixed listener. It is not a mihomo inbound mode by itself.

System Proxy affects applications that honor OS proxy settings. TUN is different:
TUN captures traffic at the IP layer.

clashtui config:

- `system_proxy.enabled`
- `system_proxy.use_default_bypass`
- `system_proxy.bypass`
- `proxy_host`
- `mixed_port`

Common diagnosis:

1. Check the saved config and draft config.
2. Check actual OS system proxy status.
3. Check whether the mixed listener is running.
4. Probe the proxy listener with HTTP through proxy.
