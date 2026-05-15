# Config Semantics

`clashtui config` edits an in-memory draft. `Save` writes `config.yaml`.
`Save & Restart` writes `config.yaml` and restarts the clashtui-owned runtime.

Missing fields receive Rust serde defaults. Unknown fields and YAML comments are
not preserved when clashtui saves the file.

Do not edit generated runtime files:

- `mihomo-run.yaml`
- `mihomo-active.yaml`
- `profiles/*`
- `cores/*`
- `*.log`
- `*.pid`
- `runtimes/*`

For config changes:

- Preserve unrelated fields.
- Preserve subscription metadata unless the user asks to reset it.
- Use exact existing subscription names and proxy names when possible.
- Use `port: 0` when adding a Port Proxy and the user did not request a port.
- Do not use `0.0.0.0` unless the user explicitly asks for LAN access.
- Listener ports, TUN, DNS listen, runtime backend, and core changes require
  Save & Restart for predictable runtime state.
