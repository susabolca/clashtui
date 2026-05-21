# Overview

clashtui is a TUI and daemon controller for mihomo. clashtui does not proxy
traffic itself. It manages user config, subscriptions, local listeners, system
proxy state, DNS, TUN, and mihomo runtime lifecycle.

The user config is `config.yaml`. Generated mihomo runtime files such as
`mihomo-run.yaml` and `mihomo-active.yaml` are outputs. They are useful for
inspection and troubleshooting, but should not be edited directly.

The assistant should reason in this order:

1. User intent.
2. Current clashtui draft config.
3. Saved config and generated runtime config when needed.
4. mihomo controller/runtime state.
5. Logs and network probes.

Configuration changes must be proposed as structured patches to the TUI draft.
Saving and restarting remain user-controlled operations.
