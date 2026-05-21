# System Proxy

System Proxy changes operating system proxy settings. It is not a mihomo inbound
mode by itself.

Modes:

- `http`: point applications to clashtui's local mixed listener.
- `pac`: point applications to the daemon PAC URL; the PAC script then returns
  the local mixed listener or DIRECT.

System Proxy affects applications that honor OS proxy settings. TUN is different:
TUN captures traffic at the IP layer.

clashtui config:

- `system_proxy.enabled`
- `system_proxy.mode`
- `system_proxy.use_default_bypass`
- `system_proxy.bypass`
- `system_proxy.pac_port`
- `system_proxy.pac_strategy`
- `system_proxy.pac_rule_source_url`
- `system_proxy.pac_proxy_rules` (custom rules)
- `system_proxy.pac_direct_rules` (custom rules)
- `system_proxy.pac_content`
- `proxy_host`
- `mixed_port`

PAC details:

- The OS auto-proxy URL is `http://<system-proxy-host>:<pac_port>/commands/pac`.
- The daemon serves the PAC script locally; PAC is not written into mihomo
  runtime YAML.
- `pac_strategy: proxy-all` proxies all PAC-aware traffic to the mixed listener
  with DIRECT fallback.
- `pac_strategy: rules` generates a gfwlist-like script by parsing
  `gfwlist.txt` and merging those rules with custom `pac_proxy_rules` and
  `pac_direct_rules`. Direct rules win first; unmatched traffic is DIRECT.
- `pac_rule_source_url` is used by Runtime `Update PAC` to download gfwlist and
  rewrite `gfwlist.txt`. It must not overwrite custom rules in `config.yaml`.
- `pac_strategy: custom` serves `pac_content`. `pac_content` may use
  `%proxy-host%`, `%proxy_host%`, and `%mixed-port%`; clashtui renders those
  placeholders when serving the PAC file.
- For complex PAC edits, read the current config first. Patch
  `system_proxy.pac_strategy`, `system_proxy.pac_rule_source_url`, custom
  `system_proxy.pac_proxy_rules`, custom `system_proxy.pac_direct_rules`, or
  `system_proxy.pac_content`. Mark the patch `restart_required: true`.
- Prefer `pac_strategy: rules` for common domain-list requests such as
  gfwlist-style split proxying. Use `custom` only when the user asks for custom
  PAC JavaScript.
- TUN and System Proxy are separate. TUN may make System Proxy unnecessary for
  local traffic, but PAC still only affects applications that honor OS proxy
  settings.

Common diagnosis:

1. Check the saved config and draft config.
2. Use `get_system_proxy_state` to compare expected and actual OS proxy/PAC
   status.
3. Check whether the mixed listener is running.
4. For PAC mode, check whether `http://127.0.0.1:<pac_port>/commands/pac`
   responds.
5. Probe the proxy listener with HTTP through proxy.
