# ClashTUI TUI Redesign Context

## Current State

- Repo: `/Users/zhaolei/src/github/clashtui`
- Branch: `fix_tun`
- Main request: move the BIOS-inspired TUI design from the prototype into the actual `clashtui config` flow.
- Production TUI implementation now lives in `src/config_menu.rs`.
- Design notes live in `issue_tui_design.md`.
- Experimental prototype remains in `examples/settings_menu.rs`.

## Product Direction

ClashTUI is mainly a single-binary controller/setup tool for mihomo. It still needs a workdir because clashtui and each mihomo runtime require config files, logs, pid files, profile snapshots, and mihomo `-d` state.

User mental model:

- Common commands should stay simple: `start`, `stop`, `status`, `config`.
- Most setup work should live in `config`/TUI, including service setup, TUN permission setup, subscriptions, proxy listener config, DNS, and runtime settings.
- Privileged work should be elevated only when needed and ideally only once for setup.
- If privileged TUN/system service setup is unavailable, port proxy mode should still work.

Proxy model:

- There is always a default `System Proxy`.
- Users can add/use multiple port proxies.
- `System Proxy` can configure OS proxy, PAC, TUN, mixed port, subscription, mode, and proxy selection.
- TUN belongs only to `System Proxy`.
- `start` should bring up all configured proxies, not only the system proxy.
- `System Proxy` maps to the Global Proxy mihomo runtime and defaults to mixed port `7070`.
- Each user-created Port Proxy maps to its own mihomo process, own workdir, own controller, own pid file, own log file, and one local/LAN listener.
- Only the Global Proxy mihomo owns TUN/DNS/system proxy. Port Proxy runtimes must not own TUN.

Subscription model:

- Users may have multiple subscriptions.
- Each proxy should be able to choose subscription, mode (`rule`, `global`, `direct`), group, and proxy server where relevant.
- Subscription UI should show list, last update, refresh cadence, URL/profile info, and actions.
- Add subscription should be a guided child page, not a single text box.
- Refresh cadence options currently modeled as `1 day`, `1 week`, `disabled`; weekly is default.
- Failed subscription updates should not destroy last-known-good data.

## TUI Design Rules

The UI is inspired by BIOS setup screens, but should feel closer to a dense terminal web settings UI:

- Top sections: `Main`, `Subscription`, `Runtime`, `Chat`, `Help`.
- Title: `ClashTUI Config`, centered.
- Version: right-aligned, low-attention color.
- Background: black.
- Use single-line Unicode separators: `─`, `│`, `┬`, `┴`.
- Do not use heavy nested boxes for normal page layout.
- Main body: left settings list, one vertical separator, right details/help pane.
- Right details pane: upper area selected-item details, bottom-aligned `Keys` block with no blank rows.
- Footer/status line: no background fill; shows status output or breadcrumb plus config/service state.
- Breadcrumb shows page stack when there is no active status.
- Root section switching with `Left`/`Right`/`Tab` is only allowed at section root.
- Submenus/pages push `Location { section, page, selected }`.
- `Esc` pops one location at a time.
- Root `Esc` opens a short `Exit Without Saving?` No/Yes confirm.
- Confirm popups are short, fixed-size, title bar plus No/Yes buttons.
- Alert popups are short, fixed-size, title bar plus one-line message and centered OK.
- Choice popups are short with meaningful titles and no padded/highlighted fake spaces.
- Text/number input popups show a visible input box; number fields accept digits and validate ports.

## Implementation Notes

Important production implementation pieces in `src/config_menu.rs`:

- `Page`: includes section roots and reusable child pages.
- `Location`: stores `{ section, page, selected }` for stack-based navigation.
- `SettingRow` + `RowKind`: data-driven settings rows used by renderer.
- `ConfigApp`: now tracks `section`, `page`, `history`, `dropdown`, `confirm`, `alert`, runtime state, and subscription draft state.
- `draw()`: renders header/body/footer and overlays input/dropdown/confirm/alert.
- `draw_settings()`: renders the left settings list.
- `draw_help()`: renders selected-item details and bottom-aligned key hints.
- `draw_footer()`: renders separator plus status/breadcrumb/config/service state.
- `begin_add_subscription()` opens `Page::AddSubscription` as a child page.
- `submit_subscription_form()` validates name/URL/duplicates and shows alert on failure.
- Mode and subscription selection use dropdowns instead of page jumps.

Runtime/data support added or used:

- `src/config.rs` has `SubscriptionRefresh { Disabled, Daily, Weekly }`, defaulting to `Weekly`.
- `src/config.rs` has `RuntimePaths` plus per-Port-Proxy runtime directories under `runtimes/port-proxy-N/`.
- `src/mihomo.rs` has `connections()` support for runtime traffic summaries.
- `src/core.rs` starts/stops the Global Proxy mihomo and each enabled Port Proxy mihomo separately; child stdout/stderr goes to each runtime `mihomo.log`.
- `src/daemon.rs` applies Global Proxy and Port Proxy runtimes independently, so stale Global Proxy selections or TUN warnings do not block Port Proxy listeners.
- macOS user-mode start cannot create `utun`; `status` now reports missing TUN privileges and uses `ifconfig`/`route`/`netstat` instead of Linux `ip` commands.
- IP info is fetched from `https://ipinfo.io/json` periodically for display.

Current production TUI has old helper functions still present behind `#![allow(dead_code)]` in `src/config_menu.rs`; they are residual from the previous UI and can be removed in a later cleanup.

## Verification Already Run

After integrating the production TUI:

- `cargo check`
- `cargo check --examples`
- `cargo test`
- `cargo clippy --all-targets`
- `git diff --check`
- `cargo build`
- Manually launched `target/debug/clashtui config` with a temporary `CLASHTUI_CONFIG_DIR`.
- Visually checked first screen and exit confirm popup.
- Confirmed no lingering `clashtui config` or `settings_menu` processes.

## Dirty Worktree Notes

Expected changed/untracked files include:

- `src/config.rs`
- `src/config_menu.rs`
- `src/mihomo.rs`
- `examples/settings_menu.rs`
- `issue_tui_design.md`
- `issue_ucd_redesign.md`
- `ctx_tui_redesign.md`

Do not revert unrelated dirty changes. Continue working with the existing changes on `fix_tun`.
