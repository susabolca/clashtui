use std::io::{self, IsTerminal as _, Stdout};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::{Frame, Terminal};

use crate::config::{AppConfig, Paths, Subscription};
use crate::core;
use crate::dns;
use crate::mihomo::{MihomoClient, ProxyGroup};
use crate::runtime_profile;
use crate::subscription;

const TICK_RATE: Duration = Duration::from_millis(200);
const REFRESH_INTERVAL: Duration = Duration::from_secs(5);

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
        terminal.terminal.draw(|frame| draw(frame, paths, config, &app))?;
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

    config.save(paths).await?;
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Page {
    Main,
    Subscriptions,
    Proxies,
    Dns,
    Runtime,
    Help,
}

impl Page {
    const ALL: [Self; 6] = [
        Self::Main,
        Self::Subscriptions,
        Self::Proxies,
        Self::Dns,
        Self::Runtime,
        Self::Help,
    ];

    const fn next(self) -> Self {
        match self {
            Self::Main => Self::Subscriptions,
            Self::Subscriptions => Self::Proxies,
            Self::Proxies => Self::Dns,
            Self::Dns => Self::Runtime,
            Self::Runtime => Self::Help,
            Self::Help => Self::Main,
        }
    }

    const fn prev(self) -> Self {
        match self {
            Self::Main => Self::Help,
            Self::Subscriptions => Self::Main,
            Self::Proxies => Self::Subscriptions,
            Self::Dns => Self::Proxies,
            Self::Runtime => Self::Dns,
            Self::Help => Self::Runtime,
        }
    }

    const fn title(self) -> &'static str {
        match self {
            Self::Main => "Main",
            Self::Subscriptions => "Subscriptions",
            Self::Proxies => "Proxy Groups",
            Self::Dns => "DNS",
            Self::Runtime => "Runtime",
            Self::Help => "Help",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MainItem {
    CorePath,
    Controller,
    MixedPort,
    HttpPort,
    SocksPort,
    RuntimeMode,
    SystemProxy,
    Tun,
    Dns,
    Subscriptions,
    ProxyGroups,
    Refresh,
}

impl MainItem {
    const ALL: [Self; 12] = [
        Self::CorePath,
        Self::Controller,
        Self::MixedPort,
        Self::HttpPort,
        Self::SocksPort,
        Self::RuntimeMode,
        Self::SystemProxy,
        Self::Tun,
        Self::Dns,
        Self::Subscriptions,
        Self::ProxyGroups,
        Self::Refresh,
    ];

    const fn label(self) -> &'static str {
        match self {
            Self::CorePath => "Mihomo Core",
            Self::Controller => "Controller",
            Self::MixedPort => "Mixed Port",
            Self::HttpPort => "HTTP Port",
            Self::SocksPort => "SOCKS Port",
            Self::RuntimeMode => "Mode",
            Self::SystemProxy => "System Proxy",
            Self::Tun => "TUN",
            Self::Dns => "DNS",
            Self::Subscriptions => "Subscriptions",
            Self::ProxyGroups => "Proxy Groups",
            Self::Refresh => "Refresh Runtime",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RuntimeItem {
    Rule,
    Global,
    Direct,
    Refresh,
    SystemProxy,
    Tun,
    Dns,
}

impl RuntimeItem {
    const ALL: [Self; 7] = [
        Self::Rule,
        Self::Global,
        Self::Direct,
        Self::Refresh,
        Self::SystemProxy,
        Self::Tun,
        Self::Dns,
    ];

    const fn label(self) -> &'static str {
        match self {
            Self::Rule => "Rule based",
            Self::Global => "Global",
            Self::Direct => "Direct",
            Self::Refresh => "Refresh mihomo",
            Self::SystemProxy => "System Proxy",
            Self::Tun => "TUN",
            Self::Dns => "DNS",
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
    AddName,
    AddUrl,
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
            Self::AddName => "Add Subscription Name",
            Self::AddUrl => "Add Subscription URL",
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
    error: Option<String>,
}

struct ConfigApp {
    page: Page,
    proxy_pane: ProxyPane,
    selected_main: usize,
    selected_runtime: usize,
    selected_subscription: usize,
    selected_group: usize,
    selected_proxy: usize,
    selected_dns: usize,
    input_mode: InputMode,
    input: String,
    pending_name: Option<String>,
    runtime: RuntimeState,
    status: String,
    should_quit: bool,
}

impl ConfigApp {
    fn new(config: &AppConfig) -> Self {
        Self {
            page: Page::Main,
            proxy_pane: ProxyPane::Groups,
            selected_main: 0,
            selected_runtime: 0,
            selected_subscription: active_subscription_index(config).unwrap_or_default(),
            selected_group: 0,
            selected_proxy: 0,
            selected_dns: 0,
            input_mode: InputMode::Normal,
            input: String::new(),
            pending_name: None,
            runtime: RuntimeState::default(),
            status: "Ready. Press F1 or h for Help.".into(),
            should_quit: false,
        }
    }

    async fn refresh_runtime(&mut self, config: &AppConfig) {
        let client = MihomoClient::new(&config.controller);
        let (version, configs, groups) = tokio::join!(client.version(), client.configs(), client.proxy_groups());

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
    }

    fn current_group(&self) -> Option<&ProxyGroup> {
        self.runtime.groups.get(self.selected_group)
    }

    fn selected_main_item(&self) -> MainItem {
        MainItem::ALL
            .get(self.selected_main)
            .copied()
            .unwrap_or(MainItem::CorePath)
    }

    fn selected_runtime_item(&self) -> RuntimeItem {
        RuntimeItem::ALL
            .get(self.selected_runtime)
            .copied()
            .unwrap_or(RuntimeItem::Rule)
    }

    fn selected_dns_item(&self) -> DnsItem {
        DnsItem::ALL.get(self.selected_dns).copied().unwrap_or(DnsItem::Enabled)
    }

    fn clamp_selection(&mut self, config: &AppConfig) {
        if self.selected_main >= MainItem::ALL.len() {
            self.selected_main = MainItem::ALL.len() - 1;
        }
        if self.selected_runtime >= RuntimeItem::ALL.len() {
            self.selected_runtime = RuntimeItem::ALL.len() - 1;
        }
        if self.selected_dns >= DnsItem::ALL.len() {
            self.selected_dns = DnsItem::ALL.len() - 1;
        }
        if config.subscriptions.is_empty() {
            self.selected_subscription = 0;
        } else if self.selected_subscription >= config.subscriptions.len() {
            self.selected_subscription = config.subscriptions.len() - 1;
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

    fn next_page(&mut self) {
        self.page = self.page.next();
        self.status = format!("Page: {}", self.page.title());
    }

    fn prev_page(&mut self) {
        self.page = self.page.prev();
        self.status = format!("Page: {}", self.page.title());
    }

    fn move_right(&mut self) {
        if self.page == Page::Proxies && self.proxy_pane == ProxyPane::Groups {
            self.proxy_pane = ProxyPane::Proxies;
            self.selected_proxy = current_proxy_index(self.current_group()).unwrap_or_default();
            self.status = "Proxy pane: Proxies".into();
        } else {
            self.next_page();
        }
    }

    fn move_left(&mut self) {
        if self.page == Page::Proxies && self.proxy_pane == ProxyPane::Proxies {
            self.proxy_pane = ProxyPane::Groups;
            self.status = "Proxy pane: Groups".into();
        } else {
            self.prev_page();
        }
    }

    fn move_next(&mut self, config: &AppConfig) {
        match self.page {
            Page::Main => self.selected_main = next_index(self.selected_main, MainItem::ALL.len()),
            Page::Subscriptions => self.select_next_subscription(config),
            Page::Proxies => match self.proxy_pane {
                ProxyPane::Groups => self.select_next_group(),
                ProxyPane::Proxies => self.select_next_proxy(),
            },
            Page::Dns => self.selected_dns = next_index(self.selected_dns, DnsItem::ALL.len()),
            Page::Runtime => self.selected_runtime = next_index(self.selected_runtime, RuntimeItem::ALL.len()),
            Page::Help => {}
        }
    }

    fn move_prev(&mut self, config: &AppConfig) {
        match self.page {
            Page::Main => self.selected_main = prev_index(self.selected_main, MainItem::ALL.len()),
            Page::Subscriptions => self.select_prev_subscription(config),
            Page::Proxies => match self.proxy_pane {
                ProxyPane::Groups => self.select_prev_group(),
                ProxyPane::Proxies => self.select_prev_proxy(),
            },
            Page::Dns => self.selected_dns = prev_index(self.selected_dns, DnsItem::ALL.len()),
            Page::Runtime => self.selected_runtime = prev_index(self.selected_runtime, RuntimeItem::ALL.len()),
            Page::Help => {}
        }
    }

    const fn select_next_subscription(&mut self, config: &AppConfig) {
        if config.subscriptions.is_empty() {
            return;
        }
        self.selected_subscription = (self.selected_subscription + 1) % config.subscriptions.len();
    }

    const fn select_prev_subscription(&mut self, config: &AppConfig) {
        if config.subscriptions.is_empty() {
            return;
        }
        self.selected_subscription = if self.selected_subscription == 0 {
            config.subscriptions.len() - 1
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

    const fn is_input(&self) -> bool {
        !matches!(self.input_mode, InputMode::Normal)
    }

    fn cancel_input(&mut self) {
        self.input_mode = InputMode::Normal;
        self.input.clear();
        self.pending_name = None;
        self.status = "Canceled".into();
    }
}

async fn handle_key(paths: &Paths, config: &mut AppConfig, app: &mut ConfigApp, key: KeyEvent) -> Result<()> {
    if app.is_input() {
        return handle_input_key(paths, config, app, key).await;
    }

    match key.code {
        KeyCode::Char('q') | KeyCode::F(10) => app.should_quit = true,
        KeyCode::Esc => {
            if app.page == Page::Help {
                app.page = Page::Main;
                app.status = "Back to Main".into();
            } else {
                app.should_quit = true;
            }
        }
        KeyCode::F(1) | KeyCode::Char('h') | KeyCode::Char('?') => {
            app.page = Page::Help;
            app.status = "Help".into();
        }
        KeyCode::Tab => app.next_page(),
        KeyCode::BackTab => app.prev_page(),
        KeyCode::Right => app.move_right(),
        KeyCode::Left => app.move_left(),
        KeyCode::Down | KeyCode::Char('j') => app.move_next(config),
        KeyCode::Up | KeyCode::Char('k') => app.move_prev(config),
        KeyCode::Enter | KeyCode::Char(' ') => submit_selection(paths, config, app).await?,
        KeyCode::Char('1') => set_runtime_mode(paths, config, app, "rule").await?,
        KeyCode::Char('2') => set_runtime_mode(paths, config, app, "global").await?,
        KeyCode::Char('3') => set_runtime_mode(paths, config, app, "direct").await?,
        KeyCode::Char('r') => refresh_runtime(config, app).await,
        KeyCode::Char('a') => begin_add_subscription(app),
        KeyCode::Char('u') => update_selected_subscription(paths, config, app).await?,
        KeyCode::Char('x') => clear_active_subscription(paths, config, app).await?,
        KeyCode::Char('s') => toggle_system_proxy(paths, config, app).await?,
        KeyCode::Char('t') => toggle_tun(paths, config, app).await?,
        KeyCode::Char('d') => toggle_dns(paths, config, app).await?,
        KeyCode::Char('c') => begin_controller_input(config, app),
        KeyCode::Char('b') => begin_core_path_input(config, app),
        KeyCode::Char('m') => begin_mixed_port_input(config, app),
        _ => {}
    }
    app.clamp_selection(config);
    Ok(())
}

async fn handle_input_key(paths: &Paths, config: &mut AppConfig, app: &mut ConfigApp, key: KeyEvent) -> Result<()> {
    match key.code {
        KeyCode::Esc => app.cancel_input(),
        KeyCode::Backspace => {
            app.input.pop();
        }
        KeyCode::Enter => submit_input(paths, config, app).await?,
        KeyCode::Char(value) => app.input.push(value),
        _ => {}
    }
    Ok(())
}

async fn submit_selection(paths: &Paths, config: &mut AppConfig, app: &mut ConfigApp) -> Result<()> {
    match app.page {
        Page::Main => submit_main_item(paths, config, app).await?,
        Page::Subscriptions => select_subscription(paths, config, app).await?,
        Page::Proxies => match app.proxy_pane {
            ProxyPane::Groups => {
                app.proxy_pane = ProxyPane::Proxies;
                app.selected_proxy = current_proxy_index(app.current_group()).unwrap_or_default();
                app.status = "Select a proxy, then press Enter".into();
            }
            ProxyPane::Proxies => select_proxy(paths, config, app).await?,
        },
        Page::Dns => submit_dns_item(paths, config, app).await?,
        Page::Runtime => submit_runtime_item(paths, config, app).await?,
        Page::Help => {
            app.page = Page::Main;
            app.status = "Back to Main".into();
        }
    }
    Ok(())
}

async fn submit_main_item(paths: &Paths, config: &mut AppConfig, app: &mut ConfigApp) -> Result<()> {
    match app.selected_main_item() {
        MainItem::CorePath => begin_core_path_input(config, app),
        MainItem::Controller => begin_controller_input(config, app),
        MainItem::MixedPort => begin_mixed_port_input(config, app),
        MainItem::HttpPort => begin_http_port_input(config, app),
        MainItem::SocksPort => begin_socks_port_input(config, app),
        MainItem::RuntimeMode => {
            app.page = Page::Runtime;
            app.status = "Choose a runtime mode".into();
        }
        MainItem::SystemProxy => toggle_system_proxy(paths, config, app).await?,
        MainItem::Tun => toggle_tun(paths, config, app).await?,
        MainItem::Dns => {
            app.page = Page::Dns;
            app.status = "DNS settings".into();
        }
        MainItem::Subscriptions => {
            app.page = Page::Subscriptions;
            app.status = "Subscriptions".into();
        }
        MainItem::ProxyGroups => {
            app.page = Page::Proxies;
            app.proxy_pane = ProxyPane::Groups;
            app.status = "Proxy Groups".into();
        }
        MainItem::Refresh => refresh_runtime(config, app).await,
    }
    Ok(())
}

async fn submit_runtime_item(paths: &Paths, config: &mut AppConfig, app: &mut ConfigApp) -> Result<()> {
    match app.selected_runtime_item() {
        RuntimeItem::Rule => set_runtime_mode(paths, config, app, "rule").await?,
        RuntimeItem::Global => set_runtime_mode(paths, config, app, "global").await?,
        RuntimeItem::Direct => set_runtime_mode(paths, config, app, "direct").await?,
        RuntimeItem::Refresh => refresh_runtime(config, app).await,
        RuntimeItem::SystemProxy => toggle_system_proxy(paths, config, app).await?,
        RuntimeItem::Tun => toggle_tun(paths, config, app).await?,
        RuntimeItem::Dns => toggle_dns(paths, config, app).await?,
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
            config.dns.direct_nameserver_follow_policy = !config.dns.direct_nameserver_follow_policy;
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
        InputMode::AddName => {
            if value.is_empty() {
                app.status = "Name is empty".into();
                return Ok(());
            }
            if config.subscriptions.iter().any(|sub| sub.name == value) {
                app.status = format!("Subscription already exists: {value}");
                return Ok(());
            }
            app.pending_name = Some(value);
            app.input.clear();
            app.input_mode = InputMode::AddUrl;
            app.status = "Enter subscription URL".into();
        }
        InputMode::AddUrl => add_subscription(paths, config, app, value).await?,
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
            let port = parse_port(&value, "mixed port")?;
            config.mixed_port = port;
            config.save(paths).await?;
            reload_current_runtime(paths, config, app, format!("Mixed port={port}")).await;
            app.input.clear();
            app.input_mode = InputMode::Normal;
        }
        InputMode::HttpPort => {
            config.proxy_ports.http = parse_optional_port(&value, "HTTP port")?;
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
            config.proxy_ports.socks = parse_optional_port(&value, "SOCKS port")?;
            config.save(paths).await?;
            reload_current_runtime(
                paths,
                config,
                app,
                format!("SOCKS port={}", optional_port_value(config.proxy_ports.socks)),
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
    app.page = Page::Subscriptions;
    app.input_mode = InputMode::AddName;
    app.input.clear();
    app.pending_name = None;
    app.status = "Enter subscription name".into();
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

async fn clear_active_subscription(paths: &Paths, config: &mut AppConfig, app: &mut ConfigApp) -> Result<()> {
    config.active_profile = None;
    config.save(paths).await?;
    app.status = "Active subscription cleared".into();
    Ok(())
}

async fn toggle_system_proxy(paths: &Paths, config: &mut AppConfig, app: &mut ConfigApp) -> Result<()> {
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
    save_dns_config(paths, config, app, &format!("DNS={}", on_off(config.dns.enable))).await?;
    Ok(())
}

async fn save_dns_config(paths: &Paths, config: &AppConfig, app: &mut ConfigApp, status: &str) -> Result<()> {
    config.save(paths).await?;
    app.status = format!("{status}; daemon will apply config");
    Ok(())
}

async fn add_subscription(paths: &Paths, config: &mut AppConfig, app: &mut ConfigApp, url: String) -> Result<()> {
    if url.is_empty() {
        app.status = "URL is empty".into();
        return Ok(());
    }

    let Some(name) = app.pending_name.take() else {
        app.status = "Missing subscription name".into();
        app.input_mode = InputMode::AddName;
        return Ok(());
    };

    config.subscriptions.push(Subscription {
        name: name.clone(),
        url,
        updated_at: None,
    });
    config.active_profile = Some(name.clone());
    app.selected_subscription = config.subscriptions.len() - 1;
    app.input.clear();
    app.input_mode = InputMode::Normal;

    match subscription::update(paths, config, app.selected_subscription).await {
        Ok(path) => app.status = format!("Added {}; downloaded {}", display_name(&name), path.display()),
        Err(err) => app.status = format!("Added {}; download failed: {err}", display_name(&name)),
    }
    config.save(paths).await?;
    load_selected_profile(paths, config, app).await;
    Ok(())
}

async fn select_subscription(paths: &Paths, config: &mut AppConfig, app: &mut ConfigApp) -> Result<()> {
    let Some(sub) = config.subscriptions.get(app.selected_subscription) else {
        app.status = "No subscription selected".into();
        return Ok(());
    };
    config.active_profile = Some(sub.name.clone());
    config.save(paths).await?;
    app.status = format!("Active subscription={}", display_name(&sub.name));
    load_selected_profile(paths, config, app).await;
    Ok(())
}

async fn update_selected_subscription(paths: &Paths, config: &mut AppConfig, app: &mut ConfigApp) -> Result<()> {
    if config.subscriptions.is_empty() {
        app.status = "No subscription selected".into();
        return Ok(());
    }

    app.clamp_selection(config);
    match subscription::update(paths, config, app.selected_subscription).await {
        Ok(path) => {
            config.save(paths).await?;
            app.status = format!("Updated {}", path.display());
            if config.active_profile.as_ref() == Some(&config.subscriptions[app.selected_subscription].name) {
                load_selected_profile(paths, config, app).await;
            }
        }
        Err(err) => app.status = format!("Update failed: {err}"),
    }
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

async fn reload_current_runtime(paths: &Paths, config: &AppConfig, app: &mut ConfigApp, success: String) {
    let client = MihomoClient::new(&config.controller);
    let path = match runtime_profile::write_current_config(paths, config).await {
        Ok(path) => path,
        Err(err) => {
            app.status = format!("{success} saved; runtime config failed: {err}");
            return;
        }
    };

    match client.reload_config(&path).await {
        Ok(()) => {
            app.status = success;
            app.refresh_runtime(config).await;
        }
        Err(err) => app.status = format!("{success} saved; runtime reload failed: {err}"),
    }
}

async fn set_runtime_mode(paths: &Paths, config: &mut AppConfig, app: &mut ConfigApp, mode: &str) -> Result<()> {
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
    config.proxy_selections.insert(group_name.clone(), proxy_name.clone());
    config.save(paths).await?;
    let client = MihomoClient::new(&config.controller);
    match client.select_proxy(&group_name, &proxy_name).await {
        Ok(()) => {
            app.status = format!("{} -> {} saved", display_name(&group_name), display_name(&proxy_name));
            app.refresh_runtime(config).await;
        }
        Err(err) => app.status = format!("Proxy selection saved; runtime patch failed: {err}"),
    }
    Ok(())
}

fn draw(frame: &mut Frame, paths: &Paths, config: &AppConfig, app: &ConfigApp) {
    let area = frame.area();
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(4), Constraint::Min(8), Constraint::Length(4)])
        .split(area);

    draw_header(frame, app, layout[0]);
    draw_body(frame, paths, config, app, layout[1]);
    draw_footer(frame, app, layout[2]);

    if app.is_input() {
        draw_input(frame, app);
    }
}

fn draw_header(frame: &mut Frame, app: &ConfigApp, area: Rect) {
    let mut tabs = Vec::new();
    for page in Page::ALL {
        tabs.push(Span::raw(" "));
        let style = if page == app.page {
            Style::default()
                .fg(Color::White)
                .bg(Color::Blue)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Gray)
        };
        tabs.push(Span::styled(format!(" {} ", page.title()), style));
    }

    let lines = vec![
        Line::from(Span::styled(
            "clashtui Setup Utility",
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        ))
        .alignment(Alignment::Center),
        Line::from(tabs),
    ];
    frame.render_widget(
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL)),
        area,
    );
}

fn draw_body(frame: &mut Frame, paths: &Paths, config: &AppConfig, app: &ConfigApp, area: Rect) {
    match app.page {
        Page::Main => draw_main_page(frame, paths, config, app, area),
        Page::Subscriptions => draw_subscriptions_page(frame, paths, config, app, area),
        Page::Proxies => draw_proxies_page(frame, config, app, area),
        Page::Dns => draw_dns_page(frame, config, app, area),
        Page::Runtime => draw_runtime_page(frame, config, app, area),
        Page::Help => draw_help_page(frame, area),
    }
}

fn draw_main_page(frame: &mut Frame, paths: &Paths, config: &AppConfig, app: &ConfigApp, area: Rect) {
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
        .split(area);
    draw_main_menu(frame, config, app, columns[0]);
    draw_main_help(frame, paths, config, app, columns[1]);
}

fn draw_main_menu(frame: &mut Frame, config: &AppConfig, app: &ConfigApp, area: Rect) {
    let items = MainItem::ALL
        .iter()
        .map(|item| {
            ListItem::new(Line::from(vec![
                Span::raw(format!("{:<18}", item.label())),
                Span::styled(main_item_value(config, app, *item), Style::default().fg(Color::Gray)),
            ]))
        })
        .collect::<Vec<_>>();

    let mut state = ListState::default();
    state.select(Some(app.selected_main));
    let list = List::new(items)
        .block(focused_block("Setup Menu", true))
        .highlight_symbol(">> ")
        .highlight_style(Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD));
    frame.render_stateful_widget(list, area, &mut state);
}

fn draw_main_help(frame: &mut Frame, paths: &Paths, config: &AppConfig, app: &ConfigApp, area: Rect) {
    let item = app.selected_main_item();
    let mut lines = vec![
        Line::from(Span::styled(
            item.label(),
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
    ];
    for help in main_item_help(item) {
        lines.push(Line::from(*help));
    }
    lines.push(Line::from(""));
    lines.extend(runtime_summary(paths, config, app));
    frame.render_widget(panel("Item Help", lines), area);
}

fn draw_subscriptions_page(frame: &mut Frame, paths: &Paths, config: &AppConfig, app: &ConfigApp, area: Rect) {
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(52), Constraint::Percentage(48)])
        .split(area);
    draw_subscriptions(frame, config, app, columns[0]);
    draw_subscription_help(frame, paths, config, app, columns[1]);
}

fn draw_subscriptions(frame: &mut Frame, config: &AppConfig, app: &ConfigApp, area: Rect) {
    let items = if config.subscriptions.is_empty() {
        vec![ListItem::new("No subscriptions. Press a to add one.")]
    } else {
        config
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
                    Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                ListItem::new(vec![
                    Line::from(Span::styled(format!("{active} {}", display_name(&sub.name)), selected)),
                    Line::from(Span::styled(
                        format!("  updated={}  {}", sub.updated_at.as_deref().unwrap_or("-"), sub.url),
                        Style::default().fg(Color::Gray),
                    )),
                ])
            })
            .collect()
    };

    let mut state = ListState::default();
    if !config.subscriptions.is_empty() {
        state.select(Some(app.selected_subscription));
    }
    let list = List::new(items)
        .block(focused_block("Subscriptions", true))
        .highlight_symbol(">> ")
        .highlight_style(Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD));
    frame.render_stateful_widget(list, area, &mut state);
}

fn draw_subscription_help(frame: &mut Frame, paths: &Paths, config: &AppConfig, app: &ConfigApp, area: Rect) {
    let mut lines = vec![
        Line::from("Enter: set active and reload profile"),
        Line::from("a: add subscription"),
        Line::from("u: update selected subscription"),
        Line::from("x: clear active subscription"),
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
            Line::from(format!("Updated: {}", sub.updated_at.as_deref().unwrap_or("-"))),
            Line::from(format!("Profile: {}", subscription::profile_path(paths, sub).display())),
            Line::from(""),
            Line::from(sub.url.clone()),
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
                    Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
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
            app.page == Page::Proxies && app.proxy_pane == ProxyPane::Groups,
        ))
        .highlight_symbol(">> ")
        .highlight_style(Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD));
    frame.render_stateful_widget(list, area, &mut state);
}

fn draw_proxy_options(frame: &mut Frame, app: &ConfigApp, area: Rect) {
    let Some(group) = app.current_group() else {
        frame.render_widget(panel("Group Proxies", vec![Line::from("No group selected")]), area);
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
                    Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
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
            app.page == Page::Proxies && app.proxy_pane == ProxyPane::Proxies,
        ))
        .highlight_symbol(">> ")
        .highlight_style(Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD));
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
                Span::styled(runtime_item_value(config, app, *item), Style::default().fg(Color::Gray)),
            ]))
        })
        .collect::<Vec<_>>();

    let mut state = ListState::default();
    state.select(Some(app.selected_runtime));
    let list = List::new(items)
        .block(focused_block("Runtime Menu", true))
        .highlight_symbol(">> ")
        .highlight_style(Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD));
    frame.render_stateful_widget(list, area, &mut state);
}

fn draw_runtime_help(frame: &mut Frame, config: &AppConfig, app: &ConfigApp, area: Rect) {
    let lines = vec![
        Line::from("Enter applies the selected runtime item."),
        Line::from("Rule based: use profile rules."),
        Line::from("Global: send all traffic to GLOBAL."),
        Line::from("Direct: bypass proxy for all traffic."),
        Line::from(""),
        Line::from(format!("Configured mode: {}", config.runtime_mode)),
        Line::from(format!(
            "mihomo mode: {}",
            app.runtime.mode.as_deref().unwrap_or("unknown")
        )),
        Line::from(format!("mihomo: {}", app.runtime.error.as_deref().unwrap_or("online"))),
        Line::from(format!("System Proxy: {}", on_off(config.system_proxy.enabled))),
        Line::from(format!("TUN: {}", on_off(config.tun.enable))),
        Line::from(format!("DNS: {}", on_off(config.dns.enable))),
    ];
    frame.render_widget(panel("Help", lines), area);
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
                Span::styled(dns_item_value(config, *item), Style::default().fg(Color::Gray)),
            ]))
        })
        .collect::<Vec<_>>();

    let mut state = ListState::default();
    state.select(Some(app.selected_dns));
    let list = List::new(items)
        .block(focused_block("DNS Menu", true))
        .highlight_symbol(">> ")
        .highlight_style(Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD));
    frame.render_stateful_widget(list, area, &mut state);
}

fn draw_dns_help(frame: &mut Frame, config: &AppConfig, app: &ConfigApp, area: Rect) {
    let item = app.selected_dns_item();
    let policy = dns::effective_nameserver_policy(&config.dns);
    let lines = vec![
        Line::from(Span::styled(
            item.label(),
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(dns_item_help(item)),
        Line::from(""),
        Line::from("List values are comma separated."),
        Line::from("Use system for the OS resolver, or IP/DoH/DoT servers."),
        Line::from(""),
        Line::from(format!("Effective policy entries: {}", policy.len())),
        Line::from(format!("LAN domains: {}", compact_list(&config.dns.lan_domains))),
        Line::from(format!("LAN DNS: {}", compact_list(&config.dns.lan_nameserver))),
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
            "Keyboard",
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        )),
        Line::from("Tab / Shift+Tab: switch top tabs"),
        Line::from("Up / Down: move in the current menu"),
        Line::from("Left / Right: switch tabs; on Proxy Groups, switch panes"),
        Line::from("Enter / Space: choose the highlighted item"),
        Line::from("F1 / h / ?: open Help"),
        Line::from("F10 / q: save and exit"),
        Line::from("Esc: leave Help, or save and exit from other pages"),
        Line::from(""),
        Line::from(Span::styled(
            "Basic flow",
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        )),
        Line::from("1. Main: check core path, controller, and proxy ports."),
        Line::from("2. Subscriptions: press a to add, Enter to activate, u to update."),
        Line::from("3. DNS: configure LAN domains and DNS servers when needed."),
        Line::from("4. Runtime: choose Rule based, Global, or Direct."),
        Line::from("5. Proxy Groups: choose a group, move right, choose a proxy."),
        Line::from("6. Start daemon with clashtui start so saved settings are applied in background."),
        Line::from(""),
        Line::from(Span::styled(
            "Quick keys",
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        )),
        Line::from("1/2/3 set rule/global/direct. s/t/d toggle System Proxy/TUN/DNS."),
        Line::from("b edits core path. c edits controller. m edits mixed port. r refreshes runtime."),
    ];
    frame.render_widget(panel("Help", lines), area);
}

fn draw_footer(frame: &mut Frame, app: &ConfigApp, area: Rect) {
    let keys = match app.page {
        Page::Main => "Tab page | Up/Down select | Enter edit/toggle/open | F1 Help | F10/q save exit",
        Page::Subscriptions => "Tab page | Up/Down select | Enter activate | a add | u update | x clear | F1 Help",
        Page::Proxies => "Tab page | Left/Right pane | Up/Down select | Enter choose | r refresh | F1 Help",
        Page::Dns => "Tab page | Up/Down select | Enter edit/toggle | d toggle DNS | F1 Help",
        Page::Runtime => "Tab page | Up/Down select | Enter apply | 1/2/3 mode | s/t/d toggles | F1 Help",
        Page::Help => "Tab page | Enter/Esc back to Main | F10/q save exit",
    };
    let lines = vec![
        Line::from(display_name(&app.status)),
        Line::from(Span::styled(keys, Style::default().fg(Color::Gray))),
    ];
    frame.render_widget(
        Paragraph::new(lines)
            .block(Block::default().borders(Borders::ALL).title("Status / Keys"))
            .wrap(Wrap { trim: true }),
        area,
    );
}

fn draw_input(frame: &mut Frame, app: &ConfigApp) {
    let area = centered_rect(72, 28, frame.area());
    frame.render_widget(Clear, area);
    let mut body = vec![
        Line::from("Type a value, then press Enter. Press Esc to cancel."),
        Line::from(""),
    ];
    if app.input_mode == InputMode::AddUrl {
        body.push(Line::from(format!(
            "name: {}",
            app.pending_name.as_deref().unwrap_or("-")
        )));
        body.push(Line::from(""));
    }
    if matches!(
        app.input_mode,
        InputMode::DnsLanDomains
            | InputMode::DnsLanNameserver
            | InputMode::DnsDirectNameserver
            | InputMode::DnsNameserver
            | InputMode::DnsFallback
            | InputMode::DnsFakeIpFilter
    ) {
        body.push(Line::from("Separate multiple values with commas."));
        body.push(Line::from(""));
    }
    if matches!(app.input_mode, InputMode::HttpPort | InputMode::SocksPort) {
        body.push(Line::from("Leave empty, type off, or type 0 to disable this port."));
        body.push(Line::from(""));
    }
    body.push(Line::from(app.input.as_str()));

    let input = Paragraph::new(body)
        .block(Block::default().borders(Borders::ALL).title(app.input_mode.title()))
        .wrap(Wrap { trim: false });
    frame.render_widget(input, area);
}

fn main_item_value(config: &AppConfig, app: &ConfigApp, item: MainItem) -> String {
    match item {
        MainItem::CorePath => config.core_path.as_deref().unwrap_or("auto").to_string(),
        MainItem::Controller => config.controller.url.clone(),
        MainItem::MixedPort => config.mixed_port.to_string(),
        MainItem::HttpPort => optional_port_value(config.proxy_ports.http),
        MainItem::SocksPort => optional_port_value(config.proxy_ports.socks),
        MainItem::RuntimeMode => format!(
            "{} / {}",
            config.runtime_mode,
            app.runtime.mode.as_deref().unwrap_or("unknown")
        ),
        MainItem::SystemProxy => on_off(config.system_proxy.enabled).into(),
        MainItem::Tun => on_off(config.tun.enable).into(),
        MainItem::Dns => on_off(config.dns.enable).into(),
        MainItem::Subscriptions => format!(
            "{} active={}",
            config.subscriptions.len(),
            config
                .active_profile
                .as_deref()
                .map_or_else(|| "-".into(), display_name)
        ),
        MainItem::ProxyGroups => app.runtime.groups.len().to_string(),
        MainItem::Refresh => "Run now".into(),
    }
}

const fn main_item_help(item: MainItem) -> &'static [&'static str] {
    match item {
        MainItem::CorePath => &[
            "Enter edits the mihomo executable path.",
            "Leave it empty to auto-detect from MIHOMO_CORE or PATH.",
        ],
        MainItem::Controller => &[
            "Enter edits the mihomo external-controller URL.",
            "The daemon and TUI use it for /configs and /proxies.",
        ],
        MainItem::MixedPort => &[
            "Enter edits the single mixed proxy port.",
            "mihomo mixed-port accepts HTTP and SOCKS5 on one port.",
        ],
        MainItem::HttpPort => &[
            "Enter edits the optional HTTP proxy port.",
            "Leave empty or type off to disable this extra inbound.",
        ],
        MainItem::SocksPort => &[
            "Enter edits the optional SOCKS5 proxy port.",
            "Leave empty or type off to disable this extra inbound.",
        ],
        MainItem::RuntimeMode => &["Enter opens Runtime mode choices."],
        MainItem::SystemProxy => &["Enter toggles OS system proxy for the mixed port."],
        MainItem::Tun => &["Enter toggles mihomo TUN configuration."],
        MainItem::Dns => &["Enter toggles mihomo DNS configuration."],
        MainItem::Subscriptions => &["Enter opens subscription management."],
        MainItem::ProxyGroups => &["Enter opens proxy group and node selection."],
        MainItem::Refresh => &["Enter refreshes runtime state from mihomo."],
    }
}

fn runtime_item_value(config: &AppConfig, app: &ConfigApp, item: RuntimeItem) -> String {
    match item {
        RuntimeItem::Rule => selected_mode_value(config, app, "rule"),
        RuntimeItem::Global => selected_mode_value(config, app, "global"),
        RuntimeItem::Direct => selected_mode_value(config, app, "direct"),
        RuntimeItem::Refresh => app.runtime.error.as_deref().unwrap_or("online").to_string(),
        RuntimeItem::SystemProxy => on_off(config.system_proxy.enabled).into(),
        RuntimeItem::Tun => on_off(config.tun.enable).into(),
        RuntimeItem::Dns => on_off(config.dns.enable).into(),
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
        DnsItem::LanNameserver => "DNS servers for LAN domains. Use system or router DNS addresses.",
        DnsItem::DirectNameserver => "DNS servers used when traffic exits through DIRECT.",
        DnsItem::DirectFollowPolicy => "When on, DIRECT DNS also follows nameserver-policy.",
        DnsItem::Nameserver => "Default DNS servers for normal domain resolution.",
        DnsItem::Fallback => "Backup DNS servers for polluted or overseas results.",
        DnsItem::FakeIpFilter => "Domain patterns that should get real IPs instead of fake IPs.",
    }
}

fn selected_mode_value(config: &AppConfig, app: &ConfigApp, mode: &str) -> String {
    let configured = if config.runtime_mode == mode { "saved" } else { "" };
    let runtime = if app.runtime.mode.as_deref() == Some(mode) {
        "runtime"
    } else {
        ""
    };
    match (configured.is_empty(), runtime.is_empty()) {
        (true, true) => String::new(),
        (false, true) => configured.into(),
        (true, false) => runtime.into(),
        (false, false) => format!("{configured}, {runtime}"),
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

fn runtime_summary(paths: &Paths, config: &AppConfig, app: &ConfigApp) -> Vec<Line<'static>> {
    let core_path = core::resolve_core_path(config)
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "not found".into());
    vec![
        Line::from(format!("Config: {}", paths.config_file.display())),
        Line::from(format!("Core: {core_path}")),
        Line::from(format!("Controller: {}", config.controller.url)),
        Line::from(format!(
            "mihomo: {} ({})",
            app.runtime.version.as_deref().unwrap_or("offline"),
            app.runtime.error.as_deref().unwrap_or("online")
        )),
        Line::from(format!(
            "Mode: {} desired={}",
            app.runtime.mode.as_deref().unwrap_or("unknown"),
            config.runtime_mode
        )),
        Line::from(format!("Proxy: {}", config.proxy_port_summary())),
        Line::from(format!("Saved proxy picks: {}", config.proxy_selections.len())),
    ]
}

fn panel<'a>(title: &'a str, lines: Vec<Line<'a>>) -> Paragraph<'a> {
    Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL).title(title))
        .wrap(Wrap { trim: true })
}

fn focused_block(title: &str, focused: bool) -> Block<'_> {
    let style = if focused {
        Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
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

fn parse_optional_port(value: &str, label: &str) -> Result<Option<u16>> {
    let value = value.trim();
    let lower = value.to_ascii_lowercase();
    if value.is_empty() || matches!(lower.as_str(), "0" | "off" | "none" | "disabled") {
        return Ok(None);
    }
    Ok(Some(parse_port(value, label)?))
}

fn optional_port_value(port: Option<u16>) -> String {
    port.map_or_else(|| "off".into(), |port| port.to_string())
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

    if output.is_empty() { "-".into() } else { output }
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
        assert_eq!(display_name("🇺🇸美国洛杉矶专线-0.1倍率"), "美国洛杉矶专线-0.1倍率");
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
