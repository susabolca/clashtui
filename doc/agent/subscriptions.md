# Subscriptions

Subscriptions provide proxies, proxy groups, rules, rule providers, and proxy
providers. clashtui stores downloaded profiles in `profiles/`.

Config fields:

- `subscriptions[]`
- `active_profile`
- `proxy_selections`
- `subscriptions[].rule_selections`

Preserve subscription metadata unless the user asks to reset it:

- `updated_at`
- `last_error`
- `user_info`

When choosing proxies, prefer exact existing proxy names from the active runtime
or local subscription profile. If a requested proxy name is ambiguous, ask the
user or present candidates.
