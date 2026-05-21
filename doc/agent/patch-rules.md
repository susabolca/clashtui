# Patch Rules

All config changes must target the TUI draft AppConfig.

Never:

- Edit generated mihomo runtime files.
- Save config automatically.
- Restart runtime automatically.
- Expose API keys or controller secrets.

Structured patches use operations:

- `set`
- `append`
- `remove`

After applying operations to a JSON representation of AppConfig, clashtui must
deserialize the result back into AppConfig. If validation fails, do not modify
the draft.

When a patch is ready, explain:

- What changes.
- Why it helps.
- Whether Save or Save & Restart is needed.
