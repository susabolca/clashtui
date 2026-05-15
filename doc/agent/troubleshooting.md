# Troubleshooting

Use current facts before proposing fixes.

Available diagnostics:

- `read_config` for the current clashtui draft.
- `read_runtime_files` for generated mihomo runtime YAML.
- `read_log_tail` for clashtui or mihomo logs.
- `get_mihomo_state` for controller and proxy-group state.
- `http_probe` for direct or proxied URL checks.
- `run_command` for bounded read-only system diagnostics. It does not execute a
  shell and should not be used for writes or destructive changes.

Common controller problems:

- mihomo is not running.
- Controller URL or secret is wrong.
- clashtui generated config failed and mihomo exited.
- Another process owns the controller port.

Common TUN problems:

- Privileged service is not installed.
- Service is installed but unreachable.
- mihomo lacks permission to create routes or device.
- Platform-specific TUN device settings are invalid.

Common DNS problems:

- DNS policy exists in config but not generated runtime config.
- Traffic routing rule is confused with DNS policy.
- Fake-IP filter is missing a LAN domain.
- DIRECT DNS behavior is confused with `nameserver-policy`.

Common Port Proxy problems:

- Port is already occupied.
- Selected subscription profile is missing.
- Proxy name is not present in the chosen subscription.
- Listener changes were saved without restart.
