#![allow(dead_code)]

use std::io::{self, IsTerminal as _, Stdout};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::{Frame, Terminal};

use crate::config::{AppConfig, Paths, Subscription, SubscriptionRefresh};
use crate::core;
use crate::dns;
use crate::mihomo::{MihomoClient, ProxyGroup};
use crate::runtime_profile;
use crate::subscription;

const TICK_RATE: Duration = Duration::from_millis(200);
const REFRESH_INTERVAL: Duration = Duration::from_secs(5);
const IPINFO_REFRESH_INTERVAL: Duration = Duration::from_secs(10 * 60);
const LABEL_WIDTH: usize = 24;
const APP_TITLE: &str = "ClashTUI Config";
const APP_VERSION: &str = concat!("v", env!("CARGO_PKG_VERSION"));
const H_LINE: char = '─';
const V_LINE: char = '│';
const TOP_JOINT: char = '┬';
const BOTTOM_JOINT: char = '┴';

pub async fn run(paths: &Paths, config: &mut AppConfig) -> Result<()> {
    paths.ensure().await?;
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        print_config(paths, config)?;
        return Ok(());
    }

    let mut terminal = TerminalGuard::enter()?;
    let mut app = ConfigApp::new(config);
    app.refresh_runtime(config).await;
    let mut last_refresh = Instant::now();

    while !app.should_quit {
        terminal
            .terminal
            .draw(|frame| draw(frame, paths, config, &app))?;
        if event::poll(TICK_RATE)?
            && let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
        {
            handle_key(paths, config, &mut app, key).await?;
        }
        if last_refresh.elapsed() >= REFRESH_INTERVAL {
            app.refresh_runtime(config).await;
            last_refresh = Instant::now();
        }
    }

    if app.save_on_exit {
        config.save(paths).await?;
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Page {
    Main,
    Subscription,
    AddSubscription,
    Runtime,
    Chat,
    Help,
    ProxyConfig,
    ProxyGroups,
    Mode,
    Dns,
}

impl Page {
    const SECTION_ROOTS: [Self; 5] = [
        Self::Main,
        Self::Subscription,
        Self::Runtime,
        Self::Chat,
        Self::Help,
    ];

    fn next(self) -> Self {
        let index = self.section_index();
        Self::SECTION_ROOTS[(index + 1) % Self::SECTION_ROOTS.len()]
    }

    fn prev(self) -> Self {
        let index = self.section_index();
        if index == 0 {
            Self::SECTION_ROOTS[Self::SECTION_ROOTS.len() - 1]
        } else {
            Self::SECTION_ROOTS[index - 1]
        }
    }

    const fn title(self) -> &'static str {
        match self {
            Self::Main => "Main",
            Self::Subscription => "Subscription",
            Self::AddSubscription => "Add Subscription",
            Self::Runtime => "Runtime",
            Self::Chat => "Chat",
            Self::Help => "Help",
            Self::ProxyConfig => "Proxy",
            Self::ProxyGroups => "Proxy Groups",
            Self::Mode => "Mode",
            Self::Dns => "DNS",
        }
    }

    fn is_section(self) -> bool {
        Self::SECTION_ROOTS.contains(&self)
    }

    const fn section_index(self) -> usize {
        match self {
            Self::Main | Self::ProxyConfig | Self::ProxyGroups | Self::Mode | Self::Dns => 0,
            Self::Subscription | Self::AddSubscription => 1,
            Self::Runtime => 2,
            Self::Chat => 3,
            Self::Help => 4,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RuntimeItem {
    Service,
    TunPermissions,
    Logs,
    CorePath,
    Controller,
    Dns,
    Refresh,
}

impl RuntimeItem {
    const ALL: [Self; 7] = [
        Self::Service,
        Self::TunPermissions,
        Self::Logs,
        Self::CorePath,
        Self::Controller,
        Self::Dns,
        Self::Refresh,
    ];

    const fn label(self) -> &'static str {
        match self {
            Self::Service => "Service",
            Self::TunPermissions => "TUN Permissions",
            Self::Logs => "Logs",
            Self::CorePath => "Mihomo Core",
            Self::Controller => "Controller",
            Self::Dns => "DNS",
            Self::Refresh => "Refresh Runtime",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProxyConfigField {
    Enabled,
    Subscription,
    Mode,
    ProxyGroups,
    LocalPort,
    OsProxy,
    Pac,
    Tun,
    Dns,
}

impl ProxyConfigField {
    const SYSTEM_ALL: [Self; 9] = [
        Self::Enabled,
        Self::Subscription,
        Self::Mode,
        Self::ProxyGroups,
        Self::LocalPort,
        Self::OsProxy,
        Self::Pac,
        Self::Tun,
        Self::Dns,
    ];

    const PORT_ALL: [Self; 5] = [
        Self::Enabled,
        Self::Subscription,
        Self::Mode,
        Self::ProxyGroups,
        Self::LocalPort,
    ];

    const fn label(self) -> &'static str {
        match self {
            Self::Enabled => "Enabled",
            Self::Subscription => "Subscription",
            Self::Mode => "Mode",
            Self::ProxyGroups => "Proxy Server",
            Self::LocalPort => "Local Port",
            Self::OsProxy => "OS System Proxy",
            Self::Pac => "PAC",
            Self::Tun => "TUN",
            Self::Dns => "DNS",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ModeItem {
    Rule,
    Global,
    Direct,
}

impl ModeItem {
    const ALL: [Self; 3] = [Self::Rule, Self::Global, Self::Direct];

    const fn label(self) -> &'static str {
        match self {
            Self::Rule => "Rule",
            Self::Global => "Global",
            Self::Direct => "Direct",
        }
    }

    const fn value(self) -> &'static str {
        match self {
            Self::Rule => "rule",
            Self::Global => "global",
            Self::Direct => "direct",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DnsItem {
    Enabled,
    Listen,
    LanDomains,
    LanNameserver,
    DirectNameserver,
    DirectFollowPolicy,
    Nameserver,
    Fallback,
    FakeIpFilter,
}

impl DnsItem {
    const ALL: [Self; 9] = [
        Self::Enabled,
        Self::Listen,
        Self::LanDomains,
        Self::LanNameserver,
        Self::DirectNameserver,
        Self::DirectFollowPolicy,
        Self::Nameserver,
        Self::Fallback,
        Self::FakeIpFilter,
    ];

    const fn label(self) -> &'static str {
        match self {
            Self::Enabled => "DNS",
            Self::Listen => "Listen",
            Self::LanDomains => "LAN Domains",
            Self::LanNameserver => "LAN DNS",
            Self::DirectNameserver => "Direct DNS",
            Self::DirectFollowPolicy => "Direct follows policy",
            Self::Nameserver => "Default DNS",
            Self::Fallback => "Fallback DNS",
            Self::FakeIpFilter => "Fake-IP Filter",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProxyPane {
    Groups,
    Proxies,
}

impl ProxyPane {
    const fn title(self) -> &'static str {
        match self {
            Self::Groups => "Groups",
            Self::Proxies => "Proxies",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InputMode {
    Normal,
    SubscriptionName,
    SubscriptionUrl,
    CorePath,
    Controller,
    MixedPort,
    HttpPort,
    SocksPort,
    DnsListen,
    DnsLanDomains,
    DnsLanNameserver,
    DnsDirectNameserver,
    DnsNameserver,
    DnsFallback,
    DnsFakeIpFilter,
}

impl InputMode {
    const fn title(self) -> &'static str {
        match self {
            Self::Normal => "",
            Self::SubscriptionName => "Subscription Name",
            Self::SubscriptionUrl => "Subscription URL",
            Self::CorePath => "Mihomo Core Path",
            Self::Controller => "Controller URL",
            Self::MixedPort => "Mixed Port",
            Self::HttpPort => "HTTP Port",
            Self::SocksPort => "SOCKS Port",
            Self::DnsListen => "DNS Listen",
            Self::DnsLanDomains => "LAN Domains",
            Self::DnsLanNameserver => "LAN DNS",
            Self::DnsDirectNameserver => "Direct DNS",
            Self::DnsNameserver => "Default DNS",
            Self::DnsFallback => "Fallback DNS",
            Self::DnsFakeIpFilter => "Fake-IP Filter",
        }
    }
}

#[derive(Default)]
struct RuntimeState {
    version: Option<String>,
    mode: Option<String>,
    groups: Vec<ProxyGroup>,
    traffic: Option<TrafficState>,
    ip_info: Option<IpInfoState>,
    ip_info_error: Option<String>,
    error: Option<String>,
}

#[derive(Default)]
struct TrafficState {
    download_total: u64,
    upload_speed: u64,
    download_speed: u64,
}

struct IpInfoState {
    ip: String,
    country: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SubscriptionFormField {
    Name,
    Url,
    Refresh,
    Ok,
}

impl SubscriptionFormField {
    const ALL: [Self; 4] = [Self::Name, Self::Url, Self::Refresh, Self::Ok];
}

#[derive(Default)]
struct SubscriptionForm {
    name: String,
    url: String,
    refresh: SubscriptionRefresh,
    selected: usize,
}

impl SubscriptionForm {
    fn clear(&mut self) {
        self.name.clear();
        self.url.clear();
        self.refresh = SubscriptionRefresh::default();
        self.selected = 0;
    }

    fn selected_field(&self) -> SubscriptionFormField {
        SubscriptionFormField::ALL
            .get(self.selected)
            .copied()
            .unwrap_or(SubscriptionFormField::Name)
    }

    fn next_field(&mut self) {
        self.selected = next_index(self.selected, SubscriptionFormField::ALL.len());
    }

    fn prev_field(&mut self) {
        self.selected = prev_index(self.selected, SubscriptionFormField::ALL.len());
    }

    fn next_refresh(&mut self) {
        self.refresh = match self.refresh {
            SubscriptionRefresh::Daily => SubscriptionRefresh::Weekly,
            SubscriptionRefresh::Weekly => SubscriptionRefresh::Disabled,
            SubscriptionRefresh::Disabled => SubscriptionRefresh::Daily,
        };
    }

    fn prev_refresh(&mut self) {
        self.refresh = match self.refresh {
            SubscriptionRefresh::Daily => SubscriptionRefresh::Disabled,
            SubscriptionRefresh::Weekly => SubscriptionRefresh::Daily,
            SubscriptionRefresh::Disabled => SubscriptionRefresh::Weekly,
        };
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConfirmAction {
    ExitWithoutSaving,
    LoadDefaults,
    SaveRestart,
    SaveRestartExit,
}

impl ConfirmAction {
    const fn title(self) -> &'static str {
        match self {
            Self::ExitWithoutSaving => "Exit Without Saving?",
            Self::LoadDefaults => "Load Setup Defaults?",
            Self::SaveRestart => "Save And Restart?",
            Self::SaveRestartExit => "Save, Restart, And Exit?",
        }
    }

    const fn message(self) -> &'static [&'static str] {
        match self {
            Self::ExitWithoutSaving => &[
                "Exit ClashTUI Config without saving pending changes?",
                "Some edits are saved immediately in this preview.",
            ],
            Self::LoadDefaults => &[
                "Load setup defaults for configurable fields?",
                "This action is not implemented in this preview.",
            ],
            Self::SaveRestart => &["Save config and restart mihomo now?"],
            Self::SaveRestartExit => &["Save config, restart mihomo, then exit?"],
        }
    }

    const fn yes_label(self) -> &'static str {
        match self {
            Self::ExitWithoutSaving => "Yes, Exit",
            Self::LoadDefaults => "Yes, Load Defaults",
            Self::SaveRestart => "Yes, Save & Restart",
            Self::SaveRestartExit => "Yes, Save & Restart & Exit",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Dropdown {
    ProxySubscription,
    Mode,
    SubscriptionRefresh,
}

#[derive(Debug, Clone, Copy)]
struct Location {
    section: Page,
    page: Page,
    selected: usize,
}

#[derive(Debug, Clone)]
struct SettingRow {
    label: String,
    value: String,
    help: String,
    kind: RowKind,
}

#[derive(Debug, Clone, Copy)]
enum RowKind {
    Submenu(Page),
    Toggle(ToggleAction),
    Choice(ChoiceAction),
    Input(InputMode),
    Action(ActionKind),
    Info,
}

#[derive(Debug, Clone, Copy)]
enum ToggleAction {
    SystemProxy,
    Tun,
    Dns,
    DnsDirectFollowPolicy,
}

#[derive(Debug, Clone, Copy)]
enum ChoiceAction {
    Subscription,
    Mode,
    SubscriptionRefresh,
}

#[derive(Debug, Clone, Copy)]
enum ActionKind {
    SaveSubscription,
    Service,
    TunPermissions,
    Logs,
    RefreshRuntime,
    SelectProxyGroup,
    SelectProxy,
    HelpBack,
}

#[derive(Debug, Clone)]
struct Alert {
    title: String,
    message: String,
}

struct ConfigApp {
    page: Page,
    section: Page,
    history: Vec<Location>,
    proxy_pane: ProxyPane,
    selected_main: usize,
    selected_runtime: usize,
    selected_subscription: usize,
    selected_dropdown: usize,
    selected_group: usize,
    selected_proxy: usize,
    selected_proxy_field: usize,
    selected_mode: usize,
    selected_dns: usize,
    input_mode: InputMode,
    dropdown: Option<Dropdown>,
    input: String,
    subscription_form: SubscriptionForm,
    runtime: RuntimeState,
    last_ip_info_refresh: Option<Instant>,
    status: String,
    should_quit: bool,
    save_on_exit: bool,
    confirm: Option<ConfirmAction>,
    confirm_yes: bool,
    alert: Option<Alert>,
}

impl ConfigApp {
    fn new(config: &AppConfig) -> Self {
        Self {
            page: Page::Main,
            section: Page::Main,
            history: Vec::new(),
            proxy_pane: ProxyPane::Groups,
            selected_main: 0,
            selected_runtime: 0,
            selected_subscription: active_subscription_index(config).unwrap_or_default(),
            selected_dropdown: 0,
            selected_group: 0,
            selected_proxy: 0,
            selected_proxy_field: 0,
            selected_mode: runtime_mode_index(config),
            selected_dns: 0,
            input_mode: InputMode::Normal,
            dropdown: None,
            input: String::new(),
            subscription_form: SubscriptionForm::default(),
            runtime: RuntimeState::default(),
            last_ip_info_refresh: None,
            status: String::new(),
            should_quit: false,
            save_on_exit: true,
            confirm: None,
            confirm_yes: false,
            alert: None,
        }
    }

    async fn refresh_runtime(&mut self, config: &AppConfig) {
        let client = MihomoClient::new(&config.controller);
        let (version, configs, groups, connections) = tokio::join!(
            client.version(),
            client.configs(),
            client.proxy_groups(),
            client.connections()
        );
        self.runtime.traffic = connections
            .ok()
            .and_then(|value| traffic_from_value(&value));

        match (version, configs, groups) {
            (Ok(version), Ok(configs), Ok(groups)) => {
                self.runtime.version = Some(version);
                self.runtime.mode = configs
                    .get("mode")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string);
                self.runtime.groups = groups;
                self.runtime.error = None;
                self.clamp_runtime_selection();
            }
            (version, configs, groups) => {
                self.runtime.version = version.ok();
                self.runtime.mode = configs.ok().and_then(|value| {
                    value
                        .get("mode")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_string)
                });
                self.runtime.groups = groups.unwrap_or_default();
                self.runtime.error = Some("mihomo offline or /proxies unavailable".into());
                self.clamp_runtime_selection();
            }
        }

        if self.should_refresh_ip_info() {
            self.last_ip_info_refresh = Some(Instant::now());
            match fetch_ip_info().await {
                Ok(ip_info) => {
                    self.runtime.ip_info = Some(ip_info);
                    self.runtime.ip_info_error = None;
                }
                Err(err) => {
                    if self.runtime.ip_info.is_none() {
                        self.runtime.ip_info_error = Some(err.to_string());
                    }
                }
            }
        }
    }

    fn should_refresh_ip_info(&self) -> bool {
        match self.last_ip_info_refresh {
            Some(last_refresh) => last_refresh.elapsed() >= IPINFO_REFRESH_INTERVAL,
            None => true,
        }
    }

    fn current_group(&self) -> Option<&ProxyGroup> {
        self.runtime.groups.get(self.selected_group)
    }

    fn selected_runtime_item(&self) -> RuntimeItem {
        RuntimeItem::ALL
            .get(self.selected_runtime)
            .copied()
            .unwrap_or(RuntimeItem::Service)
    }

    fn selected_dns_item(&self) -> DnsItem {
        DnsItem::ALL
            .get(self.selected_dns)
            .copied()
            .unwrap_or(DnsItem::Enabled)
    }

    fn proxy_config_fields(&self) -> &'static [ProxyConfigField] {
        if self.selected_main == 0 {
            &ProxyConfigField::SYSTEM_ALL
        } else {
            &ProxyConfigField::PORT_ALL
        }
    }

    fn selected_proxy_config_field(&self) -> ProxyConfigField {
        self.proxy_config_fields()
            .get(self.selected_proxy_field)
            .copied()
            .unwrap_or(ProxyConfigField::Enabled)
    }

    fn clamp_selection(&mut self, config: &AppConfig) {
        let proxy_count = main_proxy_count(config);
        if proxy_count == 0 {
            self.selected_main = 0;
        } else if self.selected_main >= proxy_count {
            self.selected_main = proxy_count - 1;
        }
        if self.selected_runtime >= RuntimeItem::ALL.len() {
            self.selected_runtime = RuntimeItem::ALL.len() - 1;
        }
        if self.selected_proxy_field >= self.proxy_config_fields().len() {
            self.selected_proxy_field = self.proxy_config_fields().len() - 1;
        }
        if self.selected_mode >= ModeItem::ALL.len() {
            self.selected_mode = ModeItem::ALL.len() - 1;
        }
        if self.selected_dns >= DnsItem::ALL.len() {
            self.selected_dns = DnsItem::ALL.len() - 1;
        }
        if self.subscription_form.selected >= SubscriptionFormField::ALL.len() {
            self.subscription_form.selected = SubscriptionFormField::ALL.len() - 1;
        }
        let subscription_count = subscription_menu_count(config);
        if self.selected_subscription >= subscription_count {
            self.selected_subscription = subscription_count - 1;
        }
        let dropdown_count = self.dropdown_item_count(config);
        if dropdown_count > 0 && self.selected_dropdown >= dropdown_count {
            self.selected_dropdown = dropdown_count - 1;
        }
        self.clamp_runtime_selection();
    }

    fn clamp_runtime_selection(&mut self) {
        if self.runtime.groups.is_empty() {
            self.selected_group = 0;
            self.selected_proxy = 0;
            return;
        }
        if self.selected_group >= self.runtime.groups.len() {
            self.selected_group = self.runtime.groups.len() - 1;
        }
        let proxy_count = self.runtime.groups[self.selected_group].all.len();
        if proxy_count == 0 {
            self.selected_proxy = 0;
        } else if self.selected_proxy >= proxy_count {
            self.selected_proxy = self
                .runtime
                .groups
                .get(self.selected_group)
                .and_then(|group| group.all.iter().position(|proxy| proxy == &group.now))
                .unwrap_or(0);
        }
    }

    fn selected_index(&self) -> usize {
        match self.page {
            Page::Main => self.selected_main,
            Page::Subscription => self.selected_subscription,
            Page::AddSubscription => self.subscription_form.selected,
            Page::Runtime => self.selected_runtime,
            Page::ProxyConfig => self.selected_proxy_field,
            Page::ProxyGroups => match self.proxy_pane {
                ProxyPane::Groups => self.selected_group,
                ProxyPane::Proxies => self.selected_proxy,
            },
            Page::Mode => self.selected_mode,
            Page::Dns => self.selected_dns,
            Page::Chat | Page::Help => 0,
        }
    }

    fn set_selected_index(&mut self, selected: usize) {
        match self.page {
            Page::Main => self.selected_main = selected,
            Page::Subscription => self.selected_subscription = selected,
            Page::AddSubscription => self.subscription_form.selected = selected,
            Page::Runtime => self.selected_runtime = selected,
            Page::ProxyConfig => self.selected_proxy_field = selected,
            Page::ProxyGroups => match self.proxy_pane {
                ProxyPane::Groups => self.selected_group = selected,
                ProxyPane::Proxies => self.selected_proxy = selected,
            },
            Page::Mode => self.selected_mode = selected,
            Page::Dns => self.selected_dns = selected,
            Page::Chat | Page::Help => {}
        }
    }

    fn current_location(&self) -> Location {
        Location {
            section: self.section,
            page: self.page,
            selected: self.selected_index(),
        }
    }

    fn restore_location(&mut self, location: Location) {
        self.section = location.section;
        self.page = location.page;
        self.set_selected_index(location.selected);
    }

    fn enter_page(&mut self, page: Page, status: impl Into<String>) {
        self.dropdown = None;
        if self.page != page {
            self.history.push(self.current_location());
            self.page = page;
            self.set_selected_index(0);
            if page == Page::AddSubscription
                && self.subscription_form.name.trim().is_empty()
                && self.subscription_form.url.trim().is_empty()
            {
                self.subscription_form.refresh = SubscriptionRefresh::default();
            }
        }
        self.status = status.into();
    }

    fn replace_page_with_status(&mut self, page: Page, status: impl Into<String>) {
        self.dropdown = None;
        self.page = page;
        if page.is_section() {
            self.section = page;
        }
        self.status = status.into();
    }

    fn go_back(&mut self) -> bool {
        self.dropdown = None;
        if let Some(location) = self.history.pop() {
            self.restore_location(location);
            self.status = format!("Back to {}", self.page.title());
            return true;
        }
        false
    }

    fn back_or_confirm_exit(&mut self) {
        if self.go_back() {
            return;
        }
        self.open_confirm(ConfirmAction::ExitWithoutSaving);
    }

    fn return_to_previous_or(&mut self, fallback: Page, status: impl Into<String>) {
        let status = status.into();
        if !self.go_back() {
            self.replace_page_with_status(fallback, status.clone());
        }
        self.status = status;
    }

    fn can_switch_sections(&self) -> bool {
        self.history.is_empty() && self.page == self.section && self.section.is_section()
    }

    fn switch_section(&mut self, section: Page) {
        if !section.is_section() {
            return;
        }
        if !self.can_switch_sections() {
            self.status = "Back to section root before switching sections".into();
            return;
        }
        self.section = section;
        self.page = section;
        self.set_selected_index(0);
        self.status.clear();
    }

    fn next_page(&mut self) {
        self.switch_section(self.section.next());
    }

    fn prev_page(&mut self) {
        self.switch_section(self.section.prev());
    }

    fn move_right(&mut self) {
        if self.page == Page::ProxyGroups && self.proxy_pane == ProxyPane::Groups {
            self.proxy_pane = ProxyPane::Proxies;
            self.selected_proxy = current_proxy_index(self.current_group()).unwrap_or_default();
            self.status = "Proxy pane: Proxies".into();
        } else {
            self.next_page();
        }
    }

    fn move_left(&mut self) {
        if self.page == Page::ProxyGroups && self.proxy_pane == ProxyPane::Proxies {
            self.proxy_pane = ProxyPane::Groups;
            self.status = "Proxy pane: Groups".into();
        } else {
            self.prev_page();
        }
    }

    fn move_next(&mut self, config: &AppConfig) {
        match self.page {
            Page::Main => {
                self.selected_main = next_index(self.selected_main, main_proxy_count(config))
            }
            Page::Subscription => self.select_next_subscription(config),
            Page::AddSubscription => self.subscription_form.next_field(),
            Page::ProxyConfig => {
                self.selected_proxy_field =
                    next_index(self.selected_proxy_field, self.proxy_config_fields().len())
            }
            Page::ProxyGroups => match self.proxy_pane {
                ProxyPane::Groups => self.select_next_group(),
                ProxyPane::Proxies => self.select_next_proxy(),
            },
            Page::Mode => self.selected_mode = next_index(self.selected_mode, ModeItem::ALL.len()),
            Page::Dns => self.selected_dns = next_index(self.selected_dns, DnsItem::ALL.len()),
            Page::Runtime => {
                self.selected_runtime = next_index(self.selected_runtime, RuntimeItem::ALL.len())
            }
            Page::Chat | Page::Help => {}
        }
    }

    fn move_prev(&mut self, config: &AppConfig) {
        match self.page {
            Page::Main => {
                self.selected_main = prev_index(self.selected_main, main_proxy_count(config))
            }
            Page::Subscription => self.select_prev_subscription(config),
            Page::AddSubscription => self.subscription_form.prev_field(),
            Page::ProxyConfig => {
                self.selected_proxy_field =
                    prev_index(self.selected_proxy_field, self.proxy_config_fields().len())
            }
            Page::ProxyGroups => match self.proxy_pane {
                ProxyPane::Groups => self.select_prev_group(),
                ProxyPane::Proxies => self.select_prev_proxy(),
            },
            Page::Mode => self.selected_mode = prev_index(self.selected_mode, ModeItem::ALL.len()),
            Page::Dns => self.selected_dns = prev_index(self.selected_dns, DnsItem::ALL.len()),
            Page::Runtime => {
                self.selected_runtime = prev_index(self.selected_runtime, RuntimeItem::ALL.len())
            }
            Page::Chat | Page::Help => {}
        }
    }

    fn select_next_subscription(&mut self, config: &AppConfig) {
        self.selected_subscription =
            (self.selected_subscription + 1) % subscription_menu_count(config);
    }

    fn select_prev_subscription(&mut self, config: &AppConfig) {
        self.selected_subscription = if self.selected_subscription == 0 {
            subscription_menu_count(config) - 1
        } else {
            self.selected_subscription - 1
        };
    }

    fn select_next_group(&mut self) {
        if self.runtime.groups.is_empty() {
            return;
        }
        self.selected_group = (self.selected_group + 1) % self.runtime.groups.len();
        self.selected_proxy = current_proxy_index(self.current_group()).unwrap_or_default();
    }

    fn select_prev_group(&mut self) {
        if self.runtime.groups.is_empty() {
            return;
        }
        self.selected_group = if self.selected_group == 0 {
            self.runtime.groups.len() - 1
        } else {
            self.selected_group - 1
        };
        self.selected_proxy = current_proxy_index(self.current_group()).unwrap_or_default();
    }

    fn select_next_proxy(&mut self) {
        let Some(group) = self.current_group() else {
            return;
        };
        if group.all.is_empty() {
            return;
        }
        self.selected_proxy = (self.selected_proxy + 1) % group.all.len();
    }

    fn select_prev_proxy(&mut self) {
        let Some(group) = self.current_group() else {
            return;
        };
        if group.all.is_empty() {
            return;
        }
        self.selected_proxy = if self.selected_proxy == 0 {
            group.all.len() - 1
        } else {
            self.selected_proxy - 1
        };
    }

    fn open_subscription_dropdown(&mut self, config: &AppConfig) {
        self.dropdown = Some(Dropdown::ProxySubscription);
        self.selected_dropdown = config
            .active_profile
            .as_ref()
            .and_then(|name| {
                config
                    .subscriptions
                    .iter()
                    .position(|subscription| &subscription.name == name)
            })
            .unwrap_or_default();
        self.status = "Choose subscription, then press Enter".into();
    }

    fn open_mode_dropdown(&mut self, config: &AppConfig) {
        self.dropdown = Some(Dropdown::Mode);
        self.selected_dropdown = runtime_mode_index(config);
        self.status = "Choose mode, then press Enter".into();
    }

    fn open_subscription_refresh_dropdown(&mut self) {
        self.dropdown = Some(Dropdown::SubscriptionRefresh);
        self.selected_dropdown = subscription_refresh_index(self.subscription_form.refresh);
        self.status = "Choose refresh interval, then press Enter".into();
    }

    fn close_dropdown(&mut self) {
        self.dropdown = None;
        self.status = "Canceled".into();
    }

    fn dropdown_item_count(&self, config: &AppConfig) -> usize {
        match self.dropdown {
            Some(Dropdown::ProxySubscription) => subscription_dropdown_count(config),
            Some(Dropdown::Mode) => ModeItem::ALL.len(),
            Some(Dropdown::SubscriptionRefresh) => SUBSCRIPTION_REFRESH_OPTIONS.len(),
            None => 0,
        }
    }

    fn select_next_dropdown(&mut self, config: &AppConfig) {
        let count = self.dropdown_item_count(config);
        if count > 0 {
            self.selected_dropdown = (self.selected_dropdown + 1) % count;
        }
    }

    fn select_prev_dropdown(&mut self, config: &AppConfig) {
        let count = self.dropdown_item_count(config);
        if count == 0 {
            return;
        }
        self.selected_dropdown = if self.selected_dropdown == 0 {
            count - 1
        } else {
            self.selected_dropdown - 1
        };
    }

    const fn is_input(&self) -> bool {
        !matches!(self.input_mode, InputMode::Normal)
    }

    fn cancel_input(&mut self) {
        self.input_mode = InputMode::Normal;
        self.input.clear();
        self.status = "Canceled".into();
    }

    fn open_confirm(&mut self, action: ConfirmAction) {
        self.confirm = Some(action);
        self.confirm_yes = false;
        self.status = format!("Confirm: {}", action.title());
    }

    fn cancel_confirm(&mut self) {
        self.confirm = None;
        self.confirm_yes = false;
        self.status = "Canceled".into();
    }

    fn set_confirm_choice(&mut self, yes: bool) {
        self.confirm_yes = yes;
    }

    fn alert(&mut self, title: impl Into<String>, message: impl Into<String>) {
        self.alert = Some(Alert {
            title: title.into(),
            message: message.into(),
        });
        self.status.clear();
    }

    fn close_alert(&mut self) {
        self.alert = None;
    }
}

async fn handle_key(
    paths: &Paths,
    config: &mut AppConfig,
    app: &mut ConfigApp,
    key: KeyEvent,
) -> Result<()> {
    if app.alert.is_some() {
        match key.code {
            KeyCode::Esc | KeyCode::Enter | KeyCode::Char(' ') => app.close_alert(),
            _ => {}
        }
        return Ok(());
    }

    if app.confirm.is_some() {
        match key.code {
            KeyCode::Esc => app.cancel_confirm(),
            KeyCode::Left | KeyCode::Up => app.set_confirm_choice(false),
            KeyCode::Right | KeyCode::Down => app.set_confirm_choice(true),
            KeyCode::Enter => submit_confirm(paths, config, app).await?,
            _ => {}
        }
        return Ok(());
    }

    if app.dropdown.is_some() {
        return handle_dropdown_key(paths, config, app, key).await;
    }

    if app.is_input() {
        return handle_input_key(paths, config, app, key).await;
    }

    match key.code {
        KeyCode::F(9) => app.open_confirm(ConfirmAction::LoadDefaults),
        KeyCode::F(10) => app.open_confirm(ConfirmAction::SaveRestart),
        KeyCode::F(11) => app.open_confirm(ConfirmAction::SaveRestartExit),
        KeyCode::Esc => app.back_or_confirm_exit(),
        KeyCode::F(1) => {
            app.enter_page(Page::Help, "Help");
        }
        KeyCode::Tab => app.next_page(),
        KeyCode::BackTab => app.prev_page(),
        KeyCode::Right => app.move_right(),
        KeyCode::Left => app.move_left(),
        KeyCode::Down => app.move_next(config),
        KeyCode::Up => app.move_prev(config),
        KeyCode::Enter | KeyCode::Char(' ') => submit_selection(paths, config, app).await?,
        _ => {}
    }
    app.clamp_selection(config);
    Ok(())
}

async fn handle_dropdown_key(
    paths: &Paths,
    config: &mut AppConfig,
    app: &mut ConfigApp,
    key: KeyEvent,
) -> Result<()> {
    match key.code {
        KeyCode::Esc => app.close_dropdown(),
        KeyCode::Down => app.select_next_dropdown(config),
        KeyCode::Up => app.select_prev_dropdown(config),
        KeyCode::Enter | KeyCode::Char(' ') => submit_dropdown(paths, config, app).await?,
        _ => {}
    }
    app.clamp_selection(config);
    Ok(())
}

async fn handle_input_key(
    paths: &Paths,
    config: &mut AppConfig,
    app: &mut ConfigApp,
    key: KeyEvent,
) -> Result<()> {
    match key.code {
        KeyCode::Esc => app.cancel_input(),
        KeyCode::Backspace => {
            app.input.pop();
        }
        KeyCode::Enter => submit_input(paths, config, app).await?,
        KeyCode::Up if is_number_input(app.input_mode) => adjust_number_input(&mut app.input, 1),
        KeyCode::Down if is_number_input(app.input_mode) => adjust_number_input(&mut app.input, -1),
        KeyCode::Char(value) if is_number_input(app.input_mode) && value.is_ascii_digit() => {
            push_number_digit(&mut app.input, value);
        }
        KeyCode::Char(value) => app.input.push(value),
        _ => {}
    }
    Ok(())
}

async fn handle_subscription_form_key(
    paths: &Paths,
    config: &mut AppConfig,
    app: &mut ConfigApp,
    key: KeyEvent,
) -> Result<()> {
    match key.code {
        KeyCode::Esc => app.cancel_input(),
        KeyCode::Tab | KeyCode::Down => app.subscription_form.next_field(),
        KeyCode::BackTab | KeyCode::Up => app.subscription_form.prev_field(),
        KeyCode::Left
            if app.subscription_form.selected_field() == SubscriptionFormField::Refresh =>
        {
            app.subscription_form.prev_refresh();
        }
        KeyCode::Right
            if app.subscription_form.selected_field() == SubscriptionFormField::Refresh =>
        {
            app.subscription_form.next_refresh();
        }
        KeyCode::Backspace => match app.subscription_form.selected_field() {
            SubscriptionFormField::Name => {
                app.subscription_form.name.pop();
            }
            SubscriptionFormField::Url => {
                app.subscription_form.url.pop();
            }
            SubscriptionFormField::Refresh | SubscriptionFormField::Ok => {}
        },
        KeyCode::Enter => {
            if app.subscription_form.selected_field() == SubscriptionFormField::Ok {
                submit_subscription_form(paths, config, app).await?;
            } else {
                app.subscription_form.next_field();
            }
        }
        KeyCode::Char(value) => match app.subscription_form.selected_field() {
            SubscriptionFormField::Name => app.subscription_form.name.push(value),
            SubscriptionFormField::Url => app.subscription_form.url.push(value),
            SubscriptionFormField::Refresh | SubscriptionFormField::Ok => {}
        },
        _ => {}
    }
    Ok(())
}

async fn submit_confirm(paths: &Paths, config: &mut AppConfig, app: &mut ConfigApp) -> Result<()> {
    let Some(action) = app.confirm else {
        return Ok(());
    };

    if !app.confirm_yes {
        app.cancel_confirm();
        return Ok(());
    }

    app.confirm = None;
    app.confirm_yes = false;

    match action {
        ConfirmAction::ExitWithoutSaving => {
            app.save_on_exit = false;
            app.should_quit = true;
        }
        ConfirmAction::LoadDefaults => {
            app.status = "Load defaults is not implemented in this preview".into();
        }
        ConfirmAction::SaveRestart => {
            config.save(paths).await?;
            let _ =
                reload_current_runtime(paths, config, app, "Saved and restarted runtime".into())
                    .await;
        }
        ConfirmAction::SaveRestartExit => {
            config.save(paths).await?;
            if reload_current_runtime(paths, config, app, "Saved and restarted runtime".into())
                .await
            {
                app.save_on_exit = false;
                app.should_quit = true;
            }
        }
    }

    Ok(())
}

async fn submit_selection(
    paths: &Paths,
    config: &mut AppConfig,
    app: &mut ConfigApp,
) -> Result<()> {
    match app.page {
        Page::Main => {
            app.selected_proxy_field = 0;
            app.enter_page(
                Page::ProxyConfig,
                format!("Configuring {}", main_proxy_name(config, app.selected_main)),
            );
        }
        Page::Subscription => submit_subscription_selection(paths, config, app).await?,
        Page::AddSubscription => submit_add_subscription_selection(paths, config, app).await?,
        Page::ProxyConfig => submit_proxy_config_item(paths, config, app).await?,
        Page::ProxyGroups => match app.proxy_pane {
            ProxyPane::Groups => {
                app.proxy_pane = ProxyPane::Proxies;
                app.selected_proxy = current_proxy_index(app.current_group()).unwrap_or_default();
                app.status = "Select a proxy, then press Enter".into();
            }
            ProxyPane::Proxies => select_proxy(paths, config, app).await?,
        },
        Page::Mode => submit_mode_selection(paths, config, app).await?,
        Page::Dns => submit_dns_item(paths, config, app).await?,
        Page::Runtime => submit_runtime_item(paths, config, app).await?,
        Page::Chat => {
            app.status = "Chat assistant is a design preview; LLM integration is pending".into();
        }
        Page::Help => {
            if !app.go_back() {
                app.replace_page_with_status(Page::Main, "Back to Main");
            }
        }
    }
    Ok(())
}

async fn submit_subscription_selection(
    paths: &Paths,
    config: &mut AppConfig,
    app: &mut ConfigApp,
) -> Result<()> {
    match selected_subscription_action(config, app) {
        SubscriptionAction::Activate(index) => {
            activate_subscription(paths, config, app, index).await?
        }
        SubscriptionAction::Add => begin_add_subscription(app),
    }
    Ok(())
}

async fn submit_add_subscription_selection(
    paths: &Paths,
    config: &mut AppConfig,
    app: &mut ConfigApp,
) -> Result<()> {
    match app.subscription_form.selected_field() {
        SubscriptionFormField::Name => begin_subscription_name_input(app),
        SubscriptionFormField::Url => begin_subscription_url_input(app),
        SubscriptionFormField::Refresh => app.open_subscription_refresh_dropdown(),
        SubscriptionFormField::Ok => submit_subscription_form(paths, config, app).await?,
    }
    Ok(())
}

async fn submit_dropdown(paths: &Paths, config: &mut AppConfig, app: &mut ConfigApp) -> Result<()> {
    match app.dropdown {
        Some(Dropdown::ProxySubscription) => {
            submit_subscription_dropdown(paths, config, app).await?;
        }
        Some(Dropdown::Mode) => {
            submit_mode_dropdown(paths, config, app).await?;
        }
        Some(Dropdown::SubscriptionRefresh) => {
            submit_subscription_refresh_dropdown(app);
        }
        None => {}
    }
    Ok(())
}

async fn submit_subscription_dropdown(
    paths: &Paths,
    config: &mut AppConfig,
    app: &mut ConfigApp,
) -> Result<()> {
    app.dropdown = None;
    if config.subscriptions.is_empty() {
        app.status = "No subscriptions configured".into();
        return Ok(());
    }

    let index = app
        .selected_dropdown
        .min(config.subscriptions.len().saturating_sub(1));
    app.selected_subscription = index;
    activate_subscription(paths, config, app, index).await
}

async fn submit_mode_dropdown(
    paths: &Paths,
    config: &mut AppConfig,
    app: &mut ConfigApp,
) -> Result<()> {
    app.dropdown = None;
    app.selected_mode = app
        .selected_dropdown
        .min(ModeItem::ALL.len().saturating_sub(1));
    let mode = ModeItem::ALL
        .get(app.selected_mode)
        .copied()
        .unwrap_or(ModeItem::Rule);
    set_runtime_mode(paths, config, app, mode.value()).await?;
    app.status = format!("Mode={} saved", mode.label());
    Ok(())
}

fn submit_subscription_refresh_dropdown(app: &mut ConfigApp) {
    app.dropdown = None;
    app.subscription_form.refresh = subscription_refresh_from_index(app.selected_dropdown);
    app.status = "Refresh saved".into();
}

async fn submit_mode_selection(
    paths: &Paths,
    config: &mut AppConfig,
    app: &mut ConfigApp,
) -> Result<()> {
    let mode = ModeItem::ALL
        .get(app.selected_mode)
        .copied()
        .unwrap_or(ModeItem::Rule);
    set_runtime_mode(paths, config, app, mode.value()).await?;
    app.return_to_previous_or(Page::ProxyConfig, format!("Mode={} saved", mode.label()));
    Ok(())
}

async fn submit_runtime_item(
    _paths: &Paths,
    config: &mut AppConfig,
    app: &mut ConfigApp,
) -> Result<()> {
    match app.selected_runtime_item() {
        RuntimeItem::Service => {
            app.status = "Service install/status is not implemented in this preview".into()
        }
        RuntimeItem::TunPermissions => {
            app.status = "TUN permission install/status is not implemented in this preview".into()
        }
        RuntimeItem::Logs => app.status = "Logs view is not implemented in this preview".into(),
        RuntimeItem::CorePath => begin_core_path_input(config, app),
        RuntimeItem::Controller => begin_controller_input(config, app),
        RuntimeItem::Dns => {
            app.enter_page(Page::Dns, "DNS settings");
        }
        RuntimeItem::Refresh => refresh_runtime(config, app).await,
    }
    Ok(())
}

async fn submit_proxy_config_item(
    paths: &Paths,
    config: &mut AppConfig,
    app: &mut ConfigApp,
) -> Result<()> {
    match app.selected_proxy_config_field() {
        ProxyConfigField::Enabled => {
            if app.selected_main == 0 {
                toggle_system_proxy(paths, config, app).await?;
            } else {
                app.status =
                    "Enable/disable for extra port proxies is not implemented in this preview"
                        .into();
            }
        }
        ProxyConfigField::Subscription => {
            app.open_subscription_dropdown(config);
        }
        ProxyConfigField::Mode => {
            app.open_mode_dropdown(config);
        }
        ProxyConfigField::ProxyGroups => {
            app.proxy_pane = ProxyPane::Groups;
            app.enter_page(Page::ProxyGroups, "Choose a group, then choose a proxy");
        }
        ProxyConfigField::LocalPort => match main_proxy_kind(config, app.selected_main) {
            MainProxyKind::System => begin_mixed_port_input(config, app),
            MainProxyKind::Http => begin_http_port_input(config, app),
            MainProxyKind::Socks => begin_socks_port_input(config, app),
            MainProxyKind::Service => {
                app.status = "Custom listener editing is not implemented in this preview".into();
            }
        },
        ProxyConfigField::OsProxy => toggle_system_proxy(paths, config, app).await?,
        ProxyConfigField::Pac => {
            app.status = "PAC configuration is not implemented in this preview".into()
        }
        ProxyConfigField::Tun => toggle_tun(paths, config, app).await?,
        ProxyConfigField::Dns => {
            app.enter_page(Page::Dns, "DNS settings");
        }
    }
    Ok(())
}

async fn submit_dns_item(paths: &Paths, config: &mut AppConfig, app: &mut ConfigApp) -> Result<()> {
    match app.selected_dns_item() {
        DnsItem::Enabled => toggle_dns(paths, config, app).await?,
        DnsItem::Listen => begin_dns_listen_input(config, app),
        DnsItem::LanDomains => begin_dns_lan_domains_input(config, app),
        DnsItem::LanNameserver => begin_dns_lan_nameserver_input(config, app),
        DnsItem::DirectNameserver => begin_dns_direct_nameserver_input(config, app),
        DnsItem::DirectFollowPolicy => {
            config.dns.direct_nameserver_follow_policy =
                !config.dns.direct_nameserver_follow_policy;
            save_dns_config(paths, config, app, "Direct DNS policy saved").await?;
        }
        DnsItem::Nameserver => begin_dns_nameserver_input(config, app),
        DnsItem::Fallback => begin_dns_fallback_input(config, app),
        DnsItem::FakeIpFilter => begin_dns_fake_ip_filter_input(config, app),
    }
    Ok(())
}

async fn submit_input(paths: &Paths, config: &mut AppConfig, app: &mut ConfigApp) -> Result<()> {
    let value = app.input.trim().to_string();
    match app.input_mode {
        InputMode::Normal => {}
        InputMode::SubscriptionName => {
            app.subscription_form.name = value;
            app.input.clear();
            app.input_mode = InputMode::Normal;
            app.status = "Name saved".into();
        }
        InputMode::SubscriptionUrl => {
            app.subscription_form.url = value;
            app.input.clear();
            app.input_mode = InputMode::Normal;
            app.status = "URL saved".into();
        }
        InputMode::CorePath => {
            config.core_path = if value.is_empty() { None } else { Some(value) };
            config.save(paths).await?;
            app.input.clear();
            app.input_mode = InputMode::Normal;
            app.status = "Core path saved".into();
        }
        InputMode::Controller => {
            if !value.is_empty() {
                config.controller.url = value;
                config.save(paths).await?;
                app.status = "Controller saved".into();
                app.refresh_runtime(config).await;
            }
            app.input.clear();
            app.input_mode = InputMode::Normal;
        }
        InputMode::MixedPort => {
            let Some(port) = parse_port_or_alert(app, &value, "mixed port") else {
                return Ok(());
            };
            config.mixed_port = port;
            config.save(paths).await?;
            reload_current_runtime(paths, config, app, format!("Mixed port={port}")).await;
            app.input.clear();
            app.input_mode = InputMode::Normal;
        }
        InputMode::HttpPort => {
            let Some(port) = parse_optional_port_or_alert(app, &value, "HTTP port") else {
                return Ok(());
            };
            config.proxy_ports.http = port;
            config.save(paths).await?;
            reload_current_runtime(
                paths,
                config,
                app,
                format!("HTTP port={}", optional_port_value(config.proxy_ports.http)),
            )
            .await;
            app.input.clear();
            app.input_mode = InputMode::Normal;
        }
        InputMode::SocksPort => {
            let Some(port) = parse_optional_port_or_alert(app, &value, "SOCKS port") else {
                return Ok(());
            };
            config.proxy_ports.socks = port;
            config.save(paths).await?;
            reload_current_runtime(
                paths,
                config,
                app,
                format!(
                    "SOCKS port={}",
                    optional_port_value(config.proxy_ports.socks)
                ),
            )
            .await;
            app.input.clear();
            app.input_mode = InputMode::Normal;
        }
        InputMode::DnsListen => {
            if !value.is_empty() {
                config.dns.listen = value;
                save_dns_config(paths, config, app, "DNS listen saved").await?;
            }
            app.input.clear();
            app.input_mode = InputMode::Normal;
        }
        InputMode::DnsLanDomains => {
            config.dns.lan_domains = split_list(&value);
            save_dns_config(paths, config, app, "LAN domains saved").await?;
            app.input.clear();
            app.input_mode = InputMode::Normal;
        }
        InputMode::DnsLanNameserver => {
            config.dns.lan_nameserver = split_list(&value);
            save_dns_config(paths, config, app, "LAN DNS saved").await?;
            app.input.clear();
            app.input_mode = InputMode::Normal;
        }
        InputMode::DnsDirectNameserver => {
            config.dns.direct_nameserver = split_list(&value);
            save_dns_config(paths, config, app, "Direct DNS saved").await?;
            app.input.clear();
            app.input_mode = InputMode::Normal;
        }
        InputMode::DnsNameserver => {
            config.dns.nameserver = split_list(&value);
            save_dns_config(paths, config, app, "Default DNS saved").await?;
            app.input.clear();
            app.input_mode = InputMode::Normal;
        }
        InputMode::DnsFallback => {
            config.dns.fallback = split_list(&value);
            save_dns_config(paths, config, app, "Fallback DNS saved").await?;
            app.input.clear();
            app.input_mode = InputMode::Normal;
        }
        InputMode::DnsFakeIpFilter => {
            config.dns.fake_ip_filter = split_list(&value);
            save_dns_config(paths, config, app, "Fake-IP filter saved").await?;
            app.input.clear();
            app.input_mode = InputMode::Normal;
        }
    }
    Ok(())
}

fn begin_add_subscription(app: &mut ConfigApp) {
    app.input.clear();
    if app.subscription_form.name.trim().is_empty() && app.subscription_form.url.trim().is_empty() {
        app.subscription_form.clear();
    }
    app.enter_page(Page::AddSubscription, "Fill subscription form");
}

fn begin_subscription_name_input(app: &mut ConfigApp) {
    app.input_mode = InputMode::SubscriptionName;
    app.input.clone_from(&app.subscription_form.name);
    app.status = "Edit subscription name".into();
}

fn begin_subscription_url_input(app: &mut ConfigApp) {
    app.input_mode = InputMode::SubscriptionUrl;
    app.input.clone_from(&app.subscription_form.url);
    app.status = "Edit subscription URL".into();
}

fn begin_core_path_input(config: &AppConfig, app: &mut ConfigApp) {
    app.input_mode = InputMode::CorePath;
    app.input = config.core_path.clone().unwrap_or_default();
    app.status = "Edit mihomo core path".into();
}

fn begin_controller_input(config: &AppConfig, app: &mut ConfigApp) {
    app.input_mode = InputMode::Controller;
    app.input.clone_from(&config.controller.url);
    app.status = "Edit controller URL".into();
}

fn begin_mixed_port_input(config: &AppConfig, app: &mut ConfigApp) {
    app.input_mode = InputMode::MixedPort;
    app.input = config.mixed_port.to_string();
    app.status = "Edit mixed port".into();
}

fn begin_http_port_input(config: &AppConfig, app: &mut ConfigApp) {
    app.input_mode = InputMode::HttpPort;
    app.input = optional_port_value(config.proxy_ports.http);
    app.status = "Edit HTTP port; empty/off disables it".into();
}

fn begin_socks_port_input(config: &AppConfig, app: &mut ConfigApp) {
    app.input_mode = InputMode::SocksPort;
    app.input = optional_port_value(config.proxy_ports.socks);
    app.status = "Edit SOCKS port; empty/off disables it".into();
}

fn begin_dns_listen_input(config: &AppConfig, app: &mut ConfigApp) {
    app.page = Page::Dns;
    app.input_mode = InputMode::DnsListen;
    app.input.clone_from(&config.dns.listen);
    app.status = "Edit DNS listen address".into();
}

fn begin_dns_lan_domains_input(config: &AppConfig, app: &mut ConfigApp) {
    app.page = Page::Dns;
    app.input_mode = InputMode::DnsLanDomains;
    app.input = join_list(&config.dns.lan_domains);
    app.status = "Edit LAN domain suffixes".into();
}

fn begin_dns_lan_nameserver_input(config: &AppConfig, app: &mut ConfigApp) {
    app.page = Page::Dns;
    app.input_mode = InputMode::DnsLanNameserver;
    app.input = join_list(&config.dns.lan_nameserver);
    app.status = "Edit LAN DNS servers".into();
}

fn begin_dns_direct_nameserver_input(config: &AppConfig, app: &mut ConfigApp) {
    app.page = Page::Dns;
    app.input_mode = InputMode::DnsDirectNameserver;
    app.input = join_list(&config.dns.direct_nameserver);
    app.status = "Edit DIRECT DNS servers".into();
}

fn begin_dns_nameserver_input(config: &AppConfig, app: &mut ConfigApp) {
    app.page = Page::Dns;
    app.input_mode = InputMode::DnsNameserver;
    app.input = join_list(&config.dns.nameserver);
    app.status = "Edit default DNS servers".into();
}

fn begin_dns_fallback_input(config: &AppConfig, app: &mut ConfigApp) {
    app.page = Page::Dns;
    app.input_mode = InputMode::DnsFallback;
    app.input = join_list(&config.dns.fallback);
    app.status = "Edit fallback DNS servers".into();
}

fn begin_dns_fake_ip_filter_input(config: &AppConfig, app: &mut ConfigApp) {
    app.page = Page::Dns;
    app.input_mode = InputMode::DnsFakeIpFilter;
    app.input = join_list(&config.dns.fake_ip_filter);
    app.status = "Edit fake-IP filter".into();
}

async fn refresh_runtime(config: &AppConfig, app: &mut ConfigApp) {
    app.refresh_runtime(config).await;
    app.status = "Runtime refreshed".into();
}

async fn toggle_system_proxy(
    paths: &Paths,
    config: &mut AppConfig,
    app: &mut ConfigApp,
) -> Result<()> {
    config.system_proxy.enabled = !config.system_proxy.enabled;
    config.save(paths).await?;
    app.status = format!("System proxy={}", on_off(config.system_proxy.enabled));
    Ok(())
}

async fn toggle_tun(paths: &Paths, config: &mut AppConfig, app: &mut ConfigApp) -> Result<()> {
    config.tun.enable = !config.tun.enable;
    config.save(paths).await?;
    app.status = format!("TUN={}", on_off(config.tun.enable));
    Ok(())
}

async fn toggle_dns(paths: &Paths, config: &mut AppConfig, app: &mut ConfigApp) -> Result<()> {
    config.dns.enable = !config.dns.enable;
    save_dns_config(
        paths,
        config,
        app,
        &format!("DNS={}", on_off(config.dns.enable)),
    )
    .await?;
    Ok(())
}

async fn save_dns_config(
    paths: &Paths,
    config: &AppConfig,
    app: &mut ConfigApp,
    status: &str,
) -> Result<()> {
    config.save(paths).await?;
    app.status = format!("{status}; daemon will apply config");
    Ok(())
}

async fn submit_subscription_form(
    paths: &Paths,
    config: &mut AppConfig,
    app: &mut ConfigApp,
) -> Result<()> {
    let name = app.subscription_form.name.trim().to_string();
    let url = app.subscription_form.url.trim().to_string();
    let refresh = app.subscription_form.refresh;

    if name.is_empty() {
        app.subscription_form.selected = 0;
        app.alert("Required Field", "Name is required.");
        return Ok(());
    }
    if url.is_empty() {
        app.subscription_form.selected = 1;
        app.alert("Required Field", "URL is required.");
        return Ok(());
    }
    if config.subscriptions.iter().any(|sub| sub.name == name) {
        app.subscription_form.selected = 0;
        app.alert("Duplicate Name", format!("{name} already exists."));
        return Ok(());
    }

    add_subscription(paths, config, app, name, url, refresh).await
}

async fn add_subscription(
    paths: &Paths,
    config: &mut AppConfig,
    app: &mut ConfigApp,
    name: String,
    url: String,
    refresh: SubscriptionRefresh,
) -> Result<()> {
    if url.is_empty() {
        app.status = "URL is empty".into();
        return Ok(());
    }

    config.subscriptions.push(Subscription {
        name: name.clone(),
        url,
        refresh,
        updated_at: None,
    });
    config.active_profile = Some(name.clone());
    app.selected_subscription = config.subscriptions.len() - 1;
    app.input.clear();
    app.input_mode = InputMode::Normal;
    app.subscription_form.clear();

    match subscription::update(paths, config, app.selected_subscription).await {
        Ok(path) => {
            app.status = format!(
                "Added {}; downloaded {}",
                display_name(&name),
                path.display()
            )
        }
        Err(err) => app.status = format!("Added {}; download failed: {err}", display_name(&name)),
    }
    config.save(paths).await?;
    load_selected_profile(paths, config, app).await;
    if app.page == Page::AddSubscription {
        let status = app.status.clone();
        app.return_to_previous_or(Page::Subscription, status);
    }
    Ok(())
}

async fn activate_subscription(
    paths: &Paths,
    config: &mut AppConfig,
    app: &mut ConfigApp,
    index: usize,
) -> Result<()> {
    let Some(sub) = config.subscriptions.get(index) else {
        app.status = "No subscription selected".into();
        return Ok(());
    };
    config.active_profile = Some(sub.name.clone());
    config.save(paths).await?;
    app.status = format!("Active subscription={}", display_name(&sub.name));
    load_selected_profile(paths, config, app).await;
    Ok(())
}

async fn load_selected_profile(paths: &Paths, config: &AppConfig, app: &mut ConfigApp) {
    let Some(sub) = config.subscriptions.get(app.selected_subscription) else {
        return;
    };
    let client = MihomoClient::new(&config.controller);
    let path = match runtime_profile::write_active_config(paths, config, sub).await {
        Ok(path) => path,
        Err(err) => {
            app.status = format!("Saved active profile; runtime config failed: {err}");
            return;
        }
    };
    match client.reload_config(&path).await {
        Ok(()) => {
            app.status = match client.set_mixed_port(config.mixed_port).await {
                Ok(()) => format!("Loaded profile {}", display_name(&sub.name)),
                Err(err) => format!("Loaded profile; mixed-port patch failed: {err}"),
            };
            app.refresh_runtime(config).await;
        }
        Err(err) => app.status = format!("Saved active profile; runtime load failed: {err}"),
    }
}

async fn reload_current_runtime(
    paths: &Paths,
    config: &AppConfig,
    app: &mut ConfigApp,
    success: String,
) -> bool {
    let client = MihomoClient::new(&config.controller);
    let path = match runtime_profile::write_current_config(paths, config).await {
        Ok(path) => path,
        Err(err) => {
            app.status = format!("{success} saved; runtime config failed: {err}");
            return false;
        }
    };

    match client.reload_config(&path).await {
        Ok(()) => {
            app.status = success;
            app.refresh_runtime(config).await;
            true
        }
        Err(err) => {
            app.status = format!("{success} saved; runtime reload failed: {err}");
            false
        }
    }
}

async fn set_runtime_mode(
    paths: &Paths,
    config: &mut AppConfig,
    app: &mut ConfigApp,
    mode: &str,
) -> Result<()> {
    config.runtime_mode = mode.to_string();
    config.save(paths).await?;
    let client = MihomoClient::new(&config.controller);
    match client.set_mode(mode).await {
        Ok(()) => {
            app.status = format!("Mode={mode} saved");
            app.refresh_runtime(config).await;
        }
        Err(err) => app.status = format!("Mode saved; runtime patch failed: {err}"),
    }
    Ok(())
}

async fn select_proxy(paths: &Paths, config: &mut AppConfig, app: &mut ConfigApp) -> Result<()> {
    let Some(group) = app.current_group() else {
        app.status = "No proxy group selected".into();
        return Ok(());
    };
    let Some(proxy) = group.all.get(app.selected_proxy) else {
        app.status = "No proxy selected".into();
        return Ok(());
    };

    let group_name = group.name.clone();
    let proxy_name = proxy.clone();
    config
        .proxy_selections
        .insert(group_name.clone(), proxy_name.clone());
    config.save(paths).await?;
    let client = MihomoClient::new(&config.controller);
    match client.select_proxy(&group_name, &proxy_name).await {
        Ok(()) => {
            app.status = format!(
                "{} -> {} saved",
                display_name(&group_name),
                display_name(&proxy_name)
            );
            app.refresh_runtime(config).await;
        }
        Err(err) => app.status = format!("Proxy selection saved; runtime patch failed: {err}"),
    }
    Ok(())
}

fn draw(frame: &mut Frame, paths: &Paths, config: &AppConfig, app: &ConfigApp) {
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
    draw_body(frame, paths, config, app, layout[1]);
    draw_footer(frame, config, app, layout[2], separator_column);

    if app.is_input() {
        draw_input(frame, app);
    }
    if app.dropdown.is_some() {
        draw_dropdown(frame, config, app);
    }
    if app.confirm.is_some() {
        draw_confirm(frame, app);
    }
    if app.alert.is_some() {
        draw_alert(frame, app);
    }
}

fn draw_header(frame: &mut Frame, app: &ConfigApp, area: Rect, separator_column: u16) {
    frame.render_widget(Clear, area);
    let mut tabs = Vec::new();
    for page in Page::SECTION_ROOTS {
        tabs.push(Span::raw(" "));
        let active = page == app.section;
        tabs.push(Span::styled(
            if active {
                format!("[ {} ]", page.title())
            } else {
                format!(" {} ", page.title())
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

fn draw_body(frame: &mut Frame, paths: &Paths, config: &AppConfig, app: &ConfigApp, area: Rect) {
    frame.render_widget(Clear, area);
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(66),
            Constraint::Length(1),
            Constraint::Percentage(34),
        ])
        .split(area);
    draw_settings(frame, paths, config, app, columns[0]);
    draw_vertical_separator(frame, columns[1]);
    draw_help(frame, paths, config, app, columns[2]);
}

fn draw_settings(
    frame: &mut Frame,
    paths: &Paths,
    config: &AppConfig,
    app: &ConfigApp,
    area: Rect,
) {
    if app.page == Page::Main {
        draw_main_settings(frame, paths, config, app, area);
        return;
    }

    let rows = setting_rows(paths, config, app);
    let selected = app.selected_index();
    let content_width = area.width.saturating_sub(2) as usize;
    let mut lines = vec![
        with_padding(Line::from(Span::styled(
            page_summary(app.page),
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
        let text = fit_width(
            &format!("{prefix}{:<LABEL_WIDTH$} {}", row.label, row.value),
            content_width,
        );
        let style = row_style(row, index == selected);
        lines.push(with_padding(Line::from(Span::styled(text, style))));
    }

    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

fn draw_main_settings(
    frame: &mut Frame,
    paths: &Paths,
    config: &AppConfig,
    app: &ConfigApp,
    area: Rect,
) {
    let rows = main_proxy_rows(config);
    let selected = app.selected_main;
    let content_width = area.width.saturating_sub(2) as usize;
    let mut lines = vec![
        with_padding(Line::from(Span::styled(
            page_summary(Page::Main),
            Style::default().fg(Color::Gray),
        ))),
        Line::from(Span::styled(
            horizontal_line(area.width),
            Style::default().fg(Color::DarkGray),
        )),
    ];

    let runtime_line = proxy_runtime_line(app);
    for (index, row) in rows.iter().enumerate() {
        let selected_row = index == selected;
        let first = fit_width(
            &format!("> {:<14} {:<7} {}", row.name, row.status, row.listen),
            content_width,
        );
        lines.push(with_padding(Line::from(Span::styled(
            first,
            if selected_row {
                selected_style()
            } else {
                Style::default().fg(Color::Cyan)
            },
        ))));

        let second = fit_width(
            &format!(
                "  mode={:<8} sub={} {}",
                row.mode, row.subscription, row.features
            ),
            content_width,
        );
        lines.push(with_padding(Line::from(Span::styled(
            second,
            Style::default().fg(Color::Gray),
        ))));

        let third = fit_width(
            &format!("  {}  workdir={}", runtime_line, paths.config_dir.display()),
            content_width,
        );
        lines.push(with_padding(Line::from(Span::styled(
            third,
            Style::default().fg(Color::DarkGray),
        ))));
    }

    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

fn draw_help(frame: &mut Frame, paths: &Paths, config: &AppConfig, app: &ConfigApp, area: Rect) {
    const HOTKEY_ROWS: u16 = 6;
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(6), Constraint::Length(HOTKEY_ROWS)])
        .split(area);
    let rows = setting_rows(paths, config, app);
    let row = rows.get(app.selected_index());
    let mut lines = vec![
        Line::from(Span::styled(
            "Details",
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
    ];

    if app.page == Page::Main {
        let row = main_proxy_row(config, app.selected_main);
        lines.extend([
            Line::from(Span::styled(row.name, Style::default().fg(Color::Cyan))),
            Line::from(""),
            Line::from(format!("Listen: {}", row.listen)),
            Line::from(format!("Mode: {}", row.mode)),
            Line::from(format!("Subscription: {}", row.subscription)),
            Line::from(row.features),
            Line::from(proxy_runtime_line(app)),
        ]);
    } else if let Some(row) = row {
        lines.extend([
            Line::from(Span::styled(
                row.label.clone(),
                Style::default().fg(Color::Cyan),
            )),
            Line::from(""),
            Line::from(row.help.clone()),
        ]);
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

fn setting_rows(_paths: &Paths, config: &AppConfig, app: &ConfigApp) -> Vec<SettingRow> {
    match app.page {
        Page::Main => main_proxy_rows(config)
            .into_iter()
            .map(|row| SettingRow {
                label: row.name,
                value: row.listen,
                help: row.features,
                kind: RowKind::Submenu(Page::ProxyConfig),
            })
            .collect(),
        Page::Subscription => subscription_rows(config),
        Page::AddSubscription => add_subscription_rows(app),
        Page::Runtime => runtime_rows(config, app),
        Page::Chat => chat_rows(config, app),
        Page::Help => help_rows(),
        Page::ProxyConfig => proxy_config_rows(config, app),
        Page::ProxyGroups => proxy_group_rows(app),
        Page::Mode => mode_rows(config, app),
        Page::Dns => dns_rows(config),
    }
}

fn subscription_rows(config: &AppConfig) -> Vec<SettingRow> {
    let mut rows = config
        .subscriptions
        .iter()
        .map(|subscription| {
            let active = config.active_profile.as_ref() == Some(&subscription.name);
            SettingRow {
                label: if active {
                    format!("* {}", display_name(&subscription.name))
                } else {
                    display_name(&subscription.name)
                },
                value: format!(
                    "{}  updated={}",
                    subscription_refresh_label(subscription.refresh),
                    subscription.updated_at.as_deref().unwrap_or("-")
                ),
                help: format!(
                    "Enter activates this subscription. URL: {}",
                    subscription.url
                ),
                kind: RowKind::Action(ActionKind::RefreshRuntime),
            }
        })
        .collect::<Vec<_>>();

    rows.push(SettingRow {
        label: "Add Subscription".into(),
        value: "page".into(),
        help: "Open a child page for name, URL, refresh interval, and OK.".into(),
        kind: RowKind::Submenu(Page::AddSubscription),
    });
    rows
}

fn add_subscription_rows(app: &ConfigApp) -> Vec<SettingRow> {
    vec![
        SettingRow {
            label: "Name".into(),
            value: required_value(&app.subscription_form.name),
            help: "Subscription display name.".into(),
            kind: RowKind::Input(InputMode::SubscriptionName),
        },
        SettingRow {
            label: "URL".into(),
            value: required_value(&app.subscription_form.url),
            help: "Subscription profile URL.".into(),
            kind: RowKind::Input(InputMode::SubscriptionUrl),
        },
        SettingRow {
            label: "Refresh".into(),
            value: subscription_refresh_label(app.subscription_form.refresh).into(),
            help: "Refresh cadence for this subscription.".into(),
            kind: RowKind::Choice(ChoiceAction::SubscriptionRefresh),
        },
        SettingRow {
            label: "OK".into(),
            value: "save".into(),
            help: "Save this subscription and return to the subscription list.".into(),
            kind: RowKind::Action(ActionKind::SaveSubscription),
        },
    ]
}

fn proxy_config_rows(config: &AppConfig, app: &ConfigApp) -> Vec<SettingRow> {
    app.proxy_config_fields()
        .iter()
        .map(|field| {
            let kind = match field {
                ProxyConfigField::Enabled => {
                    if app.selected_main == 0 {
                        RowKind::Toggle(ToggleAction::SystemProxy)
                    } else {
                        RowKind::Info
                    }
                }
                ProxyConfigField::Subscription => RowKind::Choice(ChoiceAction::Subscription),
                ProxyConfigField::Mode => RowKind::Choice(ChoiceAction::Mode),
                ProxyConfigField::ProxyGroups => RowKind::Submenu(Page::ProxyGroups),
                ProxyConfigField::LocalPort => match main_proxy_kind(config, app.selected_main) {
                    MainProxyKind::System => RowKind::Input(InputMode::MixedPort),
                    MainProxyKind::Http => RowKind::Input(InputMode::HttpPort),
                    MainProxyKind::Socks => RowKind::Input(InputMode::SocksPort),
                    MainProxyKind::Service => RowKind::Info,
                },
                ProxyConfigField::OsProxy => RowKind::Toggle(ToggleAction::SystemProxy),
                ProxyConfigField::Pac => RowKind::Info,
                ProxyConfigField::Tun => RowKind::Toggle(ToggleAction::Tun),
                ProxyConfigField::Dns => RowKind::Submenu(Page::Dns),
            };
            SettingRow {
                label: field.label().into(),
                value: proxy_config_field_value(config, app, *field),
                help: proxy_config_field_help(*field).into(),
                kind,
            }
        })
        .collect()
}

fn proxy_group_rows(app: &ConfigApp) -> Vec<SettingRow> {
    match app.proxy_pane {
        ProxyPane::Groups => {
            if app.runtime.groups.is_empty() {
                return vec![SettingRow {
                    label: "No Groups".into(),
                    value: "offline".into(),
                    help: "Start the daemon or refresh runtime before choosing a proxy.".into(),
                    kind: RowKind::Info,
                }];
            }
            app.runtime
                .groups
                .iter()
                .map(|group| SettingRow {
                    label: display_name(&group.name),
                    value: format!("{} -> {}", group.kind, display_name(&group.now)),
                    help: "Enter opens the proxy list for this group.".into(),
                    kind: RowKind::Action(ActionKind::SelectProxyGroup),
                })
                .collect()
        }
        ProxyPane::Proxies => {
            let Some(group) = app.current_group() else {
                return vec![SettingRow {
                    label: "No Group".into(),
                    value: "-".into(),
                    help: "Go back to groups and choose one first.".into(),
                    kind: RowKind::Info,
                }];
            };
            group
                .all
                .iter()
                .map(|proxy| SettingRow {
                    label: if proxy == &group.now {
                        format!("* {}", display_name(proxy))
                    } else {
                        display_name(proxy)
                    },
                    value: if proxy == &group.now {
                        "current".into()
                    } else {
                        "select".into()
                    },
                    help: format!("Enter saves this proxy for {}.", display_name(&group.name)),
                    kind: RowKind::Action(ActionKind::SelectProxy),
                })
                .collect()
        }
    }
}

fn runtime_rows(config: &AppConfig, app: &ConfigApp) -> Vec<SettingRow> {
    RuntimeItem::ALL
        .iter()
        .map(|item| {
            let kind = match item {
                RuntimeItem::Service => RowKind::Action(ActionKind::Service),
                RuntimeItem::TunPermissions => RowKind::Action(ActionKind::TunPermissions),
                RuntimeItem::Logs => RowKind::Action(ActionKind::Logs),
                RuntimeItem::CorePath => RowKind::Input(InputMode::CorePath),
                RuntimeItem::Controller => RowKind::Input(InputMode::Controller),
                RuntimeItem::Dns => RowKind::Submenu(Page::Dns),
                RuntimeItem::Refresh => RowKind::Action(ActionKind::RefreshRuntime),
            };
            SettingRow {
                label: item.label().into(),
                value: runtime_item_value(config, app, *item),
                help: runtime_item_help(*item).into(),
                kind,
            }
        })
        .collect()
}

fn dns_rows(config: &AppConfig) -> Vec<SettingRow> {
    DnsItem::ALL
        .iter()
        .map(|item| {
            let kind = match item {
                DnsItem::Enabled => RowKind::Toggle(ToggleAction::Dns),
                DnsItem::Listen => RowKind::Input(InputMode::DnsListen),
                DnsItem::LanDomains => RowKind::Input(InputMode::DnsLanDomains),
                DnsItem::LanNameserver => RowKind::Input(InputMode::DnsLanNameserver),
                DnsItem::DirectNameserver => RowKind::Input(InputMode::DnsDirectNameserver),
                DnsItem::DirectFollowPolicy => RowKind::Toggle(ToggleAction::DnsDirectFollowPolicy),
                DnsItem::Nameserver => RowKind::Input(InputMode::DnsNameserver),
                DnsItem::Fallback => RowKind::Input(InputMode::DnsFallback),
                DnsItem::FakeIpFilter => RowKind::Input(InputMode::DnsFakeIpFilter),
            };
            SettingRow {
                label: item.label().into(),
                value: dns_item_value(config, *item),
                help: dns_item_help(*item).into(),
                kind,
            }
        })
        .collect()
}

fn chat_rows(config: &AppConfig, app: &ConfigApp) -> Vec<SettingRow> {
    vec![
        SettingRow {
            label: "Prompt Goal".into(),
            value: "configure proxy".into(),
            help: "Future AI-assisted configuration entry point.".into(),
            kind: RowKind::Info,
        },
        SettingRow {
            label: "Spec".into(),
            value: format!(
                "system:{} tun:{} mode:{}",
                config.mixed_port,
                on_off(config.tun.enable),
                config.runtime_mode
            ),
            help: format!(
                "LLM should propose a structured patch. Runtime: {}",
                app.runtime.error.as_deref().unwrap_or("online")
            ),
            kind: RowKind::Info,
        },
    ]
}

fn help_rows() -> Vec<SettingRow> {
    vec![
        SettingRow {
            label: "Navigation".into(),
            value: "Esc stack".into(),
            help: "Esc returns through saved locations; root Esc asks before exit.".into(),
            kind: RowKind::Action(ActionKind::HelpBack),
        },
        SettingRow {
            label: "Inputs".into(),
            value: "choice/input/number".into(),
            help: "Every editable value uses a visible popup or child page.".into(),
            kind: RowKind::Info,
        },
    ]
}

fn mode_rows(config: &AppConfig, app: &ConfigApp) -> Vec<SettingRow> {
    ModeItem::ALL
        .iter()
        .map(|mode| SettingRow {
            label: mode.label().into(),
            value: if mode.value() == config.runtime_mode {
                "saved".into()
            } else if Some(mode.value()) == app.runtime.mode.as_deref() {
                "runtime".into()
            } else {
                String::new()
            },
            help: match mode {
                ModeItem::Rule => "Use subscription rules to decide the outbound proxy.",
                ModeItem::Global => "Send traffic through one selected proxy group/server.",
                ModeItem::Direct => "Bypass proxy rules and send traffic directly.",
            }
            .into(),
            kind: RowKind::Choice(ChoiceAction::Mode),
        })
        .collect()
}

fn page_summary(page: Page) -> &'static str {
    match page {
        Page::Main => "runtime overview / proxy entrypoints",
        Page::Subscription => "profiles / refresh / usage",
        Page::AddSubscription => "new profile / URL / refresh",
        Page::Runtime => "service / controller / logs",
        Page::Chat => "assistant / structured patch",
        Page::Help => "navigation / input model",
        Page::ProxyConfig => "proxy settings / listener / TUN",
        Page::ProxyGroups => "proxy group / server selection",
        Page::Mode => "runtime mode",
        Page::Dns => "resolve strategy / nameservers",
    }
}

fn row_style(row: &SettingRow, selected: bool) -> Style {
    if selected {
        selected_style()
    } else if matches!(row.kind, RowKind::Submenu(_)) {
        Style::default().fg(Color::Cyan)
    } else if matches!(row.kind, RowKind::Action(_)) {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default().fg(Color::White)
    }
}

fn required_value(value: &str) -> String {
    if value.trim().is_empty() {
        "(required)".into()
    } else {
        value.to_string()
    }
}

const fn runtime_item_help(item: RuntimeItem) -> &'static str {
    match item {
        RuntimeItem::Service => "Install or inspect the system service. Implementation is pending.",
        RuntimeItem::TunPermissions => "Install or inspect platform permissions required for TUN.",
        RuntimeItem::Logs => "Open runtime logs. Implementation is pending.",
        RuntimeItem::CorePath => "Path to mihomo core. Empty uses automatic discovery.",
        RuntimeItem::Controller => "Mihomo external controller URL used by clashtui.",
        RuntimeItem::Dns => "Open DNS settings shared by the runtime.",
        RuntimeItem::Refresh => "Refresh runtime information from mihomo.",
    }
}

fn draw_main_page(
    frame: &mut Frame,
    paths: &Paths,
    config: &AppConfig,
    app: &ConfigApp,
    area: Rect,
) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(6), Constraint::Min(8)])
        .split(area);
    draw_main_dashboard(frame, paths, config, app, rows[0]);

    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(62), Constraint::Percentage(38)])
        .split(rows[1]);
    draw_main_proxies(frame, config, app, columns[0]);
    draw_main_proxy_detail(frame, paths, config, app, columns[1]);
}

fn draw_main_dashboard(
    frame: &mut Frame,
    paths: &Paths,
    config: &AppConfig,
    app: &ConfigApp,
    area: Rect,
) {
    let mihomo = app.runtime.version.as_deref().unwrap_or("offline");
    let runtime = app.runtime.error.as_deref().unwrap_or("online");
    let core_path = core::resolve_core_path(config)
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "not found".into());
    let lines = vec![
        Line::from(vec![
            Span::styled("Daemon ", Style::default().fg(Color::Gray)),
            Span::raw("config-driven"),
            Span::raw("   "),
            Span::styled("mihomo ", Style::default().fg(Color::Gray)),
            Span::raw(mihomo.to_string()),
            Span::raw("   "),
            Span::styled("runtime ", Style::default().fg(Color::Gray)),
            Span::raw(runtime.to_string()),
        ]),
        Line::from(format!("Workdir: {}", paths.config_dir.display())),
        Line::from(format!(
            "Core: {core_path}  Controller: {}",
            config.controller.url
        )),
        Line::from(format!(
            "System Proxy={}  TUN={}  DNS={}  Mode desired={} runtime={}",
            on_off(config.system_proxy.enabled),
            on_off(config.tun.enable),
            on_off(config.dns.enable),
            config.runtime_mode,
            app.runtime.mode.as_deref().unwrap_or("unknown")
        )),
    ];
    frame.render_widget(bios_panel("Runtime", lines), area);
}

fn draw_main_proxies(frame: &mut Frame, config: &AppConfig, app: &ConfigApp, area: Rect) {
    let runtime_line = proxy_runtime_line(app);
    let items = main_proxy_rows(config)
        .into_iter()
        .enumerate()
        .map(|(index, row)| {
            let style = if index == app.selected_main {
                form_choice_style(true)
            } else {
                Style::default()
            };
            let feature_summary = match main_proxy_kind(config, index) {
                MainProxyKind::System => format!(
                    "os={} tun={}",
                    on_off(config.system_proxy.enabled),
                    on_off(config.tun.enable)
                ),
                MainProxyKind::Http | MainProxyKind::Socks | MainProxyKind::Service => {
                    row.features.clone()
                }
            };
            ListItem::new(vec![
                Line::from(Span::styled(
                    format!(
                        "{:<3} {:<14} {:<6} {}",
                        row.status, row.name, row.kind, row.listen
                    ),
                    style,
                )),
                Line::from(Span::styled(
                    format!(
                        "     mode={:<8} sub={} {}",
                        row.mode, row.subscription, feature_summary
                    ),
                    Style::default().fg(Color::Gray),
                )),
                Line::from(Span::styled(
                    format!("     {runtime_line}"),
                    Style::default().fg(Color::DarkGray),
                )),
            ])
        })
        .collect::<Vec<_>>();

    let mut state = ListState::default();
    state.select(Some(app.selected_main));
    let list = List::new(items)
        .block(focused_block("Proxies", true))
        .highlight_symbol("▶ ")
        .highlight_style(form_choice_style(true));
    frame.render_stateful_widget(list, area, &mut state);
}

fn draw_main_proxy_detail(
    frame: &mut Frame,
    _paths: &Paths,
    config: &AppConfig,
    app: &ConfigApp,
    area: Rect,
) {
    let row = main_proxy_row(config, app.selected_main);
    let mut lines = vec![
        Line::from(Span::styled(
            row.name,
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(format!("Listen: {}", row.listen)),
    ];
    match main_proxy_kind(config, app.selected_main) {
        MainProxyKind::System => lines.extend([
            Line::from(format!(
                "os={} pac=off",
                on_off(config.system_proxy.enabled),
            )),
            Line::from(format!(
                "tun={} dns={}",
                on_off(config.tun.enable),
                on_off(config.dns.enable)
            )),
        ]),
        _ => lines.extend([
            Line::from("os/pac/tun: system only"),
            Line::from(format!("listener: {}", row.listen)),
        ]),
    }
    lines.extend([
        Line::from("Enter: proxy config"),
        Line::from("F10 restart"),
        Line::from("F11 restart+exit"),
    ]);
    frame.render_widget(bios_panel("Selected Proxy Config", lines), area);
}

fn draw_subscriptions_page(
    frame: &mut Frame,
    paths: &Paths,
    config: &AppConfig,
    app: &ConfigApp,
    area: Rect,
) {
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(52), Constraint::Percentage(48)])
        .split(area);
    draw_subscriptions(frame, config, app, columns[0]);
    draw_subscription_help(frame, paths, config, app, columns[1]);
}

fn draw_subscriptions(frame: &mut Frame, config: &AppConfig, app: &ConfigApp, area: Rect) {
    let mut items = config
        .subscriptions
        .iter()
        .enumerate()
        .map(|(index, sub)| {
            let active = if config.active_profile.as_ref() == Some(&sub.name) {
                "*"
            } else {
                " "
            };
            let selected = if index == app.selected_subscription {
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            ListItem::new(vec![
                Line::from(Span::styled(
                    format!("{active} {}", display_name(&sub.name)),
                    selected,
                )),
                Line::from(Span::styled(
                    format!(
                        "  refresh={} updated={}  {}",
                        subscription_refresh_label(sub.refresh),
                        sub.updated_at.as_deref().unwrap_or("-"),
                        sub.url
                    ),
                    Style::default().fg(Color::Gray),
                )),
            ])
        })
        .collect::<Vec<_>>();

    let action_offset = config.subscriptions.len();
    let style = if action_offset == app.selected_subscription {
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };
    items.push(ListItem::new(vec![
        Line::from(Span::styled("Add Subscription", style)),
        Line::from(Span::styled(
            "  name, URL, refresh interval",
            Style::default().fg(Color::Gray),
        )),
    ]));

    let mut state = ListState::default();
    state.select(Some(app.selected_subscription));
    let list = List::new(items)
        .block(focused_block("Subscriptions", true))
        .highlight_symbol(">> ")
        .highlight_style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        );
    frame.render_stateful_widget(list, area, &mut state);
}

fn draw_subscription_help(
    frame: &mut Frame,
    paths: &Paths,
    config: &AppConfig,
    app: &ConfigApp,
    area: Rect,
) {
    let mut lines = vec![
        Line::from("Choose a subscription or Add Subscription."),
        Line::from("Enter activates the selected row."),
        Line::from(""),
        Line::from(format!(
            "Active: {}",
            config
                .active_profile
                .as_deref()
                .map_or_else(|| "-".into(), display_name)
        )),
    ];

    if let Some(sub) = config.subscriptions.get(app.selected_subscription) {
        lines.extend([
            Line::from(""),
            Line::from(format!("Selected: {}", display_name(&sub.name))),
            Line::from(format!(
                "Updated: {}",
                sub.updated_at.as_deref().unwrap_or("-")
            )),
            Line::from(format!(
                "Refresh: {}",
                subscription_refresh_label(sub.refresh)
            )),
            Line::from(format!(
                "Profile: {}",
                subscription::profile_path(paths, sub).display()
            )),
            Line::from(""),
            Line::from(sub.url.clone()),
        ]);
    } else {
        let action = selected_subscription_action(config, app);
        lines.extend([
            Line::from(""),
            Line::from(match action {
                SubscriptionAction::Add => "Action: Add Subscription",
                SubscriptionAction::Activate(_) => "Action: Activate Subscription",
            }),
            Line::from(match action {
                SubscriptionAction::Add => "The form collects name, URL, and refresh interval.",
                SubscriptionAction::Activate(_) => "Sets the selected subscription active.",
            }),
        ]);
    }

    frame.render_widget(panel("Help", lines), area);
}

fn draw_proxies_page(frame: &mut Frame, config: &AppConfig, app: &ConfigApp, area: Rect) {
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(34),
            Constraint::Percentage(36),
            Constraint::Percentage(30),
        ])
        .split(area);
    draw_groups(frame, app, columns[0]);
    draw_proxy_options(frame, app, columns[1]);
    draw_proxy_help(frame, config, app, columns[2]);
}

fn draw_groups(frame: &mut Frame, app: &ConfigApp, area: Rect) {
    let items = if app.runtime.groups.is_empty() {
        vec![ListItem::new("No groups. Start daemon, then refresh.")]
    } else {
        app.runtime
            .groups
            .iter()
            .enumerate()
            .map(|(index, group)| {
                let selected = if index == app.selected_group {
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                ListItem::new(vec![
                    Line::from(Span::styled(display_name(&group.name), selected)),
                    Line::from(Span::styled(
                        format!("  {} -> {}", group.kind, display_name(&group.now)),
                        Style::default().fg(Color::Gray),
                    )),
                ])
            })
            .collect()
    };

    let mut state = ListState::default();
    if !app.runtime.groups.is_empty() {
        state.select(Some(app.selected_group));
    }
    let list = List::new(items)
        .block(focused_block(
            "Proxy Groups",
            app.page == Page::ProxyGroups && app.proxy_pane == ProxyPane::Groups,
        ))
        .highlight_symbol(">> ")
        .highlight_style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        );
    frame.render_stateful_widget(list, area, &mut state);
}

fn draw_proxy_options(frame: &mut Frame, app: &ConfigApp, area: Rect) {
    let Some(group) = app.current_group() else {
        frame.render_widget(
            panel("Group Proxies", vec![Line::from("No group selected")]),
            area,
        );
        return;
    };
    let items = if group.all.is_empty() {
        vec![ListItem::new("No selectable proxies.")]
    } else {
        group
            .all
            .iter()
            .enumerate()
            .map(|(index, proxy)| {
                let current = if proxy == &group.now { "*" } else { " " };
                let style = if index == app.selected_proxy {
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                ListItem::new(Line::from(Span::styled(
                    format!("{current} {}", display_name(proxy)),
                    style,
                )))
            })
            .collect()
    };

    let mut state = ListState::default();
    if !group.all.is_empty() {
        state.select(Some(app.selected_proxy));
    }
    let list = List::new(items)
        .block(focused_block(
            "Group Proxies",
            app.page == Page::ProxyGroups && app.proxy_pane == ProxyPane::Proxies,
        ))
        .highlight_symbol(">> ")
        .highlight_style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        );
    frame.render_stateful_widget(list, area, &mut state);
}

fn draw_proxy_help(frame: &mut Frame, config: &AppConfig, app: &ConfigApp, area: Rect) {
    let mut lines = vec![
        Line::from(format!("Pane: {}", app.proxy_pane.title())),
        Line::from("Left/Right: switch group/proxy pane"),
        Line::from("Enter on group: choose proxies"),
        Line::from("Enter on proxy: save selection"),
        Line::from("r: refresh from mihomo /proxies"),
        Line::from(""),
    ];

    if let Some(err) = &app.runtime.error {
        lines.push(Line::from(format!("Runtime: {err}")));
        lines.push(Line::from(""));
    }

    if let Some(group) = app.current_group() {
        lines.extend([
            Line::from(format!("Group: {}", display_name(&group.name))),
            Line::from(format!("Type: {}", group.kind)),
            Line::from(format!("Current: {}", display_name(&group.now))),
            Line::from(format!("Members: {}", group.all.len())),
            Line::from(format!(
                "Saved: {}",
                config
                    .proxy_selections
                    .get(&group.name)
                    .map_or_else(|| "-".into(), |value| display_name(value))
            )),
        ]);
    }

    frame.render_widget(panel("Help", lines), area);
}

fn draw_proxy_config_page(frame: &mut Frame, config: &AppConfig, app: &ConfigApp, area: Rect) {
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(52), Constraint::Percentage(48)])
        .split(area);
    draw_proxy_config_menu(frame, config, app, columns[0]);
    draw_proxy_config_help(frame, config, app, columns[1]);
}

fn draw_proxy_config_menu(frame: &mut Frame, config: &AppConfig, app: &ConfigApp, area: Rect) {
    let fields = app.proxy_config_fields();
    let items = fields
        .iter()
        .map(|field| {
            ListItem::new(Line::from(vec![
                Span::raw(format!("{:<20}", field.label())),
                Span::styled(
                    proxy_config_field_value(config, app, *field),
                    Style::default().fg(Color::Gray),
                ),
            ]))
        })
        .collect::<Vec<_>>();

    let mut state = ListState::default();
    state.select(Some(app.selected_proxy_field));
    let title = format!(
        "{} Configuration",
        main_proxy_name(config, app.selected_main)
    );
    let list = List::new(items)
        .block(focused_block(&title, true))
        .highlight_symbol("▶ ")
        .highlight_style(form_choice_style(true));
    frame.render_stateful_widget(list, area, &mut state);
}

fn draw_proxy_config_help(frame: &mut Frame, config: &AppConfig, app: &ConfigApp, area: Rect) {
    let field = app.selected_proxy_config_field();
    let row = main_proxy_row(config, app.selected_main);
    let mut lines = vec![
        Line::from(Span::styled(
            field.label(),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(proxy_config_field_help(field)),
        Line::from(""),
        Line::from(format!("Proxy: {}", row.name)),
        Line::from(format!("Kind: {}", row.kind)),
        Line::from(format!("Listen: {}", row.listen)),
        Line::from(format!("Mode: {}", row.mode)),
        Line::from(format!("Subscription: {}", row.subscription)),
        Line::from(""),
        Line::from("Enter edits or opens the selected item."),
        Line::from("Esc returns to Main."),
    ];
    if app.selected_main == 0 {
        lines.push(Line::from("TUN is only configured on System Proxy."));
    }
    frame.render_widget(bios_panel("Item Help", lines), area);
}

fn draw_mode_page(frame: &mut Frame, config: &AppConfig, app: &ConfigApp, area: Rect) {
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(44), Constraint::Percentage(56)])
        .split(area);
    draw_mode_menu(frame, app, columns[0]);
    draw_mode_help(frame, config, app, columns[1]);
}

fn draw_mode_menu(frame: &mut Frame, app: &ConfigApp, area: Rect) {
    let items = ModeItem::ALL
        .iter()
        .map(|mode| ListItem::new(Line::from(mode.label())))
        .collect::<Vec<_>>();

    let mut state = ListState::default();
    state.select(Some(app.selected_mode));
    let list = List::new(items)
        .block(focused_block("Mode", true))
        .highlight_symbol(">> ")
        .highlight_style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        );
    frame.render_stateful_widget(list, area, &mut state);
}

fn draw_mode_help(frame: &mut Frame, config: &AppConfig, app: &ConfigApp, area: Rect) {
    let mode = ModeItem::ALL
        .get(app.selected_mode)
        .copied()
        .unwrap_or(ModeItem::Rule);
    let lines = vec![
        Line::from(Span::styled(
            mode.label(),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(match mode {
            ModeItem::Rule => "Use subscription rules to decide the outbound proxy.",
            ModeItem::Global => "Send traffic through one selected proxy group/server.",
            ModeItem::Direct => "Bypass proxy rules and send traffic directly.",
        }),
        Line::from(""),
        Line::from(format!("Saved mode: {}", config.runtime_mode)),
        Line::from(format!(
            "Runtime mode: {}",
            app.runtime.mode.as_deref().unwrap_or("unknown")
        )),
        Line::from(""),
        Line::from("Enter saves this mode and returns to the Proxy page."),
        Line::from("Esc returns without changing the mode."),
    ];
    frame.render_widget(bios_panel("Mode Help", lines), area);
}

fn draw_runtime_page(frame: &mut Frame, config: &AppConfig, app: &ConfigApp, area: Rect) {
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
        .split(area);
    draw_runtime_menu(frame, config, app, columns[0]);
    draw_runtime_help(frame, config, app, columns[1]);
}

fn draw_runtime_menu(frame: &mut Frame, config: &AppConfig, app: &ConfigApp, area: Rect) {
    let items = RuntimeItem::ALL
        .iter()
        .map(|item| {
            ListItem::new(Line::from(vec![
                Span::raw(format!("{:<18}", item.label())),
                Span::styled(
                    runtime_item_value(config, app, *item),
                    Style::default().fg(Color::Gray),
                ),
            ]))
        })
        .collect::<Vec<_>>();

    let mut state = ListState::default();
    state.select(Some(app.selected_runtime));
    let list = List::new(items)
        .block(focused_block("Runtime Menu", true))
        .highlight_symbol(">> ")
        .highlight_style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        );
    frame.render_stateful_widget(list, area, &mut state);
}

fn draw_runtime_help(frame: &mut Frame, config: &AppConfig, app: &ConfigApp, area: Rect) {
    let lines = vec![
        Line::from("Runtime contains non-proxy operational settings."),
        Line::from("Proxy mode and server selection live on each Proxy config page."),
        Line::from(""),
        Line::from(format!(
            "Core: {}",
            config.core_path.as_deref().unwrap_or("auto")
        )),
        Line::from(format!("Controller: {}", config.controller.url)),
        Line::from(format!(
            "mihomo: {}",
            app.runtime.version.as_deref().unwrap_or("offline")
        )),
        Line::from(format!(
            "runtime: {}",
            app.runtime.error.as_deref().unwrap_or("online")
        )),
        Line::from(""),
        Line::from("Service and TUN permission installers are planned."),
        Line::from("Enter opens editable items where available."),
    ];
    frame.render_widget(bios_panel("Runtime Help", lines), area);
}

fn draw_chat_page(frame: &mut Frame, config: &AppConfig, app: &ConfigApp, area: Rect) {
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(58), Constraint::Percentage(42)])
        .split(area);
    let chat_lines = vec![
        Line::from(Span::styled(
            "AI Configuration Assistant",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from("Preview page. LLM integration is not wired yet."),
        Line::from("The intended flow is:"),
        Line::from("1. Explain what you want in plain language."),
        Line::from("2. Review the proposed config diff."),
        Line::from("3. Confirm Save & Restart, Save Draft, or Cancel."),
        Line::from(""),
        Line::from("Examples:"),
        Line::from("- Use system proxy but keep TUN off."),
        Line::from("- Create a SOCKS5 LAN proxy on 7081 using HK."),
        Line::from("- Explain why TUN is unavailable on macOS."),
    ];
    frame.render_widget(bios_panel("Chat", chat_lines), columns[0]);

    let spec_lines = vec![
        Line::from(Span::styled(
            "LLM Spec Preview",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from("version: 1"),
        Line::from("proxies:"),
        Line::from("  - id: system"),
        Line::from("    kind: system"),
        Line::from(format!("    mixed_port: {}", config.mixed_port)),
        Line::from(format!("    os_proxy: {}", config.system_proxy.enabled)),
        Line::from(format!("    tun: {}", config.tun.enable)),
        Line::from(format!("    mode: {}", config.runtime_mode)),
        Line::from(format!(
            "    subscription: {}",
            config.active_profile.as_deref().unwrap_or("none")
        )),
        Line::from(""),
        Line::from(format!(
            "runtime: {}",
            app.runtime.error.as_deref().unwrap_or("online")
        )),
    ];
    frame.render_widget(
        bios_panel("Validated Config Surface", spec_lines),
        columns[1],
    );
}

fn draw_dns_page(frame: &mut Frame, config: &AppConfig, app: &ConfigApp, area: Rect) {
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(48), Constraint::Percentage(52)])
        .split(area);
    draw_dns_menu(frame, config, app, columns[0]);
    draw_dns_help(frame, config, app, columns[1]);
}

fn draw_dns_menu(frame: &mut Frame, config: &AppConfig, app: &ConfigApp, area: Rect) {
    let items = DnsItem::ALL
        .iter()
        .map(|item| {
            ListItem::new(Line::from(vec![
                Span::raw(format!("{:<24}", item.label())),
                Span::styled(
                    dns_item_value(config, *item),
                    Style::default().fg(Color::Gray),
                ),
            ]))
        })
        .collect::<Vec<_>>();

    let mut state = ListState::default();
    state.select(Some(app.selected_dns));
    let list = List::new(items)
        .block(focused_block("DNS Menu", true))
        .highlight_symbol(">> ")
        .highlight_style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        );
    frame.render_stateful_widget(list, area, &mut state);
}

fn draw_dns_help(frame: &mut Frame, config: &AppConfig, app: &ConfigApp, area: Rect) {
    let item = app.selected_dns_item();
    let policy = dns::effective_nameserver_policy(&config.dns);
    let lines = vec![
        Line::from(Span::styled(
            item.label(),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(dns_item_help(item)),
        Line::from(""),
        Line::from("List values are comma separated."),
        Line::from("Use system for the OS resolver, or IP/DoH/DoT servers."),
        Line::from(""),
        Line::from(format!("Effective policy entries: {}", policy.len())),
        Line::from(format!(
            "LAN domains: {}",
            compact_list(&config.dns.lan_domains)
        )),
        Line::from(format!(
            "LAN DNS: {}",
            compact_list(&config.dns.lan_nameserver)
        )),
        Line::from(format!(
            "Effective fake-IP filter: {}",
            dns::effective_fake_ip_filter(&config.dns).len()
        )),
        Line::from(""),
        Line::from("Typical LAN DNS: system, 192.168.0.1"),
    ];
    frame.render_widget(panel("DNS Help", lines), area);
}

fn draw_help_page(frame: &mut Frame, area: Rect) {
    let lines = vec![
        Line::from(Span::styled(
            "Navigation",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from("Tab / Shift+Tab: switch top tabs"),
        Line::from("Up / Down: move in the current menu"),
        Line::from("Left / Right: switch tabs; on Proxy Groups, switch panes"),
        Line::from("Enter / Space: choose the highlighted item"),
        Line::from("F1: open Help"),
        Line::from("Esc: go back; on Main it asks before exiting without saving"),
        Line::from(""),
        Line::from(Span::styled(
            "Function Actions",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from("F9: load setup defaults, with Yes/No confirmation."),
        Line::from("F10: save and restart runtime, with Yes/No confirmation."),
        Line::from("F11: save, restart runtime, and exit, with Yes/No confirmation."),
        Line::from(""),
        Line::from(Span::styled(
            "Basic Flow",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from("1. Main: inspect runtime status and choose a proxy to configure."),
        Line::from("2. Proxy: set subscription, mode, listener, OS proxy, TUN, and DNS."),
        Line::from("3. Subscription: choose a profile or Add Subscription."),
        Line::from("4. Runtime: configure service, permissions, logs, core path, controller."),
        Line::from("5. Chat: preview AI-assisted config through a structured spec."),
    ];
    frame.render_widget(panel("Help", lines), area);
}

fn draw_footer(
    frame: &mut Frame,
    config: &AppConfig,
    app: &ConfigApp,
    area: Rect,
    separator_column: u16,
) {
    frame.render_widget(Clear, area);
    let lines = vec![
        Line::from(connected_horizontal_line(
            area.width,
            separator_column,
            BOTTOM_JOINT,
        )),
        status_bar_line(config, app, area.width),
    ];
    frame.render_widget(Paragraph::new(lines), area);
}

fn draw_input(frame: &mut Frame, app: &ConfigApp) {
    let area = fixed_rect(58, 10, frame.area());
    frame.render_widget(Clear, area);
    let mut body = vec![
        popup_title_line(app.input_mode.title(), area.width.saturating_sub(2)),
        Line::from(""),
    ];
    body.extend(input_box_lines(
        &app.input,
        area.width.saturating_sub(6) as usize,
    ));
    body.extend([
        Line::from(""),
        Line::from(Span::styled(
            if is_number_input(app.input_mode) {
                "Up/Down change | Enter OK | Esc cancel"
            } else {
                "Type value | Enter OK | Esc cancel"
            },
            Style::default().fg(Color::Gray),
        )),
    ]);

    frame.render_widget(dialog_panel(body), area);
}

fn draw_subscription_form(frame: &mut Frame, app: &ConfigApp) {
    let area = centered_rect(64, 42, frame.area());
    frame.render_widget(Clear, area);
    let form = &app.subscription_form;
    let lines = vec![
        Line::from("Tab moves between fields. Esc cancels."),
        Line::from(""),
        subscription_form_line(
            "Name",
            &form.name,
            form.selected_field() == SubscriptionFormField::Name,
        ),
        subscription_form_line(
            "URL",
            &form.url,
            form.selected_field() == SubscriptionFormField::Url,
        ),
        Line::from(vec![
            Span::raw(format!("{:<10}", "Refresh")),
            refresh_option_span(
                "1 day",
                form.refresh == SubscriptionRefresh::Daily,
                form.selected_field() == SubscriptionFormField::Refresh,
            ),
            Span::raw("  "),
            refresh_option_span(
                "1 week",
                form.refresh == SubscriptionRefresh::Weekly,
                form.selected_field() == SubscriptionFormField::Refresh,
            ),
            Span::raw("  "),
            refresh_option_span(
                "disabled",
                form.refresh == SubscriptionRefresh::Disabled,
                form.selected_field() == SubscriptionFormField::Refresh,
            ),
        ]),
        Line::from(""),
        Line::from(vec![Span::styled(
            " OK ",
            form_choice_style(form.selected_field() == SubscriptionFormField::Ok),
        )]),
    ];
    frame.render_widget(panel("Add Subscription", lines), area);
}

fn subscription_form_line<'a>(label: &'a str, value: &'a str, selected: bool) -> Line<'a> {
    Line::from(vec![
        Span::raw(format!("{label:<10}")),
        Span::styled(
            if value.is_empty() { " " } else { value },
            form_choice_style(selected),
        ),
    ])
}

fn refresh_option_span<'a>(label: &'a str, active: bool, selected_field: bool) -> Span<'a> {
    let style = if active {
        if selected_field {
            form_choice_style(true)
        } else {
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        }
    } else if selected_field {
        Style::default().fg(Color::White)
    } else {
        Style::default().fg(Color::Gray)
    };
    Span::styled(format!("[{label}]"), style)
}

fn draw_dropdown(frame: &mut Frame, config: &AppConfig, app: &ConfigApp) {
    let Some(dropdown) = app.dropdown else {
        return;
    };
    let options = dropdown_options(dropdown, config);
    let height = (options.len() as u16 + 4).clamp(6, 12);
    let area = fixed_rect(46, height, frame.area());
    frame.render_widget(Clear, area);

    let selected = app.selected_dropdown.min(options.len().saturating_sub(1));
    let mut lines = vec![
        popup_title_line(dropdown_title(dropdown), area.width.saturating_sub(2)),
        Line::from(""),
    ];
    for (index, option) in options.iter().enumerate() {
        let style = if index == selected {
            selected_style()
        } else if dropdown == Dropdown::ProxySubscription
            && subscription_dropdown_active(config, index)
        {
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::White)
        };
        lines.push(Line::from(Span::styled(option.as_str(), style)));
    }
    frame.render_widget(dialog_panel(lines), area);
}

fn dropdown_title(dropdown: Dropdown) -> &'static str {
    match dropdown {
        Dropdown::ProxySubscription => "Subscription",
        Dropdown::Mode => "Mode",
        Dropdown::SubscriptionRefresh => "Refresh",
    }
}

fn dropdown_options(dropdown: Dropdown, config: &AppConfig) -> Vec<String> {
    match dropdown {
        Dropdown::ProxySubscription => {
            if config.subscriptions.is_empty() {
                vec!["No subscriptions configured".into()]
            } else {
                config
                    .subscriptions
                    .iter()
                    .map(|subscription| display_name(&subscription.name))
                    .collect()
            }
        }
        Dropdown::Mode => ModeItem::ALL
            .iter()
            .map(|mode| mode.label().to_string())
            .collect(),
        Dropdown::SubscriptionRefresh => SUBSCRIPTION_REFRESH_OPTIONS
            .iter()
            .map(|refresh| subscription_refresh_label(*refresh).to_string())
            .collect(),
    }
}

fn draw_subscription_dropdown(frame: &mut Frame, config: &AppConfig, app: &ConfigApp) {
    let area = centered_rect(52, 40, frame.area());
    frame.render_widget(Clear, area);

    let count = subscription_dropdown_count(config);
    let selected = app.selected_dropdown.min(count.saturating_sub(1));
    let max_visible = 5usize;
    let start = if count > max_visible {
        let mut start = selected.saturating_sub(max_visible - 1);
        if start + max_visible > count {
            start = count - max_visible;
        }
        start
    } else {
        0
    };
    let end = (start + max_visible).min(count);

    let mut lines = vec![
        Line::from("Choose subscription for this proxy."),
        Line::from(""),
    ];
    if start > 0 {
        lines.push(Line::from(Span::styled(
            "  ...",
            Style::default().fg(Color::Gray),
        )));
    }
    for index in start..end {
        let active = subscription_dropdown_active(config, index);
        let selected_row = index == selected;
        let label = subscription_dropdown_label(config, index);
        let marker = if active { "*" } else { " " };
        let style = if selected_row {
            form_choice_style(true)
        } else if active {
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::White)
        };
        lines.push(Line::from(Span::styled(
            format!("> {marker} {label}"),
            style,
        )));
    }
    if end < count {
        lines.push(Line::from(Span::styled(
            "  ...",
            Style::default().fg(Color::Gray),
        )));
    }
    lines.extend([
        Line::from(""),
        Line::from(Span::styled(
            "Enter select | Esc cancel",
            Style::default().fg(Color::Gray),
        )),
    ]);

    frame.render_widget(bios_panel("Subscription", lines), area);
}

fn subscription_dropdown_label(config: &AppConfig, index: usize) -> String {
    config
        .subscriptions
        .get(index)
        .map(|subscription| display_name(&subscription.name))
        .unwrap_or_else(|| "No subscriptions configured".into())
}

fn subscription_dropdown_active(config: &AppConfig, index: usize) -> bool {
    config
        .subscriptions
        .get(index)
        .is_some_and(|subscription| config.active_profile.as_ref() == Some(&subscription.name))
}

fn draw_confirm(frame: &mut Frame, app: &ConfigApp) {
    let Some(action) = app.confirm else {
        return;
    };

    let area = fixed_rect(42, 6, frame.area());
    frame.render_widget(Clear, area);
    let lines = vec![
        popup_title_line(action.title(), area.width.saturating_sub(2)),
        Line::from(""),
        Line::from(vec![
            Span::raw("          "),
            Span::styled(" No ", button_style(!app.confirm_yes)),
            Span::raw("              "),
            Span::styled(" Yes ", button_style(app.confirm_yes)),
        ]),
    ];
    frame.render_widget(dialog_panel(lines), area);
}

fn draw_alert(frame: &mut Frame, app: &ConfigApp) {
    let Some(alert) = &app.alert else {
        return;
    };

    let area = fixed_rect(46, 7, frame.area());
    frame.render_widget(Clear, area);
    let lines = vec![
        popup_title_line(&alert.title, area.width.saturating_sub(2)),
        Line::from(""),
        Line::from(alert.message.clone()).alignment(Alignment::Center),
        Line::from(""),
        Line::from(Span::styled(" OK ", button_style(true))).alignment(Alignment::Center),
    ];
    frame.render_widget(dialog_panel(lines), area);
}

fn confirm_choice_style(selected: bool) -> Style {
    button_style(selected)
}

fn form_choice_style(selected: bool) -> Style {
    if selected {
        selected_style()
    } else {
        Style::default().fg(Color::Gray)
    }
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

fn input_box_lines(value: &str, width: usize) -> Vec<Line<'_>> {
    let inner_width = width.saturating_sub(4).max(8);
    let value = fit_width(value, inner_width);
    vec![
        Line::from(format!("  ┌{}┐", "─".repeat(inner_width))),
        Line::from(format!("  │{value:<inner_width$}│")),
        Line::from(format!("  └{}┘", "─".repeat(inner_width))),
    ]
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

fn with_horizontal_padding<'a>(lines: Vec<Line<'a>>) -> Vec<Line<'a>> {
    lines.into_iter().map(with_padding).collect()
}

fn with_padding<'a>(mut line: Line<'a>) -> Line<'a> {
    line.spans.insert(0, Span::raw(" "));
    line.spans.push(Span::raw(" "));
    line
}

fn status_bar_line(config: &AppConfig, app: &ConfigApp, width: u16) -> Line<'static> {
    let width = width as usize;
    let right = format!(" │ {} │ {} ", config_state(app), service_state(config, app));
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

fn breadcrumb(app: &ConfigApp) -> String {
    app.history
        .iter()
        .map(|location| location.page.title())
        .chain(std::iter::once(app.page.title()))
        .collect::<Vec<_>>()
        .join(" / ")
}

fn config_state(app: &ConfigApp) -> &'static str {
    if !app.subscription_form.name.trim().is_empty() || !app.subscription_form.url.trim().is_empty()
    {
        "cfg draft"
    } else {
        "cfg saved"
    }
}

fn service_state(config: &AppConfig, app: &ConfigApp) -> &'static str {
    if app.runtime.error.is_none() || config.system_proxy.enabled || config.tun.enable {
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

#[derive(Clone, Copy, PartialEq, Eq)]
enum SubscriptionAction {
    Activate(usize),
    Add,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum MainProxyKind {
    System,
    Http,
    Socks,
    Service,
}

struct MainProxyRow {
    status: String,
    name: String,
    kind: String,
    listen: String,
    mode: String,
    subscription: String,
    features: String,
}

fn subscription_menu_count(config: &AppConfig) -> usize {
    config.subscriptions.len() + 1
}

fn subscription_dropdown_count(config: &AppConfig) -> usize {
    config.subscriptions.len().max(1)
}

fn selected_subscription_action(config: &AppConfig, app: &ConfigApp) -> SubscriptionAction {
    let index = app.selected_subscription;
    if index < config.subscriptions.len() {
        return SubscriptionAction::Activate(index);
    }

    SubscriptionAction::Add
}

fn runtime_mode_index(config: &AppConfig) -> usize {
    ModeItem::ALL
        .iter()
        .position(|mode| mode.value() == config.runtime_mode)
        .unwrap_or_default()
}

fn main_proxy_count(config: &AppConfig) -> usize {
    1 + usize::from(config.proxy_ports.http.is_some())
        + usize::from(config.proxy_ports.socks.is_some())
        + config.proxy_ports.services.len()
}

fn main_proxy_rows(config: &AppConfig) -> Vec<MainProxyRow> {
    (0..main_proxy_count(config))
        .map(|index| main_proxy_row(config, index))
        .collect()
}

fn main_proxy_row(config: &AppConfig, index: usize) -> MainProxyRow {
    let subscription = config
        .active_profile
        .as_deref()
        .map_or_else(|| "-".into(), display_name);
    match main_proxy_kind(config, index) {
        MainProxyKind::System => MainProxyRow {
            status: if config.system_proxy.enabled || config.tun.enable {
                "OK".into()
            } else {
                "Off".into()
            },
            name: "System Proxy".into(),
            kind: "System".into(),
            listen: format!("{}:{}", config.proxy_host, config.mixed_port),
            mode: config.runtime_mode.clone(),
            subscription,
            features: format!(
                "os={} pac=off tun={} dns={}",
                on_off(config.system_proxy.enabled),
                on_off(config.tun.enable),
                on_off(config.dns.enable)
            ),
        },
        MainProxyKind::Http => MainProxyRow {
            status: "OK".into(),
            name: "HTTP Port".into(),
            kind: "HTTP".into(),
            listen: format!(
                "{}:{}",
                config.proxy_host,
                config.proxy_ports.http.unwrap_or_default()
            ),
            mode: config.runtime_mode.clone(),
            subscription,
            features: format!("allow-lan={}", on_off(config.proxy_ports.allow_lan)),
        },
        MainProxyKind::Socks => MainProxyRow {
            status: "OK".into(),
            name: "SOCKS Port".into(),
            kind: "SOCKS5".into(),
            listen: format!(
                "{}:{}",
                config.proxy_host,
                config.proxy_ports.socks.unwrap_or_default()
            ),
            mode: config.runtime_mode.clone(),
            subscription,
            features: format!("allow-lan={}", on_off(config.proxy_ports.allow_lan)),
        },
        MainProxyKind::Service => {
            let service_index = service_index_for_main_proxy(config, index).unwrap_or_default();
            let service = config.proxy_ports.services.get(service_index);
            let listen = service
                .map(|service| {
                    let host = if service.listen.trim().is_empty() {
                        "127.0.0.1"
                    } else {
                        service.listen.trim()
                    };
                    format!("{host}:{}", service.port)
                })
                .unwrap_or_else(|| "-".into());
            MainProxyRow {
                status: service
                    .map(|service| if service.enabled { "OK" } else { "Off" })
                    .unwrap_or("Off")
                    .into(),
                name: service
                    .map(|service| {
                        if service.name.trim().is_empty() {
                            format!("{} Port", service.kind)
                        } else {
                            service.name.clone()
                        }
                    })
                    .unwrap_or_else(|| "Port Proxy".into()),
                kind: service.map_or_else(|| "Port".into(), |service| service.kind.clone()),
                listen,
                mode: config.runtime_mode.clone(),
                subscription,
                features: service
                    .map(|service| format!("udp={}", on_off(service.udp)))
                    .unwrap_or_else(|| "custom".into()),
            }
        }
    }
}

fn main_proxy_kind(config: &AppConfig, index: usize) -> MainProxyKind {
    if index == 0 {
        return MainProxyKind::System;
    }
    let mut cursor = 1;
    if config.proxy_ports.http.is_some() {
        if index == cursor {
            return MainProxyKind::Http;
        }
        cursor += 1;
    }
    if config.proxy_ports.socks.is_some() && index == cursor {
        return MainProxyKind::Socks;
    }
    MainProxyKind::Service
}

fn service_index_for_main_proxy(config: &AppConfig, index: usize) -> Option<usize> {
    let mut cursor = 1;
    if config.proxy_ports.http.is_some() {
        cursor += 1;
    }
    if config.proxy_ports.socks.is_some() {
        cursor += 1;
    }
    index.checked_sub(cursor)
}

fn main_proxy_name(config: &AppConfig, index: usize) -> String {
    main_proxy_row(config, index).name
}

fn proxy_config_field_value(
    config: &AppConfig,
    app: &ConfigApp,
    field: ProxyConfigField,
) -> String {
    match field {
        ProxyConfigField::Enabled => {
            if app.selected_main == 0 {
                on_off(config.system_proxy.enabled || config.tun.enable).into()
            } else {
                main_proxy_row(config, app.selected_main).status
            }
        }
        ProxyConfigField::Subscription => config
            .active_profile
            .as_deref()
            .map_or_else(|| "-".into(), display_name),
        ProxyConfigField::Mode => format!(
            "{} / {}",
            config.runtime_mode,
            app.runtime.mode.as_deref().unwrap_or("unknown")
        ),
        ProxyConfigField::ProxyGroups => app
            .current_group()
            .map(|group| {
                format!(
                    "{} -> {}",
                    display_name(&group.name),
                    display_name(&group.now)
                )
            })
            .unwrap_or_else(|| "start daemon and refresh".into()),
        ProxyConfigField::LocalPort => main_proxy_row(config, app.selected_main).listen,
        ProxyConfigField::OsProxy => on_off(config.system_proxy.enabled).into(),
        ProxyConfigField::Pac => "off".into(),
        ProxyConfigField::Tun => on_off(config.tun.enable).into(),
        ProxyConfigField::Dns => on_off(config.dns.enable).into(),
    }
}

const fn proxy_config_field_help(field: ProxyConfigField) -> &'static str {
    match field {
        ProxyConfigField::Enabled => "System Proxy enabled means OS proxy or TUN is active.",
        ProxyConfigField::Subscription => "Choose the source from an inline dropdown.",
        ProxyConfigField::Mode => "Choose rule, global, or direct behavior.",
        ProxyConfigField::ProxyGroups => {
            "Choose the group and concrete server for the current mode."
        }
        ProxyConfigField::LocalPort => "Edit the local listener port for this proxy.",
        ProxyConfigField::OsProxy => "Point the operating system proxy settings to the mixed port.",
        ProxyConfigField::Pac => "PAC support is planned; it will live on System Proxy.",
        ProxyConfigField::Tun => {
            "TUN is transparent system traffic and belongs only to System Proxy."
        }
        ProxyConfigField::Dns => "Open DNS settings used by mihomo runtime.",
    }
}

fn runtime_item_value(config: &AppConfig, app: &ConfigApp, item: RuntimeItem) -> String {
    match item {
        RuntimeItem::Service => "not installed".into(),
        RuntimeItem::TunPermissions => {
            if config.tun.enable {
                "required when TUN is on".into()
            } else {
                "not required".into()
            }
        }
        RuntimeItem::Logs => "open planned".into(),
        RuntimeItem::CorePath => config.core_path.as_deref().unwrap_or("auto").to_string(),
        RuntimeItem::Controller => config.controller.url.clone(),
        RuntimeItem::Dns => on_off(config.dns.enable).into(),
        RuntimeItem::Refresh => app.runtime.error.as_deref().unwrap_or("online").to_string(),
    }
}

fn dns_item_value(config: &AppConfig, item: DnsItem) -> String {
    match item {
        DnsItem::Enabled => on_off(config.dns.enable).into(),
        DnsItem::Listen => config.dns.listen.clone(),
        DnsItem::LanDomains => compact_list(&config.dns.lan_domains),
        DnsItem::LanNameserver => compact_list(&config.dns.lan_nameserver),
        DnsItem::DirectNameserver => compact_list(&config.dns.direct_nameserver),
        DnsItem::DirectFollowPolicy => on_off(config.dns.direct_nameserver_follow_policy).into(),
        DnsItem::Nameserver => compact_list(&config.dns.nameserver),
        DnsItem::Fallback => compact_list(&config.dns.fallback),
        DnsItem::FakeIpFilter => compact_list(&config.dns.fake_ip_filter),
    }
}

const fn dns_item_help(item: DnsItem) -> &'static str {
    match item {
        DnsItem::Enabled => "Enable or disable mihomo built-in DNS.",
        DnsItem::Listen => "Local mihomo DNS listen address. Default avoids Clash Verge 1053.",
        DnsItem::LanDomains => "Domain patterns that should resolve with LAN DNS and skip fake IP.",
        DnsItem::LanNameserver => {
            "DNS servers for LAN domains. Use system or router DNS addresses."
        }
        DnsItem::DirectNameserver => "DNS servers used when traffic exits through DIRECT.",
        DnsItem::DirectFollowPolicy => "When on, DIRECT DNS also follows nameserver-policy.",
        DnsItem::Nameserver => "Default DNS servers for normal domain resolution.",
        DnsItem::Fallback => "Backup DNS servers for polluted or overseas results.",
        DnsItem::FakeIpFilter => "Domain patterns that should get real IPs instead of fake IPs.",
    }
}

fn compact_list(values: &[String]) -> String {
    if values.is_empty() {
        return "-".into();
    }
    let joined = values.join(", ");
    const MAX: usize = 44;
    if joined.chars().count() <= MAX {
        joined
    } else {
        let mut value = joined.chars().take(MAX).collect::<String>();
        value.push_str("...");
        value
    }
}

fn proxy_runtime_line(app: &ConfigApp) -> String {
    format!(
        "n {} t {} {}",
        traffic_speed_summary(app.runtime.traffic.as_ref()),
        traffic_total_summary(app.runtime.traffic.as_ref()),
        ip_info_summary(app)
    )
}

fn traffic_speed_summary(traffic: Option<&TrafficState>) -> String {
    traffic.map_or_else(
        || "-/-".into(),
        |traffic| {
            format!(
                "{}/{}",
                format_bytes_short(traffic.upload_speed),
                format_bytes_short(traffic.download_speed)
            )
        },
    )
}

fn traffic_total_summary(traffic: Option<&TrafficState>) -> String {
    traffic.map_or_else(
        || "-".into(),
        |traffic| format_bytes_short(traffic.download_total),
    )
}

fn ip_info_summary(app: &ConfigApp) -> String {
    if let Some(info) = &app.runtime.ip_info {
        let mut location = [info.country.as_deref()]
            .into_iter()
            .flatten()
            .filter(|value| !value.is_empty())
            .collect::<Vec<_>>()
            .join("/");
        if location.is_empty() {
            location = "-".into();
        }
        format!("{} {}", info.ip, location)
    } else if app.runtime.ip_info_error.is_some() {
        "unavailable".into()
    } else {
        "pending".into()
    }
}

fn traffic_from_value(value: &serde_json::Value) -> Option<TrafficState> {
    let download_total = value
        .get("downloadTotal")
        .and_then(serde_json::Value::as_u64)?;
    let (upload_speed, download_speed) = value
        .get("connections")
        .and_then(serde_json::Value::as_array)
        .map(|connections| {
            connections
                .iter()
                .fold((0_u64, 0_u64), |(up, down), connection| {
                    (
                        up + connection
                            .get("upload")
                            .and_then(serde_json::Value::as_u64)
                            .unwrap_or_default(),
                        down + connection
                            .get("download")
                            .and_then(serde_json::Value::as_u64)
                            .unwrap_or_default(),
                    )
                })
        })
        .unwrap_or_default();
    Some(TrafficState {
        download_total,
        upload_speed,
        download_speed,
    })
}

async fn fetch_ip_info() -> Result<IpInfoState> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()
        .context("failed to build ipinfo client")?;
    let value: serde_json::Value = client
        .get("https://ipinfo.io/json")
        .send()
        .await
        .context("failed to query ipinfo.io")?
        .json()
        .await
        .context("failed to decode ipinfo.io response")?;
    let ip = value
        .get("ip")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("-")
        .to_string();
    Ok(IpInfoState {
        ip,
        country: value
            .get("country")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
    })
}

fn format_bytes_short(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "K", "M", "G", "T"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes}{}", UNITS[unit])
    } else if value >= 10.0 {
        format!("{value:.0}{}", UNITS[unit])
    } else {
        format!("{value:.1}{}", UNITS[unit])
    }
}

const fn subscription_refresh_label(refresh: SubscriptionRefresh) -> &'static str {
    match refresh {
        SubscriptionRefresh::Disabled => "disabled",
        SubscriptionRefresh::Daily => "1 day",
        SubscriptionRefresh::Weekly => "1 week",
    }
}

const SUBSCRIPTION_REFRESH_OPTIONS: [SubscriptionRefresh; 3] = [
    SubscriptionRefresh::Daily,
    SubscriptionRefresh::Weekly,
    SubscriptionRefresh::Disabled,
];

fn subscription_refresh_index(refresh: SubscriptionRefresh) -> usize {
    SUBSCRIPTION_REFRESH_OPTIONS
        .iter()
        .position(|option| *option == refresh)
        .unwrap_or_default()
}

fn subscription_refresh_from_index(index: usize) -> SubscriptionRefresh {
    SUBSCRIPTION_REFRESH_OPTIONS
        .get(index)
        .copied()
        .unwrap_or_default()
}

fn panel<'a>(title: &'a str, lines: Vec<Line<'a>>) -> Paragraph<'a> {
    Paragraph::new(lines)
        .style(Style::default().fg(Color::White).bg(Color::Black))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Gray).bg(Color::Black))
                .title(title),
        )
        .wrap(Wrap { trim: true })
}

fn bios_panel<'a>(title: &'a str, lines: Vec<Line<'a>>) -> Paragraph<'a> {
    Paragraph::new(lines)
        .style(Style::default().fg(Color::White).bg(Color::Black))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Gray).bg(Color::Black))
                .title(Span::styled(
                    title.to_string(),
                    Style::default()
                        .fg(Color::White)
                        .bg(Color::Black)
                        .add_modifier(Modifier::BOLD),
                )),
        )
        .wrap(Wrap { trim: true })
}

fn focused_block(title: &str, focused: bool) -> Block<'_> {
    let style = if focused {
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };
    Block::default()
        .borders(Borders::ALL)
        .title(Span::styled(title.to_string(), style))
}

fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1])[1]
}

const fn next_index(current: usize, len: usize) -> usize {
    if len == 0 { 0 } else { (current + 1) % len }
}

const fn prev_index(current: usize, len: usize) -> usize {
    if len == 0 {
        0
    } else if current == 0 {
        len - 1
    } else {
        current - 1
    }
}

fn active_subscription_index(config: &AppConfig) -> Option<usize> {
    let active = config.active_profile.as_ref()?;
    config
        .subscriptions
        .iter()
        .position(|subscription| &subscription.name == active)
}

fn current_proxy_index(group: Option<&ProxyGroup>) -> Option<usize> {
    let group = group?;
    group.all.iter().position(|proxy| proxy == &group.now)
}

fn split_list(value: &str) -> Vec<String> {
    value
        .split([',', ';', '\n'])
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(str::to_string)
        .collect()
}

fn parse_port(value: &str, label: &str) -> Result<u16> {
    let port = value
        .parse::<u16>()
        .with_context(|| format!("invalid {label}: {value}"))?;
    if port == 0 {
        anyhow::bail!("{label} must be greater than 0");
    }
    Ok(port)
}

fn parse_port_or_alert(app: &mut ConfigApp, value: &str, label: &str) -> Option<u16> {
    match parse_port(value, label) {
        Ok(port) => Some(port),
        Err(_) => {
            app.alert("Invalid Port", "Port must be 1..65535.");
            None
        }
    }
}

fn parse_optional_port(value: &str, label: &str) -> Result<Option<u16>> {
    let value = value.trim();
    let lower = value.to_ascii_lowercase();
    if value.is_empty() || matches!(lower.as_str(), "0" | "off" | "none" | "disabled") {
        return Ok(None);
    }
    Ok(Some(parse_port(value, label)?))
}

fn parse_optional_port_or_alert(
    app: &mut ConfigApp,
    value: &str,
    label: &str,
) -> Option<Option<u16>> {
    match parse_optional_port(value, label) {
        Ok(port) => Some(port),
        Err(_) => {
            app.alert("Invalid Port", "Port must be 1..65535, off, or empty.");
            None
        }
    }
}

fn optional_port_value(port: Option<u16>) -> String {
    port.map_or_else(|| "off".into(), |port| port.to_string())
}

fn is_number_input(input_mode: InputMode) -> bool {
    matches!(
        input_mode,
        InputMode::MixedPort | InputMode::HttpPort | InputMode::SocksPort
    )
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

fn join_list(values: &[String]) -> String {
    values.join(", ")
}

const fn on_off(value: bool) -> &'static str {
    if value { "on" } else { "off" }
}

fn display_name(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let mut pending_space = false;

    for ch in value.chars() {
        if is_emoji_codepoint(ch) {
            continue;
        }
        if ch.is_whitespace() {
            if !output.is_empty() {
                pending_space = true;
            }
            continue;
        }
        if pending_space {
            output.push(' ');
            pending_space = false;
        }
        output.push(ch);
    }

    if output.is_empty() {
        "-".into()
    } else {
        output
    }
}

const fn is_emoji_codepoint(ch: char) -> bool {
    matches!(
        ch as u32,
        0x1F000..=0x1FAFF
            | 0x2600..=0x27BF
            | 0xFE00..=0xFE0F
            | 0xE0020..=0xE007F
            | 0x200D
            | 0x20E3
    )
}

fn print_config(paths: &Paths, config: &AppConfig) -> Result<()> {
    println!("# {}", paths.config_file.display());
    println!("{}", serde_yaml_ng::to_string(config)?);
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_name_removes_flag_emoji_and_keeps_node_text() {
        assert_eq!(display_name("🇺🇸 美国 01"), "美国 01");
        assert_eq!(
            display_name("🇺🇸美国洛杉矶专线-0.1倍率"),
            "美国洛杉矶专线-0.1倍率"
        );
        assert_eq!(display_name("Google-US"), "Google-US");
    }

    #[test]
    fn split_list_accepts_common_separators() {
        assert_eq!(
            split_list("system, 192.168.0.1; https://dns.alidns.com/dns-query"),
            vec![
                "system".to_string(),
                "192.168.0.1".to_string(),
                "https://dns.alidns.com/dns-query".to_string()
            ]
        );
    }
}
