# TUI Design

## Research Notes

AMI Aptio setup source is not public in a way that is appropriate to use here. Do not base this project on leaked AMI BIOS code. Use public UEFI HII and EDK II setup browser material as the reference model.

Useful public references:

- UEFI HII overview: https://uefi.org/specs/UEFI/2.10/33_Human_Interface_Infrastructure.html
- EDK II source: https://github.com/tianocore/edk2
- EDK II setup browser private model: https://raw.githubusercontent.com/tianocore/edk2/master/MdeModulePkg/Universal/SetupBrowserDxe/Setup.h

Important ideas from UEFI HII:

- HII separates configuration data from presentation. Drivers publish forms, strings, images, and storage bindings into the HII database. The forms browser reads that model and renders an implementation-specific UI.
- The browser operates on forms, questions, options, storage, validation, and submit/discard behavior.
- IFR question types map well to TUI components:
  - `EFI_IFR_REF`: submenu or cross-reference.
  - `EFI_IFR_ONE_OF`: single-choice dropdown/popover.
  - `EFI_IFR_CHECKBOX`: boolean toggle.
  - `EFI_IFR_NUMERIC`: number input with min/max/step.
  - `EFI_IFR_STRING`: text input.
  - `EFI_IFR_ACTION`: button/action row.
  - `EFI_IFR_NO_SUBMIT_IF` and `EFI_IFR_INCONSISTENT_IF`: validation before apply.
- The browser has a selected question and active form state. This matches our need for preserved cursor location and Esc stepping back through visited screens.
- EDK II keeps form view/history data. For ClashTUI, every submenu transition should push `{section_id, page_id, selected_row}` and Esc should restore the exact previous location.

## Design Goals

- Make the TUI feel like a dense terminal version of a web settings UI, not a screen full of nested boxes.
- Use boxes only for transient dialogs: confirm, choice popup, and single-field input popup.
- Use separators, whitespace, columns, and focused row highlight for the main settings surface.
- Prefer visible choices and simple Enter workflows over memorized hotkeys.
- Keep BIOS-style navigation where it helps: top sections, submenu arrows, Esc back, confirm before destructive exit.
- Prefer child screens for multi-field setup flows. Popup windows should edit one field or answer one short question.
- Keep the model data-driven so Chat/LLM and TUI can edit the same spec.

## Layout Rules

- Header: centered title, low-attention right-aligned version, top sections, one separator line.
- Body: left settings list, thin vertical separator, right help/details pane.
- Footer: status bar only. Key hints live in the right-side help pane.
- The status bar has no background fill. Use short text partitions for action status, config state, and service state.
- The status bar shares space with breadcrumbs: show the latest status output when present; otherwise show the current breadcrumb path.
- No nested cards. No framed panels inside framed panels.
- Top sections should be shown as section roots. The active section uses high-contrast white text on a dark blue background.
- Horizontal and vertical separators should use one consistent single-line Unicode set, for example `─`, `│`, `┬`, and `┴` when they meet.
- The right pane is split by layout only, not by a visible line: top area for selected item help, bottom-aligned area for key hints.
- The key hint block should use every row in its reserved area with no blank rows.
- The first line of each screen body is a short phrase summary, followed by a plain horizontal separator that does not connect to the side separator.
- Page-local separator lines are full-width within the pane and do not add leading or trailing spaces.
- Major content regions keep one column of left and right padding so text never touches a separator.
- Breadcrumb text is low-attention and lives in the status bar when there is no active status output.
- Setting rows use fixed columns:
  - submenu prefix after region padding: `>`, one space, then submenu label
  - setting prefix after region padding: two spaces, then setting label
  - label column: left-aligned
  - value column: left-aligned
  - optional dirty/status marker at the far right later
- The selected row uses inverse video or a restrained highlight.

## Component Model

Core data structures should look like this conceptually:

```text
SetupPage {
  id,
  title,
  rows: [SettingRow],
}

SettingRow {
  id,
  label,
  value,
  help,
  kind,
}

SettingKind =
  Submenu(page_id)
  Toggle(binding)
  Choice(binding, options)
  Number(binding, min, max, step)
  Range(binding, min, max, step)
  Text(binding, validation)
  Action(action_id)

Location {
  section_id,
  page_id,
  selected_row,
}
```

Navigation:

- A section is a top-level root. A page is the reusable UI unit.
- Left/Right and Tab switch sections only when the current page is the current section root and the location stack is empty.
- Enter on any submenu row pushes current `Location` and enters that page.
- Submenu page entry must not be treated as a section switch. For example, `Main > Global Proxy` keeps `Main` as the active section while showing the `Global Proxy` page.
- Multiple sections may reuse the same page.
- Esc closes the active popup first.
- If no popup is open, Esc pops one `Location`.
- On root section, Esc opens `Exit Without Saving?`.
- Location must preserve the active section, current page, and selected row within the current page path.

## Dialog Rules

Choice popup:

- Fixed width, centered near the current screen.
- Title must name the field, for example `Subscription`, `Mode`, or `DNS Strategy`.
- Title bar spans the full popup inner width and is centered with a background color.
- Shows options vertically.
- Options should not include padding spaces; highlight only the actual option text.
- Enter chooses, Esc cancels.
- Good for mode, subscription, DNS strategy, refresh interval.

Input popup:

- Fixed width and height.
- Title bar spans the full popup inner width and is centered with a background color.
- One active field for text edits, for example controller URL.
- The active input value is shown inside a small box so the user knows text entry is active.
- Number inputs use the same box but accept only digits and validate the configured range, for example ports must be `1..65535`.
- Enter validates and applies.
- Esc cancels.

Number popup:

- Fixed width and height.
- Shows the number inside the same input box shape.
- Up/Down changes the number.
- Enter validates and applies.
- Esc cancels.

Range popup:

- Fixed width and height.
- Shows a progress bar.
- Left/Right changes the value.
- Enter applies.
- Esc cancels.

Multi-field setup:

- Use a child page instead of a complex popup.
- Each row edits one field through a focused popup when needed.
- Choice rows such as refresh interval use choice popups.
- Text rows such as name and URL use input popups.
- `OK`/save is a visible action row.
- Breadcrumbs must show the setup path, for example `Subscription / Add Subscription`.
- Validation failure stays on the child page, focuses the failing row, and shows a concise alert popup.

Alert popup:

- Use for validation failures and required user attention.
- Keep content short: title, one-line message, centered `OK`.
- Esc, Enter, or Space closes the alert.

Confirm popup:

- Fixed width and height.
- Message body has a predictable line budget.
- Keep content minimal; most confirms only need a title and visible choices.
- Title bar spans the full popup inner width and is centered with a background color.
- One-button confirm uses centered `OK`.
- Yes/No confirm uses two buttons on one row, selected with Left/Right.
- F10/F11 and exit confirmations must always be Yes/No.

## ClashTUI Mapping

Main page:

- Runtime status summary.
- Proxy list as primary objects.
- Enter opens selected proxy settings.
- The proxy list always includes `Add Port Proxy` as a visible action row.
- Each proxy row uses three lines: listener identity, selected mode/subscription/config state, and refreshed traffic/IP information.
- The right pane shows selected proxy configuration, not runtime status.
- `O` is a quick on/off shortcut for the selected proxy entry.

Global Proxy page:

- Enabled
- Subscription
- Mode
- Proxy Server
- Mixed Port
- Sys Proxy
- PAC
- TUN
- DNS submenu

Port Proxy page:

- Enabled
- Subscription
- Mode
- Proxy Server
- HTTP/SOCKS/mixed listener
- Allow LAN
- UDP if relevant

Subscription page:

- Subscription list is a maintenance surface, not a proxy selection surface.
- Rows show subscription-owned state: used traffic, total traffic, expiry, last update, and last error.
- Do not show `used by` in the list; proxy usage belongs to proxy configuration.
- Enter on a subscription opens its detail page.
- Add Subscription opens a child screen with:
  - Name
  - URL
  - Refresh interval
  - OK
- The child screen uses input popups for name/URL and a choice popup for refresh.
- Per-subscription actions should be visible rows or choice popups, not hidden shortcuts.

Subscription detail page:

- Overview: URL, profile path, refresh cadence, traffic usage, expiry, update status.
- Rule Groups: subscription-level selections used when a proxy runs in `rule` mode.
- Proxies: nodes from the active/runtime subscription view with delay/availability results.
- Rules: read-only rule/rule-provider overview from the subscription profile.
- Update Now: refreshes the subscription and preserves last good local data on failure.
- Edit: changes name, URL, and refresh cadence.
- Delete: confirms deletion and handles existing proxy references explicitly.

Routing ownership:

- The subscription owns the raw profile, rules, proxy groups, rule providers, traffic metadata, and rule-mode group selections.
- A proxy owns which subscription it uses, its mode, listener settings, and global/direct routing choices.
- `rule` mode uses the subscription-level rule group selections.
- `global` mode uses a proxy-level selected node from the chosen subscription.
- `direct` mode bypasses proxy selection.
- Port proxies default to `global`; Global Proxy defaults to `rule`.
- This avoids forcing users to reconfigure rule groups for every proxy when a subscription changes.

Runtime page:

- Service install/status
- TUN permissions
- Controller
- Core path
- Logs
- DNS global settings

Chat page:

- Operates on the same `SetupPage`/binding model.
- LLM should propose a structured patch, validate, show diff, then apply.

## Implementation

The production config menu now uses this page-oriented layout in:

```text
src/config_menu.rs
```

The original isolated prototype remains available for quick UI experiments at:

```text
examples/settings_menu.rs
```

Run it with:

```sh
cargo run --example settings_menu
```

Current subscription implementation:

- `Subscription` stores `last_error`, `user_info`, and `rule_selections`.
- Subscription refresh parses the `subscription-userinfo` header and preserves the last good profile file on download failure.
- The Subscription screen is now a maintenance list; Enter opens a subscription detail page instead of activating the subscription.
- The detail page exposes Overview, Rule Groups, Proxies, Rules, Update Now, Edit, and Delete rows.
- Delete uses a confirmation dialog and clears/reassigns `active_profile` when deleting the active subscription.
- The subscription Proxies page reads nodes from the local subscription YAML so users can inspect nodes without starting mihomo.
- Proxy node delay testing is wired through mihomo `/proxies/{name}/delay` and requires the selected subscription to be loaded in the owned runtime.
- Subscription Proxies use `Check` wording and include `Check All`; delay results are displayed inline after testing.
- `Check All` runs in a background task and shows a modal progress popup so large subscriptions do not freeze the TUI event loop. `Esc` cancels the running batch; completed results remain visible in the proxy list.
- Proxy group selection is saved to the active subscription when runtime mode is `rule`; otherwise it uses the legacy global `proxy_selections`.
- Daemon apply overlays active subscription `rule_selections` on top of legacy global selections in `rule` mode.
- Long settings/proxy lists keep the selected row visible by scrolling the rendered window.

Current Main implementation:

- Main now renders Global Proxy, existing optional HTTP/SOCKS port listeners, configured port proxy services, and an `Add Port Proxy` action.
- `Add Port Proxy` creates an enabled mixed listener with the next fixed port starting at `7071` and immediately opens its proxy settings page.
- Main proxy rows use `Global`/`Port` wording and show local proxy ports directly rather than controller ports.
- Proxy rows use three concise lines: `ON/OFF LISTENER=host:port`, important mode/subscription/system settings, then `↑/↓` speed plus IP/country/city.
- Global Proxy keeps the default `127.0.0.1:7070`; using `0.0.0.0:7070` makes it LAN-accessible.
- Port Proxy settings now own independent `mode`, `subscription`, `proxy`, and `rule_selections` instead of sharing Global Proxy state.
- In `global` mode, Proxy Server selects a concrete proxy node; in `rule` mode, it opens group selection.
- The Main row detail no longer shows workdir, PAC, TUN, or DNS state; those remain explicit settings on the relevant child pages.
- Action rows such as `Add Port Proxy` are single-line, colored actions without a submenu `>` marker; their help text lives in the right detail pane.
- Traffic and IP information are refreshed by the existing TUI runtime refresh loop.
- `O` toggles Global Proxy system proxy state or the selected port proxy service. Legacy HTTP/SOCKS rows can be turned off; new listeners should use port proxy services.

Runtime startup policy:

- Opening Config/TUI should not start mihomo just to display subscription data.
- `start` starts the Global Proxy mihomo runtime plus one mihomo runtime for each enabled Port Proxy.
- Global Proxy owns system proxy, TUN, and DNS behavior.
- Each Port Proxy owns its listener, subscription, mode, selected proxy/rule choices, workdir, controller, pid file, and log file.
- Port Proxy mihomo stdout/stderr is redirected to its own runtime log and must not pollute the TUI.
- Config browsing should not imply runtime side effects.

Known follow-up:

- Edit subscription should reuse the Add Subscription form instead of the current placeholder.
- Rule Groups should get a first-class group/node editor rather than relying on the Proxy Groups page.
- Subscription delay checks should avoid temporarily loading the selected subscription into the Global Proxy runtime; a dedicated check runtime would match the multi-mihomo model better.
