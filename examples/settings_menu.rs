use std::io::{self, IsTerminal as _, Stdout};
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::{Frame, Terminal};

const TICK_RATE: Duration = Duration::from_millis(200);
const LABEL_WIDTH: usize = 26;
const APP_TITLE: &str = "ClashTUI Config";
const APP_VERSION: &str = concat!("v", env!("CARGO_PKG_VERSION"));
const H_LINE: char = '─';
const V_LINE: char = '│';
const TOP_JOINT: char = '┬';
const BOTTOM_JOINT: char = '┴';

fn main() -> Result<()> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        println!("Run this example in an interactive terminal.");
        return Ok(());
    }

    let mut terminal = TerminalGuard::enter()?;
    let mut app = App::default();

    while !app.should_quit {
        terminal.terminal.draw(|frame| draw(frame, &app))?;
        if event::poll(TICK_RATE)?
            && let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
        {
            handle_key(&mut app, key.code);
        }
    }

    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Page {
    Main,
    SystemProxy,
    PortProxy,
    Dns,
    Subscriptions,
    AddSubscription,
    Runtime,
    Chat,
    Help,
}

impl Page {
    const SECTION_ROOTS: [Self; 7] = [
        Self::Main,
        Self::SystemProxy,
        Self::PortProxy,
        Self::Subscriptions,
        Self::Runtime,
        Self::Chat,
        Self::Help,
    ];

    const fn title(self) -> &'static str {
        match self {
            Self::Main => "Main",
            Self::SystemProxy => "System Proxy",
            Self::PortProxy => "Port Proxy",
            Self::Dns => "DNS",
            Self::Subscriptions => "Subscription",
            Self::AddSubscription => "Add Subscription",
            Self::Runtime => "Runtime",
            Self::Chat => "Chat",
            Self::Help => "Help",
        }
    }

    fn is_section(self) -> bool {
        Self::SECTION_ROOTS.contains(&self)
    }
}

#[derive(Debug, Clone, Copy)]
struct Location {
    section: Page,
    page: Page,
    selected: usize,
}

#[derive(Debug, Clone, Copy)]
enum RowKind {
    Submenu(Page),
    Toggle(ToggleField),
    Choice(ChoiceField),
    Number(NumberField),
    Range(RangeField),
    Text(TextField),
    Action(ActionField),
    Info,
}

#[derive(Debug, Clone)]
struct SettingRow {
    label: &'static str,
    value: String,
    help: &'static str,
    kind: RowKind,
}

#[derive(Debug, Clone, Copy)]
enum ToggleField {
    SystemProxy,
    PortProxy,
    OsProxy,
    Pac,
    Tun,
    Dns,
}

#[derive(Debug, Clone, Copy)]
enum ChoiceField {
    Subscription,
    Mode,
    DnsStrategy,
    Refresh,
    AddSubscriptionRefresh,
    LogLevel,
}

#[derive(Debug, Clone, Copy)]
enum NumberField {
    MixedPort,
    HttpPort,
}

#[derive(Debug, Clone, Copy)]
enum RangeField {
    TrafficLimit,
}

#[derive(Debug, Clone, Copy)]
enum TextField {
    Controller,
    Nameserver,
    SubscriptionName,
    SubscriptionUrl,
}

#[derive(Debug, Clone, Copy)]
enum ActionField {
    SaveSubscription,
    ServiceInstall,
    TunInstall,
}

#[derive(Debug)]
enum Dialog {
    Choice {
        field: ChoiceField,
        selected: usize,
    },
    Input {
        field: InputField,
        value: String,
    },
    Range {
        field: RangeField,
        value: u8,
    },
    Confirm {
        action: ConfirmAction,
        yes: bool,
    },
    Alert {
        title: &'static str,
        message: &'static str,
    },
}

#[derive(Debug, Clone, Copy)]
enum InputField {
    Number(NumberField),
    Text(TextField),
}

#[derive(Debug, Clone, Copy)]
enum ConfirmAction {
    Exit,
    SaveRestart,
    Info(&'static str),
}

#[derive(Debug, Clone)]
struct Subscription {
    name: String,
    url: String,
    refresh: usize,
}

impl Subscription {
    fn new(name: impl Into<String>, url: impl Into<String>, refresh: usize) -> Self {
        Self {
            name: name.into(),
            url: url.into(),
            refresh,
        }
    }
}

#[derive(Debug)]
struct App {
    current: Location,
    stack: Vec<Location>,
    dialog: Option<Dialog>,
    status: String,
    should_quit: bool,
    system_proxy: bool,
    dirty: bool,
    port_proxy: bool,
    os_proxy: bool,
    pac: bool,
    tun: bool,
    dns: bool,
    mixed_port: u16,
    http_port: u16,
    traffic_limit: u8,
    controller: String,
    nameserver: String,
    subscriptions: Vec<Subscription>,
    active_subscription: usize,
    mode: usize,
    dns_strategy: usize,
    refresh: usize,
    draft_subscription_name: String,
    draft_subscription_url: String,
    draft_subscription_refresh: usize,
    log_level: usize,
}

impl Default for App {
    fn default() -> Self {
        Self {
            current: Location {
                section: Page::Main,
                page: Page::Main,
                selected: 0,
            },
            stack: Vec::new(),
            dialog: None,
            status: String::new(),
            should_quit: false,
            system_proxy: true,
            dirty: false,
            port_proxy: true,
            os_proxy: true,
            pac: false,
            tun: true,
            dns: true,
            mixed_port: 7070,
            http_port: 8080,
            traffic_limit: 60,
            controller: "http://127.0.0.1:9097".into(),
            nameserver: "https://dns.alidns.com/dns-query".into(),
            subscriptions: vec![
                Subscription::new("oist", "https://example.com/oist.yaml", 1),
                Subscription::new("backup", "https://example.com/backup.yaml", 1),
            ],
            active_subscription: 0,
            mode: 0,
            dns_strategy: 1,
            refresh: 1,
            draft_subscription_name: String::new(),
            draft_subscription_url: String::new(),
            draft_subscription_refresh: 1,
            log_level: 1,
        }
    }
}

impl App {
    fn rows(&self) -> Vec<SettingRow> {
        match self.current.page {
            Page::Main => vec![
                SettingRow {
                    label: "System Proxy",
                    value: format!(
                        "{}  127.0.0.1:{}",
                        on_off(self.system_proxy),
                        self.mixed_port
                    ),
                    help: "Default system-wide proxy object. Enter opens its settings.",
                    kind: RowKind::Submenu(Page::SystemProxy),
                },
                SettingRow {
                    label: "Port Proxy",
                    value: format!("HTTP 127.0.0.1:{}", self.http_port),
                    help: "A user-created listener proxy with the same basic settings model.",
                    kind: RowKind::Submenu(Page::PortProxy),
                },
                SettingRow {
                    label: "Subscriptions",
                    value: format!("{} configured", self.subscriptions.len()),
                    help: "Manage profiles and refresh intervals.",
                    kind: RowKind::Submenu(Page::Subscriptions),
                },
                SettingRow {
                    label: "Runtime",
                    value: format!("controller {}", self.controller),
                    help: "Service, permissions, controller, core path, and logs.",
                    kind: RowKind::Submenu(Page::Runtime),
                },
            ],
            Page::SystemProxy => vec![
                toggle_row(
                    "Enabled",
                    self.system_proxy,
                    "Enables the default system proxy object.",
                    ToggleField::SystemProxy,
                ),
                SettingRow {
                    label: "Subscription",
                    value: self.active_subscription_name(),
                    help: "Choose the subscription used by this proxy.",
                    kind: RowKind::Choice(ChoiceField::Subscription),
                },
                SettingRow {
                    label: "Mode",
                    value: self.choice_value(ChoiceField::Mode),
                    help: "Rule, global, or direct mode.",
                    kind: RowKind::Choice(ChoiceField::Mode),
                },
                SettingRow {
                    label: "Mixed Port",
                    value: self.mixed_port.to_string(),
                    help: "Local mixed HTTP/SOCKS listener port. Up and Down change the number in a popup.",
                    kind: RowKind::Number(NumberField::MixedPort),
                },
                SettingRow {
                    label: "Traffic Limit",
                    value: format!("{}%", self.traffic_limit),
                    help: "Prototype range setting. Left and Right adjust a progress bar in a popup.",
                    kind: RowKind::Range(RangeField::TrafficLimit),
                },
                toggle_row(
                    "OS System Proxy",
                    self.os_proxy,
                    "Set operating-system proxy settings to the mixed port.",
                    ToggleField::OsProxy,
                ),
                toggle_row(
                    "PAC",
                    self.pac,
                    "PAC belongs to System Proxy and is disabled in this prototype.",
                    ToggleField::Pac,
                ),
                toggle_row(
                    "TUN",
                    self.tun,
                    "Transparent system traffic. Requires platform privileges.",
                    ToggleField::Tun,
                ),
                SettingRow {
                    label: "DNS",
                    value: on_off(self.dns).into(),
                    help: "Open DNS settings.",
                    kind: RowKind::Submenu(Page::Dns),
                },
            ],
            Page::PortProxy => vec![
                toggle_row(
                    "Enabled",
                    self.port_proxy,
                    "A port proxy exposes a local listener without changing OS proxy settings.",
                    ToggleField::PortProxy,
                ),
                SettingRow {
                    label: "Subscription",
                    value: self.active_subscription_name(),
                    help: "Choose the subscription used by this port proxy.",
                    kind: RowKind::Choice(ChoiceField::Subscription),
                },
                SettingRow {
                    label: "Mode",
                    value: self.choice_value(ChoiceField::Mode),
                    help: "Rule, global, or direct mode.",
                    kind: RowKind::Choice(ChoiceField::Mode),
                },
                SettingRow {
                    label: "HTTP Port",
                    value: self.http_port.to_string(),
                    help: "Local HTTP listener port. Up and Down change the number in a popup.",
                    kind: RowKind::Number(NumberField::HttpPort),
                },
                SettingRow {
                    label: "Traffic Limit",
                    value: format!("{}%", self.traffic_limit),
                    help: "Range popup prototype shared with System Proxy.",
                    kind: RowKind::Range(RangeField::TrafficLimit),
                },
            ],
            Page::Dns => vec![
                toggle_row("Enabled", self.dns, "Enable mihomo DNS.", ToggleField::Dns),
                SettingRow {
                    label: "Strategy",
                    value: self.choice_value(ChoiceField::DnsStrategy),
                    help: "DNS resolve strategy.",
                    kind: RowKind::Choice(ChoiceField::DnsStrategy),
                },
                SettingRow {
                    label: "Nameserver",
                    value: self.nameserver.clone(),
                    help: "Primary DNS nameserver list in production.",
                    kind: RowKind::Text(TextField::Nameserver),
                },
            ],
            Page::Subscriptions => vec![
                SettingRow {
                    label: "Active Subscription",
                    value: self.active_subscription_summary(),
                    help: "Select the active profile source.",
                    kind: RowKind::Choice(ChoiceField::Subscription),
                },
                SettingRow {
                    label: "URL",
                    value: self.active_subscription_url(),
                    help: "Current subscription profile URL.",
                    kind: RowKind::Info,
                },
                SettingRow {
                    label: "Default Refresh",
                    value: self.choice_value(ChoiceField::Refresh),
                    help: "Default refresh cadence for newly added subscriptions.",
                    kind: RowKind::Choice(ChoiceField::Refresh),
                },
                SettingRow {
                    label: "Add Subscription",
                    value: self.add_subscription_state(),
                    help: "Open a child settings page for name, URL, refresh, and OK.",
                    kind: RowKind::Submenu(Page::AddSubscription),
                },
            ],
            Page::AddSubscription => vec![
                SettingRow {
                    label: "Name",
                    value: required_value(&self.draft_subscription_name),
                    help: "Subscription display name.",
                    kind: RowKind::Text(TextField::SubscriptionName),
                },
                SettingRow {
                    label: "URL",
                    value: required_value(&self.draft_subscription_url),
                    help: "Subscription profile URL.",
                    kind: RowKind::Text(TextField::SubscriptionUrl),
                },
                SettingRow {
                    label: "Refresh",
                    value: self.choice_value(ChoiceField::AddSubscriptionRefresh),
                    help: "Refresh cadence for this subscription.",
                    kind: RowKind::Choice(ChoiceField::AddSubscriptionRefresh),
                },
                SettingRow {
                    label: "OK",
                    value: "save".into(),
                    help: "Save this subscription and return to the subscription list.",
                    kind: RowKind::Action(ActionField::SaveSubscription),
                },
            ],
            Page::Runtime => vec![
                SettingRow {
                    label: "Controller",
                    value: self.controller.clone(),
                    help: "Mihomo external controller URL.",
                    kind: RowKind::Text(TextField::Controller),
                },
                SettingRow {
                    label: "Log Level",
                    value: self.choice_value(ChoiceField::LogLevel),
                    help: "Runtime logging detail.",
                    kind: RowKind::Choice(ChoiceField::LogLevel),
                },
                SettingRow {
                    label: "Service",
                    value: "not installed".into(),
                    help: "Prototype action row for service install/status.",
                    kind: RowKind::Action(ActionField::ServiceInstall),
                },
                SettingRow {
                    label: "TUN Permissions",
                    value: "check required".into(),
                    help: "Prototype action row for platform TUN permission setup.",
                    kind: RowKind::Action(ActionField::TunInstall),
                },
            ],
            Page::Chat => vec![
                SettingRow {
                    label: "Prompt Goal",
                    value: "configure proxy".into(),
                    help: "Future AI-assisted configuration entry point.",
                    kind: RowKind::Text(TextField::Controller),
                },
                SettingRow {
                    label: "Apply Mode",
                    value: "diff then apply".into(),
                    help: "Chat should propose a structured config patch before applying.",
                    kind: RowKind::Choice(ChoiceField::LogLevel),
                },
            ],
            Page::Help => vec![
                SettingRow {
                    label: "Navigation",
                    value: "Esc stack".into(),
                    help: "Esc returns through saved locations; root Esc asks before exit.",
                    kind: RowKind::Action(ActionField::ServiceInstall),
                },
                SettingRow {
                    label: "Inputs",
                    value: "choice/input/number/range".into(),
                    help: "Every editable value should use a visible popup, not hidden key magic.",
                    kind: RowKind::Action(ActionField::TunInstall),
                },
            ],
        }
    }

    fn active_subscription_name(&self) -> String {
        self.subscriptions
            .get(self.active_subscription)
            .map(|subscription| subscription.name.clone())
            .unwrap_or_else(|| "-".into())
    }

    fn active_subscription_summary(&self) -> String {
        self.subscriptions
            .get(self.active_subscription)
            .map(|subscription| {
                format!(
                    "{}  {}",
                    subscription.name,
                    self.refresh_label(subscription.refresh)
                )
            })
            .unwrap_or_else(|| "-".into())
    }

    fn active_subscription_url(&self) -> String {
        self.subscriptions
            .get(self.active_subscription)
            .map(|subscription| subscription.url.clone())
            .unwrap_or_else(|| "-".into())
    }

    fn refresh_label(&self, selected: usize) -> String {
        self.choice_options(ChoiceField::Refresh)
            .get(selected.min(2))
            .cloned()
            .unwrap_or_else(|| "-".into())
    }

    fn add_subscription_state(&self) -> String {
        if self.draft_subscription_name.trim().is_empty()
            && self.draft_subscription_url.trim().is_empty()
        {
            "submenu".into()
        } else {
            "draft".into()
        }
    }

    fn choice_value(&self, field: ChoiceField) -> String {
        let selected = self.choice_index(field);
        self.choice_options(field)
            .get(selected)
            .cloned()
            .unwrap_or_else(|| "-".into())
    }

    fn choice_index(&self, field: ChoiceField) -> usize {
        match field {
            ChoiceField::Subscription => self.active_subscription,
            ChoiceField::Mode => self.mode,
            ChoiceField::DnsStrategy => self.dns_strategy,
            ChoiceField::Refresh => self.refresh,
            ChoiceField::AddSubscriptionRefresh => self.draft_subscription_refresh,
            ChoiceField::LogLevel => self.log_level,
        }
    }

    fn choice_options(&self, field: ChoiceField) -> Vec<String> {
        match field {
            ChoiceField::Subscription => {
                if self.subscriptions.is_empty() {
                    vec!["No subscriptions configured".into()]
                } else {
                    self.subscriptions
                        .iter()
                        .map(|subscription| subscription.name.clone())
                        .collect()
                }
            }
            ChoiceField::Mode => vec!["rule".into(), "global".into(), "direct".into()],
            ChoiceField::DnsStrategy => {
                vec!["prefer-ipv4".into(), "ipv4-only".into(), "ipv6-only".into()]
            }
            ChoiceField::Refresh | ChoiceField::AddSubscriptionRefresh => {
                vec!["1 day".into(), "1 week".into(), "disabled".into()]
            }
            ChoiceField::LogLevel => vec![
                "debug".into(),
                "info".into(),
                "warning".into(),
                "error".into(),
            ],
        }
    }

    fn set_choice(&mut self, field: ChoiceField, selected: usize) {
        match field {
            ChoiceField::Subscription => {
                if !self.subscriptions.is_empty() {
                    self.active_subscription = selected.min(self.subscriptions.len() - 1);
                }
            }
            ChoiceField::Mode => self.mode = selected.min(2),
            ChoiceField::DnsStrategy => self.dns_strategy = selected.min(2),
            ChoiceField::Refresh => self.refresh = selected.min(2),
            ChoiceField::AddSubscriptionRefresh => {
                self.draft_subscription_refresh = selected.min(2)
            }
            ChoiceField::LogLevel => self.log_level = selected.min(3),
        }
        self.dirty = true;
    }

    fn clamp_selection(&mut self) {
        let len = self.rows().len();
        if len == 0 {
            self.current.selected = 0;
        } else if self.current.selected >= len {
            self.current.selected = len - 1;
        }
    }

    fn open_page(&mut self, page: Page) {
        self.enter_page(page);
    }

    fn enter_page(&mut self, page: Page) {
        if self.current.page != page {
            if page == Page::AddSubscription
                && self.draft_subscription_name.trim().is_empty()
                && self.draft_subscription_url.trim().is_empty()
            {
                self.draft_subscription_refresh = self.refresh;
            }
            self.stack.push(self.current);
            self.current = Location {
                section: self.current.section,
                page,
                selected: 0,
            };
            self.status.clear();
        }
    }

    fn can_switch_sections(&self) -> bool {
        self.stack.is_empty()
            && self.current.page == self.current.section
            && self.current.section.is_section()
    }

    fn switch_section(&mut self, section: Page) {
        if !section.is_section() {
            return;
        }
        if self.current.section != section || self.current.page != section {
            self.stack.clear();
            self.current = Location {
                section,
                page: section,
                selected: 0,
            };
            self.status.clear();
        }
    }

    fn next_section(&mut self) {
        if !self.can_switch_sections() {
            self.status = "Back to section root before switching sections".into();
            return;
        }
        let index = Page::SECTION_ROOTS
            .iter()
            .position(|page| *page == self.current.section)
            .unwrap_or_default();
        let next = Page::SECTION_ROOTS[(index + 1) % Page::SECTION_ROOTS.len()];
        self.switch_section(next);
    }

    fn prev_section(&mut self) {
        if !self.can_switch_sections() {
            self.status = "Back to section root before switching sections".into();
            return;
        }
        let index = Page::SECTION_ROOTS
            .iter()
            .position(|page| *page == self.current.section)
            .unwrap_or_default();
        let prev = if index == 0 {
            Page::SECTION_ROOTS[Page::SECTION_ROOTS.len() - 1]
        } else {
            Page::SECTION_ROOTS[index - 1]
        };
        self.switch_section(prev);
    }
}

fn toggle_row(
    label: &'static str,
    value: bool,
    help: &'static str,
    field: ToggleField,
) -> SettingRow {
    SettingRow {
        label,
        value: on_off(value).into(),
        help,
        kind: RowKind::Toggle(field),
    }
}

fn required_value(value: &str) -> String {
    if value.trim().is_empty() {
        "(required)".into()
    } else {
        value.to_string()
    }
}

fn page_summary(page: Page) -> &'static str {
    match page {
        Page::Main => "runtime overview / proxy entrypoints",
        Page::SystemProxy => "system traffic / OS proxy / TUN",
        Page::PortProxy => "local listener / port proxy",
        Page::Dns => "resolve strategy / nameservers",
        Page::Subscriptions => "profiles / refresh / usage",
        Page::AddSubscription => "new profile / URL / refresh",
        Page::Runtime => "service / controller / logs",
        Page::Chat => "assistant / structured patch",
        Page::Help => "navigation / input model",
    }
}

fn handle_key(app: &mut App, key: KeyCode) {
    if let Some(dialog) = app.dialog.take() {
        app.dialog = handle_dialog_key(app, dialog, key);
        app.clamp_selection();
        return;
    }

    match key {
        KeyCode::Up => {
            if app.current.selected == 0 {
                app.current.selected = app.rows().len().saturating_sub(1);
            } else {
                app.current.selected -= 1;
            }
        }
        KeyCode::Down => {
            let len = app.rows().len().max(1);
            app.current.selected = (app.current.selected + 1) % len;
        }
        KeyCode::Tab | KeyCode::Right => app.next_section(),
        KeyCode::BackTab | KeyCode::Left => app.prev_section(),
        KeyCode::Enter | KeyCode::Char(' ') => activate_row(app),
        KeyCode::Esc => {
            if let Some(location) = app.stack.pop() {
                app.current = location;
                app.status.clear();
            } else {
                app.dialog = Some(Dialog::Confirm {
                    action: ConfirmAction::Exit,
                    yes: false,
                });
            }
        }
        KeyCode::F(10) => {
            app.dialog = Some(Dialog::Confirm {
                action: ConfirmAction::SaveRestart,
                yes: false,
            });
        }
        _ => {}
    }
    app.clamp_selection();
}

fn activate_row(app: &mut App) {
    let rows = app.rows();
    let Some(row) = rows.get(app.current.selected) else {
        return;
    };

    match row.kind {
        RowKind::Submenu(page) => {
            app.open_page(page);
        }
        RowKind::Toggle(field) => {
            toggle_field(app, field);
            app.status = format!("{} toggled", row.label);
        }
        RowKind::Choice(field) => {
            app.dialog = Some(Dialog::Choice {
                field,
                selected: app.choice_index(field),
            });
            app.status = format!("Choose {}", row.label);
        }
        RowKind::Number(field) => {
            app.dialog = Some(Dialog::Input {
                field: InputField::Number(field),
                value: match field {
                    NumberField::MixedPort => app.mixed_port.to_string(),
                    NumberField::HttpPort => app.http_port.to_string(),
                },
            });
        }
        RowKind::Range(field) => {
            app.dialog = Some(Dialog::Range {
                field,
                value: range_value(app, field),
            });
        }
        RowKind::Text(field) => {
            app.dialog = Some(Dialog::Input {
                field: InputField::Text(field),
                value: match field {
                    TextField::Controller => app.controller.clone(),
                    TextField::Nameserver => app.nameserver.clone(),
                    TextField::SubscriptionName => app.draft_subscription_name.clone(),
                    TextField::SubscriptionUrl => app.draft_subscription_url.clone(),
                },
            });
        }
        RowKind::Action(ActionField::SaveSubscription) => {
            save_subscription(app);
        }
        RowKind::Action(ActionField::ServiceInstall) => {
            app.dialog = Some(Dialog::Confirm {
                action: ConfirmAction::Info("Service install is a placeholder."),
                yes: true,
            });
        }
        RowKind::Action(ActionField::TunInstall) => {
            app.dialog = Some(Dialog::Confirm {
                action: ConfirmAction::Info("TUN permission setup is a placeholder."),
                yes: true,
            });
        }
        RowKind::Info => {
            app.status = format!("{} is read-only", row.label);
        }
    }
}

fn toggle_field(app: &mut App, field: ToggleField) {
    match field {
        ToggleField::SystemProxy => app.system_proxy = !app.system_proxy,
        ToggleField::PortProxy => app.port_proxy = !app.port_proxy,
        ToggleField::OsProxy => app.os_proxy = !app.os_proxy,
        ToggleField::Pac => app.pac = !app.pac,
        ToggleField::Tun => app.tun = !app.tun,
        ToggleField::Dns => app.dns = !app.dns,
    }
    app.dirty = true;
}

fn range_value(app: &App, field: RangeField) -> u8 {
    match field {
        RangeField::TrafficLimit => app.traffic_limit,
    }
}

fn set_range(app: &mut App, field: RangeField, value: u8) {
    match field {
        RangeField::TrafficLimit => app.traffic_limit = value.min(100),
    }
    app.dirty = true;
}

fn handle_dialog_key(app: &mut App, mut dialog: Dialog, key: KeyCode) -> Option<Dialog> {
    match &mut dialog {
        Dialog::Choice { field, selected } => {
            let options_len = app.choice_options(*field).len().max(1);
            match key {
                KeyCode::Esc => {
                    app.status = "Canceled".into();
                    None
                }
                KeyCode::Up => {
                    *selected = if *selected == 0 {
                        options_len - 1
                    } else {
                        *selected - 1
                    };
                    Some(dialog)
                }
                KeyCode::Down => {
                    *selected = (*selected + 1) % options_len;
                    Some(dialog)
                }
                KeyCode::Enter | KeyCode::Char(' ') => {
                    app.set_choice(*field, *selected);
                    app.status = "Choice saved".into();
                    None
                }
                _ => Some(dialog),
            }
        }
        Dialog::Input { field, value } => match key {
            KeyCode::Esc => {
                app.status = "Canceled".into();
                None
            }
            KeyCode::Up if matches!(field, InputField::Number(_)) => {
                adjust_number_input(value, 1);
                Some(dialog)
            }
            KeyCode::Down if matches!(field, InputField::Number(_)) => {
                adjust_number_input(value, -1);
                Some(dialog)
            }
            KeyCode::Backspace => {
                value.pop();
                Some(dialog)
            }
            KeyCode::Enter => {
                apply_input(app, *field, value);
                app.dialog.take()
            }
            KeyCode::Char(ch) => {
                if matches!(field, InputField::Text(_)) {
                    value.push(ch);
                } else if ch.is_ascii_digit() {
                    push_number_digit(value, ch);
                }
                Some(dialog)
            }
            _ => Some(dialog),
        },
        Dialog::Range { field, value } => match key {
            KeyCode::Esc => {
                app.status = "Canceled".into();
                None
            }
            KeyCode::Left => {
                *value = value.saturating_sub(5);
                Some(dialog)
            }
            KeyCode::Right => {
                *value = (*value + 5).min(100);
                Some(dialog)
            }
            KeyCode::Enter | KeyCode::Char(' ') => {
                set_range(app, *field, *value);
                app.status = "Range saved".into();
                None
            }
            _ => Some(dialog),
        },
        Dialog::Confirm { action, yes } => match key {
            KeyCode::Esc => {
                app.status = "Canceled".into();
                None
            }
            KeyCode::Left | KeyCode::Up => {
                *yes = false;
                Some(dialog)
            }
            KeyCode::Right | KeyCode::Down => {
                *yes = true;
                Some(dialog)
            }
            KeyCode::Enter => {
                match action {
                    ConfirmAction::Exit if *yes => app.should_quit = true,
                    ConfirmAction::Exit => app.status = "Canceled".into(),
                    ConfirmAction::SaveRestart if *yes => {
                        app.status = "Saved and restart requested".into();
                        app.dirty = false;
                    }
                    ConfirmAction::SaveRestart => app.status = "Canceled".into(),
                    ConfirmAction::Info(_) => app.status = "Acknowledged".into(),
                }
                None
            }
            _ => Some(dialog),
        },
        Dialog::Alert { .. } => match key {
            KeyCode::Esc | KeyCode::Enter | KeyCode::Char(' ') => {
                app.status.clear();
                None
            }
            _ => Some(dialog),
        },
    }
}

fn adjust_number_input(value: &mut String, delta: i32) {
    let current = value.parse::<i32>().unwrap_or_default();
    let next = (current + delta).clamp(1, u16::MAX as i32);
    *value = next.to_string();
}

fn push_number_digit(value: &mut String, digit: char) {
    if value == "0" {
        value.clear();
    }
    let mut next = value.clone();
    next.push(digit);
    if matches!(next.parse::<u32>(), Ok(number) if number <= u16::MAX as u32) {
        *value = next;
    }
}

fn apply_input(app: &mut App, field: InputField, value: &str) {
    match field {
        InputField::Number(NumberField::MixedPort) => match value.parse::<u16>() {
            Ok(port) if port > 0 => {
                app.mixed_port = port;
                app.status = "Port saved".into();
                app.dirty = true;
            }
            _ => alert(app, "Invalid Port", "Port must be 1..65535."),
        },
        InputField::Number(NumberField::HttpPort) => match value.parse::<u16>() {
            Ok(port) if port > 0 => {
                app.http_port = port;
                app.status = "Port saved".into();
                app.dirty = true;
            }
            _ => alert(app, "Invalid Port", "Port must be 1..65535."),
        },
        InputField::Text(TextField::Controller) => {
            app.controller = value.trim().to_string();
            app.status = "Controller saved".into();
            app.dirty = true;
        }
        InputField::Text(TextField::Nameserver) => {
            app.nameserver = value.trim().to_string();
            app.status = "Nameserver saved".into();
            app.dirty = true;
        }
        InputField::Text(TextField::SubscriptionName) => {
            app.draft_subscription_name = value.trim().to_string();
            app.status = "Name saved".into();
            app.dirty = true;
        }
        InputField::Text(TextField::SubscriptionUrl) => {
            app.draft_subscription_url = value.trim().to_string();
            app.status = "URL saved".into();
            app.dirty = true;
        }
    }
}

fn save_subscription(app: &mut App) {
    let name = app.draft_subscription_name.trim();
    if name.is_empty() {
        app.current.selected = 0;
        alert(app, "Required Field", "Name is required.");
        return;
    }

    let url = app.draft_subscription_url.trim();
    if url.is_empty() {
        app.current.selected = 1;
        alert(app, "Required Field", "URL is required.");
        return;
    }

    let subscription = Subscription::new(name, url, app.draft_subscription_refresh);
    app.subscriptions.push(subscription);
    app.active_subscription = app.subscriptions.len() - 1;
    app.draft_subscription_name.clear();
    app.draft_subscription_url.clear();
    app.draft_subscription_refresh = app.refresh;
    app.dirty = true;

    if let Some(location) = app.stack.pop() {
        app.current = location;
    }
    app.status = "Subscription added".into();
}

fn alert(app: &mut App, title: &'static str, message: &'static str) {
    app.status.clear();
    app.dialog = Some(Dialog::Alert { title, message });
}

fn draw(frame: &mut Frame, app: &App) {
    let area = frame.area();
    let separator_column = split_column(area.width);
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(10),
            Constraint::Length(2),
        ])
        .split(area);

    draw_header(frame, app, layout[0], separator_column);
    draw_body(frame, app, layout[1]);
    draw_footer(frame, app, layout[2], separator_column);

    if let Some(dialog) = &app.dialog {
        draw_dialog(frame, app, dialog);
    }
}

fn draw_header(frame: &mut Frame, app: &App, area: Rect, separator_column: u16) {
    let active_section = app.current.section;
    let mut tabs = Vec::new();
    for tab in Page::SECTION_ROOTS {
        let active = tab == active_section;
        tabs.push(Span::raw(" "));
        tabs.push(Span::styled(
            if active {
                format!("[ {} ]", tab.title())
            } else {
                format!(" {} ", tab.title())
            },
            tab_style(active),
        ));
    }
    let lines = vec![
        title_bar_line(area.width),
        Line::from(tabs),
        Line::from(connected_horizontal_line(
            area.width,
            separator_column,
            TOP_JOINT,
        )),
    ];
    frame.render_widget(Paragraph::new(lines), area);
}

fn draw_body(frame: &mut Frame, app: &App, area: Rect) {
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(66),
            Constraint::Length(1),
            Constraint::Percentage(34),
        ])
        .split(area);
    draw_settings(frame, app, columns[0]);
    draw_vertical_separator(frame, columns[1]);
    draw_help(frame, app, columns[2]);
}

fn draw_settings(frame: &mut Frame, app: &App, area: Rect) {
    let rows = app.rows();
    let content_width = area.width.saturating_sub(2) as usize;
    let mut lines = vec![
        with_padding(Line::from(Span::styled(
            page_summary(app.current.page),
            Style::default().fg(Color::Gray),
        ))),
        Line::from(Span::styled(
            horizontal_line(area.width),
            Style::default().fg(Color::DarkGray),
        )),
    ];

    for (index, row) in rows.iter().enumerate() {
        let submenu = matches!(row.kind, RowKind::Submenu(_));
        let prefix = if submenu { "> " } else { "  " };
        let text = format!("{prefix}{:<LABEL_WIDTH$} {}", row.label, row.value);
        let text = fit_width(&text, content_width);
        let style = if index == app.current.selected {
            selected_style()
        } else if submenu {
            Style::default().fg(Color::Cyan)
        } else if matches!(row.kind, RowKind::Action(_)) {
            Style::default().fg(Color::Yellow)
        } else {
            Style::default().fg(Color::White)
        };
        lines.push(with_padding(Line::from(Span::styled(text, style))));
    }

    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

fn draw_help(frame: &mut Frame, app: &App, area: Rect) {
    const HOTKEY_ROWS: u16 = 6;
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(6), Constraint::Length(HOTKEY_ROWS)])
        .split(area);
    let rows = app.rows();
    let row = rows.get(app.current.selected);
    let mut lines = vec![
        Line::from(Span::styled(
            "Details",
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
    ];
    if let Some(row) = row {
        lines.push(Line::from(Span::styled(
            row.label,
            Style::default().fg(Color::Cyan),
        )));
        lines.push(Line::from(""));
        lines.push(Line::from(row.help));
    }

    frame.render_widget(
        Paragraph::new(with_horizontal_padding(lines)).wrap(Wrap { trim: false }),
        chunks[0],
    );

    let hotkeys = vec![
        Line::from(Span::styled(
            "Keys",
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from("↑↓ select"),
        Line::from("Enter open/edit page"),
        Line::from("Esc back one page"),
        Line::from("←→/Tab switch section"),
        Line::from("F10 save & restart"),
    ];
    frame.render_widget(
        Paragraph::new(with_horizontal_padding(hotkeys)).wrap(Wrap { trim: false }),
        chunks[1],
    );
}

fn draw_footer(frame: &mut Frame, app: &App, area: Rect, separator_column: u16) {
    let lines = vec![
        Line::from(connected_horizontal_line(
            area.width,
            separator_column,
            BOTTOM_JOINT,
        )),
        status_bar_line(app, area.width),
    ];
    frame.render_widget(Paragraph::new(lines), area);
}

fn draw_dialog(frame: &mut Frame, app: &App, dialog: &Dialog) {
    match dialog {
        Dialog::Choice { field, selected } => draw_choice_dialog(frame, app, *field, *selected),
        Dialog::Input { field, value } => draw_input_dialog(frame, *field, value),
        Dialog::Range { field, value } => draw_range_dialog(frame, *field, *value),
        Dialog::Confirm { action, yes } => draw_confirm_dialog(frame, *action, *yes),
        Dialog::Alert { title, message } => draw_alert_dialog(frame, title, message),
    }
}

fn draw_choice_dialog(frame: &mut Frame, app: &App, field: ChoiceField, selected: usize) {
    let options = app.choice_options(field);
    let height = (options.len() as u16 + 4).clamp(6, 12);
    let area = fixed_rect(46, height, frame.area());
    frame.render_widget(Clear, area);

    let mut lines = vec![
        popup_title_line(choice_title(field), area.width.saturating_sub(2)),
        Line::from(""),
    ];
    for (index, option) in options.iter().enumerate() {
        let style = if index == selected {
            selected_style()
        } else {
            Style::default().fg(Color::White)
        };
        lines.push(Line::from(Span::styled(option.as_str(), style)));
    }
    frame.render_widget(dialog_panel(lines), area);
}

fn draw_input_dialog(frame: &mut Frame, field: InputField, value: &str) {
    let area = fixed_rect(58, 10, frame.area());
    frame.render_widget(Clear, area);
    let title = match field {
        InputField::Number(NumberField::MixedPort) => "Mixed Port",
        InputField::Number(NumberField::HttpPort) => "HTTP Port",
        InputField::Text(TextField::Controller) => "Controller",
        InputField::Text(TextField::Nameserver) => "Nameserver",
        InputField::Text(TextField::SubscriptionName) => "Name",
        InputField::Text(TextField::SubscriptionUrl) => "URL",
    };
    let mut lines = vec![
        popup_title_line(title, area.width.saturating_sub(2)),
        Line::from(""),
    ];
    lines.extend(input_box_lines(
        value,
        area.width.saturating_sub(6) as usize,
    ));
    lines.extend([
        Line::from(""),
        Line::from(Span::styled(
            match field {
                InputField::Number(_) => "Up/Down change | Enter OK | Esc cancel",
                InputField::Text(_) => "Type value | Enter OK | Esc cancel",
            },
            Style::default().fg(Color::Gray),
        )),
    ]);
    frame.render_widget(dialog_panel(lines), area);
}

fn draw_range_dialog(frame: &mut Frame, field: RangeField, value: u8) {
    let area = fixed_rect(58, 8, frame.area());
    frame.render_widget(Clear, area);
    let title = match field {
        RangeField::TrafficLimit => "Traffic Limit",
    };
    let lines = vec![
        popup_title_line(title, area.width.saturating_sub(2)),
        Line::from(""),
        Line::from(format!("  {:>3}%  {}", value, progress_bar(value, 24))),
        Line::from(""),
        Line::from(Span::styled(
            "Left/Right adjust | Enter OK | Esc cancel",
            Style::default().fg(Color::Gray),
        )),
    ];
    frame.render_widget(dialog_panel(lines), area);
}

fn draw_confirm_dialog(frame: &mut Frame, action: ConfirmAction, yes: bool) {
    let area = fixed_rect(42, 6, frame.area());
    frame.render_widget(Clear, area);
    let (title, message, ok_only) = match action {
        ConfirmAction::Exit => ("Exit Without Saving?", "", false),
        ConfirmAction::SaveRestart => ("Save And Restart?", "", false),
        ConfirmAction::Info(message) => ("Info", message, true),
    };

    let mut lines = vec![
        popup_title_line(title, area.width.saturating_sub(2)),
        Line::from(""),
    ];
    if !message.is_empty() {
        lines.push(Line::from(message).alignment(Alignment::Center));
        lines.push(Line::from(""));
    }
    if ok_only {
        lines.push(
            Line::from(Span::styled(" OK ", button_style(true))).alignment(Alignment::Center),
        );
    } else {
        lines.push(Line::from(vec![
            Span::raw("          "),
            Span::styled(" No ", button_style(!yes)),
            Span::raw("              "),
            Span::styled(" Yes ", button_style(yes)),
        ]));
    }

    frame.render_widget(dialog_panel(lines), area);
}

fn draw_alert_dialog(frame: &mut Frame, title: &str, message: &str) {
    let area = fixed_rect(46, 7, frame.area());
    frame.render_widget(Clear, area);

    let lines = vec![
        popup_title_line(title, area.width.saturating_sub(2)),
        Line::from(""),
        Line::from(message).alignment(Alignment::Center),
        Line::from(""),
        Line::from(Span::styled(" OK ", button_style(true))).alignment(Alignment::Center),
    ];

    frame.render_widget(dialog_panel(lines), area);
}

fn choice_title(field: ChoiceField) -> &'static str {
    match field {
        ChoiceField::Subscription => "Subscription",
        ChoiceField::Mode => "Mode",
        ChoiceField::DnsStrategy => "DNS Strategy",
        ChoiceField::Refresh => "Refresh",
        ChoiceField::AddSubscriptionRefresh => "Refresh",
        ChoiceField::LogLevel => "Log Level",
    }
}

fn tab_style(active: bool) -> Style {
    if active {
        Style::default()
            .fg(Color::White)
            .bg(Color::Blue)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::Gray)
    }
}

fn title_bar_line(width: u16) -> Line<'static> {
    let width = width as usize;
    let version = fit_width(APP_VERSION, width);
    let version_width = version.chars().count();
    let version_start = width.saturating_sub(version_width + 1);
    let title = fit_width(APP_TITLE, version_start.saturating_sub(1));
    let title_width = title.chars().count();
    let title_start = width.saturating_sub(title_width) / 2;

    let title_end = title_start + title_width;
    let before_title = " ".repeat(title_start);
    let between = " ".repeat(version_start.saturating_sub(title_end));
    let after_version = " ".repeat(width.saturating_sub(version_start + version_width));

    Line::from(vec![
        Span::raw(before_title),
        Span::styled(
            title,
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(between),
        Span::styled(version, Style::default().fg(Color::DarkGray)),
        Span::raw(after_version),
    ])
}

fn popup_title_line(title: &str, width: u16) -> Line<'static> {
    let width = width.max(1) as usize;
    let title = fit_width(&format!(" {title} "), width);
    let title_width = title.chars().count();
    let left = width.saturating_sub(title_width) / 2;
    let right = width.saturating_sub(title_width + left);
    Line::from(Span::styled(
        format!("{}{}{}", " ".repeat(left), title, " ".repeat(right)),
        Style::default()
            .fg(Color::White)
            .bg(Color::Blue)
            .add_modifier(Modifier::BOLD),
    ))
}

fn selected_style() -> Style {
    Style::default()
        .fg(Color::Black)
        .bg(Color::White)
        .add_modifier(Modifier::BOLD)
}

fn button_style(selected: bool) -> Style {
    if selected {
        selected_style()
    } else {
        Style::default().fg(Color::Gray)
    }
}

fn input_box_lines(value: &str, width: usize) -> Vec<Line<'_>> {
    let inner_width = width.saturating_sub(4).max(8);
    let value = fit_width(value, inner_width);
    vec![
        Line::from(format!("  ┌{}┐", "─".repeat(inner_width))),
        Line::from(format!("  │{value:<inner_width$}│")),
        Line::from(format!("  └{}┘", "─".repeat(inner_width))),
    ]
}

fn progress_bar(value: u8, width: usize) -> String {
    let width = width.max(1);
    let filled = (value as usize * width) / 100;
    let empty = width.saturating_sub(filled);
    format!("[{}{}]", "█".repeat(filled), "░".repeat(empty))
}

fn dialog_panel<'a>(lines: Vec<Line<'a>>) -> Paragraph<'a> {
    Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL))
        .wrap(Wrap { trim: false })
}

fn draw_vertical_separator(frame: &mut Frame, area: Rect) {
    let lines = (0..area.height)
        .map(|_| {
            Line::from(Span::styled(
                V_LINE.to_string(),
                Style::default().fg(Color::Gray),
            ))
        })
        .collect::<Vec<_>>();
    frame.render_widget(Paragraph::new(lines), area);
}

fn fit_width(value: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let mut output = String::new();
    for (index, ch) in value.chars().enumerate() {
        if index + 1 >= width {
            output.push('…');
            return output;
        }
        output.push(ch);
    }
    output
}

fn with_horizontal_padding<'a>(lines: Vec<Line<'a>>) -> Vec<Line<'a>> {
    lines.into_iter().map(with_padding).collect()
}

fn with_padding<'a>(mut line: Line<'a>) -> Line<'a> {
    line.spans.insert(0, Span::raw(" "));
    line.spans.push(Span::raw(" "));
    line
}

fn status_bar_line(app: &App, width: u16) -> Line<'static> {
    let width = width as usize;
    let right = format!(" │ {} │ {} ", config_state(app), service_state(app));
    let right_width = right.chars().count();

    if width <= right_width {
        return Line::from(Span::styled(
            fit_width(&right, width),
            Style::default().fg(Color::Gray),
        ));
    }

    let left_width = width - right_width;
    let has_status = !app.status.trim().is_empty();
    let left_text = if has_status {
        app.status.clone()
    } else {
        breadcrumb(app)
    };
    let left_text = fit_width(&left_text, left_width.saturating_sub(1));
    let gap = " ".repeat(left_width.saturating_sub(left_text.chars().count()));
    let left_style = if has_status {
        Style::default().fg(Color::White)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    Line::from(vec![
        Span::styled(left_text, left_style),
        Span::raw(gap),
        Span::styled(right, Style::default().fg(Color::Gray)),
    ])
}

fn breadcrumb(app: &App) -> String {
    app.stack
        .iter()
        .map(|location| location.page.title())
        .chain(std::iter::once(app.current.page.title()))
        .collect::<Vec<_>>()
        .join(" / ")
}

fn config_state(app: &App) -> &'static str {
    if !app.draft_subscription_name.trim().is_empty()
        || !app.draft_subscription_url.trim().is_empty()
    {
        "cfg draft"
    } else if app.dirty {
        "cfg changed"
    } else {
        "cfg saved"
    }
}

fn service_state(app: &App) -> &'static str {
    if app.system_proxy || app.port_proxy {
        "svc running"
    } else {
        "svc idle"
    }
}

fn split_column(width: u16) -> u16 {
    width.saturating_mul(66) / 100
}

fn connected_horizontal_line(width: u16, joint: u16, joint_char: char) -> String {
    (0..width)
        .map(|index| if index == joint { joint_char } else { H_LINE })
        .collect()
}

fn horizontal_line(width: u16) -> String {
    H_LINE.to_string().repeat(width as usize)
}

fn fixed_rect(width: u16, height: u16, area: Rect) -> Rect {
    let width = width.min(area.width.saturating_sub(2)).max(20);
    let height = height.min(area.height.saturating_sub(2)).max(6);
    Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    }
}

const fn on_off(value: bool) -> &'static str {
    if value { "on" } else { "off" }
}

struct TerminalGuard {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl TerminalGuard {
    fn enter() -> Result<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen)?;
        let backend = CrosstermBackend::new(stdout);
        let terminal = Terminal::new(backend)?;
        Ok(Self { terminal })
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(self.terminal.backend_mut(), LeaveAlternateScreen);
        let _ = self.terminal.show_cursor();
    }
}
