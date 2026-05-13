#![allow(dead_code)]

use std::collections::BTreeMap;
use std::io::{self, IsTerminal as _, Stdout};
use std::sync::mpsc::{self, Receiver, TryRecvError};
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
use tokio::task::JoinHandle;
use tokio::time::sleep;

use crate::config::{AppConfig, Paths, PortProxyService, Subscription, SubscriptionRefresh};
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
const DEFAULT_DELAY_TEST_URL: &str = "https://www.gstatic.com/generate_204";
const DEFAULT_DELAY_TEST_TIMEOUT_MS: u64 = 5_000;
const RUNTIME_START_RETRIES: usize = 20;
const RUNTIME_START_WAIT: Duration = Duration::from_millis(250);
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
        app.poll_delay_check();
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
        app.poll_delay_check();
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
    SubscriptionDetail,
    SubscriptionRuleGroups,
    SubscriptionProxies,
    SubscriptionRules,
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
            Self::SubscriptionDetail => "Subscription Detail",
            Self::SubscriptionRuleGroups => "Rule Groups",
            Self::SubscriptionProxies => "Proxies",
            Self::SubscriptionRules => "Rules",
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
            Self::Subscription
            | Self::SubscriptionDetail
            | Self::SubscriptionRuleGroups
            | Self::SubscriptionProxies
            | Self::SubscriptionRules
            | Self::AddSubscription => 1,
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
            Self::OsProxy => "Sys Proxy",
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
    ServicePort,
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
            Self::ServicePort => "Port Proxy Port",
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
    upload_total: u64,
    download_total: u64,
    upload_speed: u64,
    download_speed: u64,
}

struct IpInfoState {
    ip: String,
    country: Option<String>,
    city: Option<String>,
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
    DeleteSubscription,
}

impl ConfirmAction {
    const fn title(self) -> &'static str {
        match self {
            Self::ExitWithoutSaving => "Exit Without Saving?",
            Self::LoadDefaults => "Load Setup Defaults?",
            Self::SaveRestart => "Save And Restart?",
            Self::SaveRestartExit => "Save, Restart, And Exit?",
            Self::DeleteSubscription => "Delete Subscription?",
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
            Self::DeleteSubscription => &["Delete this subscription?"],
        }
    }

    const fn yes_label(self) -> &'static str {
        match self {
            Self::ExitWithoutSaving => "Yes, Exit",
            Self::LoadDefaults => "Yes, Load Defaults",
            Self::SaveRestart => "Yes, Save & Restart",
            Self::SaveRestartExit => "Yes, Save & Restart & Exit",
            Self::DeleteSubscription => "Yes, Delete",
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
    PortProxy,
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
    AddPortProxy,
    AddSubscription,
    SaveSubscription,
    UpdateSubscription,
    DeleteSubscription,
    EditSubscription,
    TestProxyDelay,
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

struct DelayCheckTask {
    receiver: Receiver<DelayCheckEvent>,
    handle: JoinHandle<()>,
    progress: DelayCheckProgress,
}

#[derive(Debug, Clone)]
struct DelayCheckProgress {
    title: String,
    total: usize,
    done: usize,
    ok: usize,
    failed: usize,
    current: String,
    finished: bool,
}

impl DelayCheckProgress {
    fn new(title: String, total: usize) -> Self {
        Self {
            title,
            total,
            done: 0,
            ok: 0,
            failed: 0,
            current: "Starting".into(),
            finished: false,
        }
    }
}

#[derive(Debug)]
enum DelayCheckEvent {
    Checking {
        index: usize,
        proxy: String,
    },
    ProxyFinished {
        subscription_name: String,
        proxy: String,
        delay: Result<u64, String>,
    },
    Finished,
}

struct ConfigApp {
    page: Page,
    section: Page,
    history: Vec<Location>,
    proxy_pane: ProxyPane,
    selected_main: usize,
    selected_runtime: usize,
    selected_subscription: usize,
    selected_subscription_detail: usize,
    selected_subscription_rule_group: usize,
    selected_subscription_proxy: usize,
    selected_subscription_rule: usize,
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
    proxy_delays: BTreeMap<String, String>,
    delay_check: Option<DelayCheckTask>,
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
            selected_subscription_detail: 0,
            selected_subscription_rule_group: 0,
            selected_subscription_proxy: 0,
            selected_subscription_rule: 0,
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
            proxy_delays: BTreeMap::new(),
            delay_check: None,
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

    fn start_delay_check(
        &mut self,
        subscription_name: String,
        client: MihomoClient,
        proxies: Vec<String>,
    ) {
        if let Some(task) = self.delay_check.take() {
            task.handle.abort();
        }

        let total = proxies.len();
        let title = format!("Checking {}", display_name(&subscription_name));
        let (sender, receiver) = mpsc::channel();
        let handle = tokio::spawn(run_delay_check_task(
            sender,
            subscription_name,
            client,
            proxies,
        ));
        self.delay_check = Some(DelayCheckTask {
            receiver,
            handle,
            progress: DelayCheckProgress::new(title, total),
        });
        self.status = format!("Checking 0/{total} proxies");
    }

    fn poll_delay_check(&mut self) {
        let mut events = Vec::new();
        let mut disconnected = false;
        if let Some(task) = self.delay_check.as_mut() {
            loop {
                match task.receiver.try_recv() {
                    Ok(event) => events.push(event),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        disconnected = true;
                        break;
                    }
                }
            }
        }

        for event in events {
            self.apply_delay_check_event(event);
        }

        if disconnected
            && let Some(task) = self.delay_check.as_mut()
            && !task.progress.finished
        {
            task.progress.finished = true;
            task.progress.current = "Stopped".into();
            self.status = format!(
                "Proxy check stopped at {}/{}",
                task.progress.done, task.progress.total
            );
        }
    }

    fn apply_delay_check_event(&mut self, event: DelayCheckEvent) {
        match event {
            DelayCheckEvent::Checking { index, proxy } => {
                if let Some(task) = self.delay_check.as_mut() {
                    task.progress.current = format!("{index}. {}", display_name(&proxy));
                    self.status = format!(
                        "Checking {}/{} {}",
                        task.progress.done,
                        task.progress.total,
                        display_name(&proxy)
                    );
                }
            }
            DelayCheckEvent::ProxyFinished {
                subscription_name,
                proxy,
                delay,
            } => {
                let (value, success) = match delay {
                    Ok(delay) => (format!("{delay}ms"), true),
                    Err(_) => ("fail".into(), false),
                };
                self.proxy_delays.insert(
                    subscription_proxy_delay_key_for_name(&subscription_name, &proxy),
                    value,
                );
                if let Some(task) = self.delay_check.as_mut() {
                    task.progress.done = (task.progress.done + 1).min(task.progress.total);
                    if success {
                        task.progress.ok += 1;
                    } else {
                        task.progress.failed += 1;
                    }
                    task.progress.current = display_name(&proxy);
                    self.status = format!(
                        "Checked {}/{} proxies",
                        task.progress.done, task.progress.total
                    );
                }
            }
            DelayCheckEvent::Finished => {
                if let Some(task) = self.delay_check.as_mut() {
                    task.progress.done = task.progress.total;
                    task.progress.finished = true;
                    task.progress.current = "Complete".into();
                    self.status = format!(
                        "Checked {}/{} proxies, {} failed",
                        task.progress.ok + task.progress.failed,
                        task.progress.total,
                        task.progress.failed
                    );
                }
            }
        }
    }

    fn delay_check_finished(&self) -> bool {
        self.delay_check
            .as_ref()
            .is_some_and(|task| task.progress.finished)
    }

    fn close_delay_check(&mut self) {
        if self.delay_check_finished() {
            self.delay_check = None;
        }
    }

    fn cancel_delay_check(&mut self) {
        let Some(task) = self.delay_check.take() else {
            return;
        };
        task.handle.abort();
        self.status = format!(
            "Proxy check canceled at {}/{}",
            task.progress.done, task.progress.total
        );
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

    fn clamp_selection(&mut self, paths: &Paths, config: &AppConfig) {
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
        if self.selected_subscription_detail >= subscription_detail_count() {
            self.selected_subscription_detail = subscription_detail_count() - 1;
        }
        let rule_group_count = subscription_rule_group_count(config, self);
        if rule_group_count == 0 {
            self.selected_subscription_rule_group = 0;
        } else if self.selected_subscription_rule_group >= rule_group_count {
            self.selected_subscription_rule_group = rule_group_count - 1;
        }
        let proxy_count = subscription_proxy_count(paths, config, self);
        if proxy_count == 0 {
            self.selected_subscription_proxy = 0;
        } else if self.selected_subscription_proxy >= proxy_count {
            self.selected_subscription_proxy = proxy_count - 1;
        }
        if self.selected_subscription_rule >= SUBSCRIPTION_RULE_FALLBACK_COUNT {
            self.selected_subscription_rule = SUBSCRIPTION_RULE_FALLBACK_COUNT - 1;
        }
        let dropdown_count = self.dropdown_item_count(config);
        if dropdown_count > 0 && self.selected_dropdown >= dropdown_count {
            self.selected_dropdown = dropdown_count - 1;
        }
        if self.page == Page::ProxyGroups && proxy_groups_page_is_global_proxy_list(config, self) {
            let proxy_count = route_proxy_names(paths, config, self).len();
            if proxy_count == 0 {
                self.selected_proxy = 0;
            } else if self.selected_proxy >= proxy_count {
                self.selected_proxy = proxy_count - 1;
            }
        } else {
            self.clamp_runtime_selection();
        }
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
            Page::SubscriptionDetail => self.selected_subscription_detail,
            Page::SubscriptionRuleGroups => self.selected_subscription_rule_group,
            Page::SubscriptionProxies => self.selected_subscription_proxy,
            Page::SubscriptionRules => self.selected_subscription_rule,
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
            Page::SubscriptionDetail => self.selected_subscription_detail = selected,
            Page::SubscriptionRuleGroups => self.selected_subscription_rule_group = selected,
            Page::SubscriptionProxies => self.selected_subscription_proxy = selected,
            Page::SubscriptionRules => self.selected_subscription_rule = selected,
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

    fn move_right(&mut self, config: &AppConfig) {
        if self.page == Page::ProxyGroups && proxy_groups_page_is_global_proxy_list(config, self) {
            self.status = "Select a proxy, then press Enter".into();
        } else if self.page == Page::ProxyGroups && self.proxy_pane == ProxyPane::Groups {
            self.proxy_pane = ProxyPane::Proxies;
            self.selected_proxy = current_proxy_index(self.current_group()).unwrap_or_default();
            self.status = "Proxy pane: Proxies".into();
        } else {
            self.next_page();
        }
    }

    fn move_left(&mut self, config: &AppConfig) {
        if self.page == Page::ProxyGroups && proxy_groups_page_is_global_proxy_list(config, self) {
            self.status = "Select a proxy, then press Enter".into();
        } else if self.page == Page::ProxyGroups && self.proxy_pane == ProxyPane::Proxies {
            self.proxy_pane = ProxyPane::Groups;
            self.status = "Proxy pane: Groups".into();
        } else {
            self.prev_page();
        }
    }

    fn move_next(&mut self, paths: &Paths, config: &AppConfig) {
        match self.page {
            Page::Main => {
                self.selected_main = next_index(self.selected_main, main_proxy_count(config))
            }
            Page::Subscription => self.select_next_subscription(config),
            Page::SubscriptionDetail => {
                self.selected_subscription_detail = next_index(
                    self.selected_subscription_detail,
                    subscription_detail_count(),
                )
            }
            Page::SubscriptionRuleGroups => {
                self.selected_subscription_rule_group = next_index(
                    self.selected_subscription_rule_group,
                    subscription_rule_group_count(config, self),
                )
            }
            Page::SubscriptionProxies => {
                self.selected_subscription_proxy = next_index(
                    self.selected_subscription_proxy,
                    subscription_proxy_count(paths, config, self),
                )
            }
            Page::SubscriptionRules => {
                self.selected_subscription_rule = next_index(
                    self.selected_subscription_rule,
                    SUBSCRIPTION_RULE_FALLBACK_COUNT,
                )
            }
            Page::AddSubscription => self.subscription_form.next_field(),
            Page::ProxyConfig => {
                self.selected_proxy_field =
                    next_index(self.selected_proxy_field, self.proxy_config_fields().len())
            }
            Page::ProxyGroups => match self.proxy_pane {
                ProxyPane::Groups => self.select_next_group(),
                ProxyPane::Proxies if proxy_groups_page_is_global_proxy_list(config, self) => {
                    self.selected_proxy = next_index(
                        self.selected_proxy,
                        route_proxy_names(paths, config, self).len(),
                    )
                }
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

    fn move_prev(&mut self, paths: &Paths, config: &AppConfig) {
        match self.page {
            Page::Main => {
                self.selected_main = prev_index(self.selected_main, main_proxy_count(config))
            }
            Page::Subscription => self.select_prev_subscription(config),
            Page::SubscriptionDetail => {
                self.selected_subscription_detail = prev_index(
                    self.selected_subscription_detail,
                    subscription_detail_count(),
                )
            }
            Page::SubscriptionRuleGroups => {
                self.selected_subscription_rule_group = prev_index(
                    self.selected_subscription_rule_group,
                    subscription_rule_group_count(config, self),
                )
            }
            Page::SubscriptionProxies => {
                self.selected_subscription_proxy = prev_index(
                    self.selected_subscription_proxy,
                    subscription_proxy_count(paths, config, self),
                )
            }
            Page::SubscriptionRules => {
                self.selected_subscription_rule = prev_index(
                    self.selected_subscription_rule,
                    SUBSCRIPTION_RULE_FALLBACK_COUNT,
                )
            }
            Page::AddSubscription => self.subscription_form.prev_field(),
            Page::ProxyConfig => {
                self.selected_proxy_field =
                    prev_index(self.selected_proxy_field, self.proxy_config_fields().len())
            }
            Page::ProxyGroups => match self.proxy_pane {
                ProxyPane::Groups => self.select_prev_group(),
                ProxyPane::Proxies if proxy_groups_page_is_global_proxy_list(config, self) => {
                    self.selected_proxy = prev_index(
                        self.selected_proxy,
                        route_proxy_names(paths, config, self).len(),
                    )
                }
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
        self.selected_dropdown = proxy_subscription_index(config, self).unwrap_or_default();
        self.status = "Choose subscription, then press Enter".into();
    }

    fn open_mode_dropdown(&mut self, config: &AppConfig) {
        self.dropdown = Some(Dropdown::Mode);
        self.selected_dropdown = mode_index(proxy_mode(config, self));
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

    if app.delay_check.is_some() {
        app.poll_delay_check();
        match key.code {
            KeyCode::Esc if app.delay_check_finished() => app.close_delay_check(),
            KeyCode::Enter | KeyCode::Char(' ') if app.delay_check_finished() => {
                app.close_delay_check()
            }
            KeyCode::Esc => app.cancel_delay_check(),
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
        KeyCode::Right => app.move_right(config),
        KeyCode::Left => app.move_left(config),
        KeyCode::Down => app.move_next(paths, config),
        KeyCode::Up => app.move_prev(paths, config),
        KeyCode::Char('o') | KeyCode::Char('O') => {
            toggle_selected_main_proxy(paths, config, app).await?
        }
        KeyCode::Enter | KeyCode::Char(' ') => submit_selection(paths, config, app).await?,
        _ => {}
    }
    app.clamp_selection(paths, config);
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
    app.clamp_selection(paths, config);
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
        ConfirmAction::DeleteSubscription => {
            delete_selected_subscription(paths, config, app).await?;
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
            if main_proxy_kind(config, app.selected_main) == MainProxyKind::AddPortProxy {
                add_port_proxy(paths, config, app).await?;
            } else {
                app.selected_proxy_field = 0;
                app.enter_page(
                    Page::ProxyConfig,
                    format!("Configuring {}", main_proxy_name(config, app.selected_main)),
                );
            }
        }
        Page::Subscription => submit_subscription_selection(paths, config, app).await?,
        Page::SubscriptionDetail => submit_subscription_detail(paths, config, app).await?,
        Page::SubscriptionRuleGroups => {
            app.status = "Rule group editing uses the Proxy Groups page for now".into();
        }
        Page::SubscriptionProxies => {
            test_selected_subscription_proxy(paths, config, app).await?;
        }
        Page::SubscriptionRules => {
            app.status = "Rules are read-only in this view".into();
        }
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
        SubscriptionAction::Open(index) => {
            app.selected_subscription = index;
            app.enter_page(
                Page::SubscriptionDetail,
                format!(
                    "Subscription: {}",
                    display_name(&config.subscriptions[index].name)
                ),
            );
        }
        SubscriptionAction::Add => begin_add_subscription(app),
    }
    let _ = paths;
    Ok(())
}

async fn submit_subscription_detail(
    paths: &Paths,
    config: &mut AppConfig,
    app: &mut ConfigApp,
) -> Result<()> {
    match app.selected_subscription_detail {
        0 => {
            app.status = "Overview is read-only".into();
        }
        1 => app.enter_page(Page::SubscriptionRuleGroups, "Subscription rule groups"),
        2 => app.enter_page(Page::SubscriptionProxies, "Subscription proxies"),
        3 => app.enter_page(Page::SubscriptionRules, "Subscription rules"),
        4 => update_selected_subscription(paths, config, app).await?,
        5 => {
            app.status =
                "Subscription edit is pending; delete and add can replace it for now".into()
        }
        6 => app.open_confirm(ConfirmAction::DeleteSubscription),
        _ => {}
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
    set_selected_proxy_subscription(paths, config, app, index).await
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
    set_selected_proxy_mode(paths, config, app, mode.value()).await?;
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
    set_selected_proxy_mode(paths, config, app, mode.value()).await?;
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
            toggle_selected_main_proxy(paths, config, app).await?;
        }
        ProxyConfigField::Subscription => {
            app.open_subscription_dropdown(config);
        }
        ProxyConfigField::Mode => {
            app.open_mode_dropdown(config);
        }
        ProxyConfigField::ProxyGroups => {
            if proxy_mode(config, app).eq_ignore_ascii_case("global") {
                app.proxy_pane = ProxyPane::Proxies;
                app.selected_proxy = 0;
                app.enter_page(Page::ProxyGroups, "Choose a proxy server");
            } else {
                app.proxy_pane = ProxyPane::Groups;
                app.enter_page(Page::ProxyGroups, "Choose a group, then choose a proxy");
            }
        }
        ProxyConfigField::LocalPort => match main_proxy_kind(config, app.selected_main) {
            MainProxyKind::System => begin_mixed_port_input(config, app),
            MainProxyKind::Http => begin_http_port_input(config, app),
            MainProxyKind::Socks => begin_socks_port_input(config, app),
            MainProxyKind::Service => begin_service_port_input(config, app),
            MainProxyKind::AddPortProxy => add_port_proxy(paths, config, app).await?,
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
                config.port_allocation.auto_controller = false;
                config.save(paths).await?;
                app.status = "Controller saved".into();
                app.refresh_runtime(config).await;
            }
            app.input.clear();
            app.input_mode = InputMode::Normal;
        }
        InputMode::MixedPort => {
            let Some((host, port)) =
                parse_listen_port_or_alert(app, &value, &config.proxy_host, "mixed port")
            else {
                return Ok(());
            };
            config.proxy_host = host;
            config.mixed_port = port;
            config.port_allocation.auto_mixed = false;
            config.save(paths).await?;
            reload_current_runtime(
                paths,
                config,
                app,
                format!("MIX={}:{}", config.proxy_host, config.mixed_port),
            )
            .await;
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
        InputMode::ServicePort => {
            let Some(service_index) = service_index_for_main_proxy(config, app.selected_main)
            else {
                app.input.clear();
                app.input_mode = InputMode::Normal;
                app.alert("Invalid Proxy", "Select a port proxy first.");
                return Ok(());
            };
            let current_listen = config
                .proxy_ports
                .services
                .get(service_index)
                .map(|service| service.listen.clone())
                .unwrap_or_else(|| "127.0.0.1".into());
            let Some((listen_host, port)) =
                parse_service_listen_port_or_alert(app, &value, &current_listen)
            else {
                return Ok(());
            };
            let Some(service) = config.proxy_ports.services.get_mut(service_index) else {
                app.input.clear();
                app.input_mode = InputMode::Normal;
                app.alert("Invalid Proxy", "Select a port proxy first.");
                return Ok(());
            };
            service.listen = listen_host;
            service.port = port;
            let listen = service_listen(service);
            config.save(paths).await?;
            app.status = format!("Port proxy={listen} saved; daemon will apply");
            app.input.clear();
            app.input_mode = InputMode::Normal;
        }
        InputMode::DnsListen => {
            if !value.is_empty() {
                config.dns.listen = value;
                config.port_allocation.auto_dns = false;
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
    app.input = format!("{}:{}", config.proxy_host, config.mixed_port);
    app.status = "Edit mixed listen address; use 0.0.0.0:7070 for LAN".into();
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

fn begin_service_port_input(config: &AppConfig, app: &mut ConfigApp) {
    app.input_mode = InputMode::ServicePort;
    app.input = service_index_for_main_proxy(config, app.selected_main)
        .and_then(|index| config.proxy_ports.services.get(index))
        .map(service_listen)
        .unwrap_or_else(|| "auto".into());
    app.status = "Edit port proxy listen address; empty/auto uses allocator".into();
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

async fn toggle_selected_main_proxy(
    paths: &Paths,
    config: &mut AppConfig,
    app: &mut ConfigApp,
) -> Result<()> {
    if app.page != Page::Main && app.page != Page::ProxyConfig {
        app.status = "Quick toggle is available on Main and proxy config pages".into();
        return Ok(());
    }

    match main_proxy_kind(config, app.selected_main) {
        MainProxyKind::System => toggle_system_proxy(paths, config, app).await?,
        MainProxyKind::Http => {
            config.proxy_ports.http = None;
            config.save(paths).await?;
            app.status = "HTTP port=off".into();
        }
        MainProxyKind::Socks => {
            config.proxy_ports.socks = None;
            config.save(paths).await?;
            app.status = "SOCKS port=off".into();
        }
        MainProxyKind::Service => {
            let Some(service_index) = service_index_for_main_proxy(config, app.selected_main)
            else {
                app.alert("Invalid Proxy", "Select a port proxy first.");
                return Ok(());
            };
            let Some(service) = config.proxy_ports.services.get_mut(service_index) else {
                app.alert("Invalid Proxy", "Select a port proxy first.");
                return Ok(());
            };
            service.enabled = !service.enabled;
            let enabled = service.enabled;
            let name = service_name(service);
            config.save(paths).await?;
            app.status = format!("{name}={}", on_off(enabled));
        }
        MainProxyKind::AddPortProxy => {
            app.status = "Press Enter to add a port proxy".into();
        }
    }

    app.clamp_selection(paths, config);
    Ok(())
}

async fn add_port_proxy(paths: &Paths, config: &mut AppConfig, app: &mut ConfigApp) -> Result<()> {
    let service_index = config.proxy_ports.services.len();
    let service = PortProxyService {
        name: format!("Port Proxy {}", service_index + 1),
        port: next_port_proxy_port(config),
        subscription: config.active_profile.clone(),
        mode: "global".into(),
        ..PortProxyService::default()
    };
    config.proxy_ports.services.push(service);
    config.save(paths).await?;

    app.selected_main = main_proxy_index_for_service(config, service_index).unwrap_or(0);
    app.selected_proxy_field = 0;
    app.enter_page(
        Page::ProxyConfig,
        format!("Added {}", main_proxy_name(config, app.selected_main)),
    );
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
        last_error: None,
        user_info: Default::default(),
        rule_selections: Default::default(),
    });
    config.active_profile = Some(name.clone());
    app.selected_subscription = config.subscriptions.len() - 1;
    app.input.clear();
    app.input_mode = InputMode::Normal;
    app.subscription_form.clear();

    match subscription::update_preserving_last_good(paths, config, app.selected_subscription).await
    {
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

async fn update_selected_subscription(
    paths: &Paths,
    config: &mut AppConfig,
    app: &mut ConfigApp,
) -> Result<()> {
    let index = app.selected_subscription;
    let Some(sub) = config.subscriptions.get(index) else {
        app.alert("No Subscription", "Select a subscription first.");
        return Ok(());
    };
    let name = sub.name.clone();

    match subscription::update_preserving_last_good(paths, config, index).await {
        Ok(path) => {
            config.save(paths).await?;
            app.status = format!(
                "Updated {}; profile={}",
                display_name(&name),
                path.display()
            );
        }
        Err(err) => {
            config.save(paths).await?;
            app.status = format!(
                "Update failed for {}; last good kept: {err}",
                display_name(&name)
            );
        }
    }
    Ok(())
}

async fn delete_selected_subscription(
    paths: &Paths,
    config: &mut AppConfig,
    app: &mut ConfigApp,
) -> Result<()> {
    let index = app.selected_subscription;
    if index >= config.subscriptions.len() {
        app.alert("No Subscription", "Select a subscription first.");
        return Ok(());
    }

    let removed = config.subscriptions.remove(index);
    let profile = subscription::profile_path(paths, &removed);
    let mut status = format!("Deleted {}", display_name(&removed.name));
    if let Err(err) = tokio::fs::remove_file(&profile).await
        && err.kind() != std::io::ErrorKind::NotFound
    {
        status = format!("{status}; profile removal failed: {err}");
    }

    if config.active_profile.as_ref() == Some(&removed.name) {
        config.active_profile = config
            .subscriptions
            .first()
            .map(|subscription| subscription.name.clone());
    }
    app.selected_subscription = app
        .selected_subscription
        .min(subscription_menu_count(config).saturating_sub(1));
    app.selected_subscription_detail = 0;
    config.save(paths).await?;

    app.return_to_previous_or(Page::Subscription, status);
    Ok(())
}

async fn test_selected_subscription_proxy(
    paths: &Paths,
    config: &mut AppConfig,
    app: &mut ConfigApp,
) -> Result<()> {
    if !ensure_selected_subscription_loaded_for_check(paths, config, app).await? {
        return Ok(());
    };
    let Some(subscription) = selected_subscription(config, app).cloned() else {
        app.alert("No Subscription", "Select a subscription first.");
        return Ok(());
    };

    let proxies = subscription_proxy_names_for_view(paths, config, app);
    if app.selected_subscription_proxy >= proxies.len() {
        test_all_subscription_proxies(paths, config, app).await?;
        return Ok(());
    }
    let Some(proxy) = proxies.get(app.selected_subscription_proxy) else {
        app.alert("No Proxy", "No proxy node selected.");
        return Ok(());
    };
    let Some(client) = runtime_mihomo_client(paths, config, app).await else {
        return Ok(());
    };
    match client
        .proxy_delay(proxy, DEFAULT_DELAY_TEST_URL, DEFAULT_DELAY_TEST_TIMEOUT_MS)
        .await
    {
        Ok(delay) => {
            app.proxy_delays.insert(
                subscription_proxy_delay_key(&subscription, proxy),
                format!("{delay}ms"),
            );
            app.status = format!("{} {}ms", display_name(proxy), delay);
        }
        Err(err) => {
            app.proxy_delays.insert(
                subscription_proxy_delay_key(&subscription, proxy),
                "fail".into(),
            );
            app.status = format!("{} check failed: {err}", display_name(proxy));
        }
    }
    Ok(())
}

async fn test_all_subscription_proxies(
    paths: &Paths,
    config: &mut AppConfig,
    app: &mut ConfigApp,
) -> Result<()> {
    if !ensure_selected_subscription_loaded_for_check(paths, config, app).await? {
        return Ok(());
    }
    let Some(subscription) = selected_subscription(config, app).cloned() else {
        app.alert("No Subscription", "Select a subscription first.");
        return Ok(());
    };
    let proxies = subscription_proxy_names_for_view(paths, config, app);
    if proxies.is_empty() {
        app.status = "No proxies to check".into();
        return Ok(());
    }
    let Some(client) = runtime_mihomo_client(paths, config, app).await else {
        return Ok(());
    };
    app.start_delay_check(subscription.name, client, proxies);
    Ok(())
}

async fn run_delay_check_task(
    sender: mpsc::Sender<DelayCheckEvent>,
    subscription_name: String,
    client: MihomoClient,
    proxies: Vec<String>,
) {
    for (index, proxy) in proxies.into_iter().enumerate() {
        if sender
            .send(DelayCheckEvent::Checking {
                index: index + 1,
                proxy: proxy.clone(),
            })
            .is_err()
        {
            return;
        }

        let delay = client
            .proxy_delay(
                &proxy,
                DEFAULT_DELAY_TEST_URL,
                DEFAULT_DELAY_TEST_TIMEOUT_MS,
            )
            .await
            .map_err(|err| err.to_string());
        if sender
            .send(DelayCheckEvent::ProxyFinished {
                subscription_name: subscription_name.clone(),
                proxy,
                delay,
            })
            .is_err()
        {
            return;
        }
    }

    let _ = sender.send(DelayCheckEvent::Finished);
}

async fn ensure_selected_subscription_loaded_for_check(
    paths: &Paths,
    config: &mut AppConfig,
    app: &mut ConfigApp,
) -> Result<bool> {
    let Some(subscription) = selected_subscription(config, app).cloned() else {
        app.alert("No Subscription", "Select a subscription first.");
        return Ok(false);
    };

    if !is_selected_subscription_active(config, &subscription) {
        config.active_profile = Some(subscription.name.clone());
        config.save(paths).await?;
        load_selected_profile(paths, config, app).await;
    }

    Ok(true)
}

async fn load_selected_profile(paths: &Paths, config: &AppConfig, app: &mut ConfigApp) {
    let Some(sub) = config.subscriptions.get(app.selected_subscription) else {
        return;
    };
    let Some(client) = runtime_mihomo_client(paths, config, app).await else {
        return;
    };
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
    let Some(client) = runtime_mihomo_client(paths, config, app).await else {
        return false;
    };
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

async fn writable_mihomo_client(
    paths: &Paths,
    config: &AppConfig,
    app: &mut ConfigApp,
) -> Option<MihomoClient> {
    let client = MihomoClient::new(&config.controller);
    match core::ensure_controller_is_owned(paths, config, &client).await {
        Ok(()) => Some(client),
        Err(err) => {
            app.status = format!("Runtime write blocked: {err}");
            app.alert(
                "External Mihomo",
                "This controller is not owned by clashtui.",
            );
            None
        }
    }
}

async fn runtime_mihomo_client(
    paths: &Paths,
    config: &AppConfig,
    app: &mut ConfigApp,
) -> Option<MihomoClient> {
    let client = writable_mihomo_client(paths, config, app).await?;
    if client.version().await.is_ok() {
        return Some(client);
    }

    if let Err(err) = core::ensure_running(paths, config).await {
        app.status = format!("Mihomo start failed: {err:#}");
        app.alert(
            "Mihomo Offline",
            "Set core_path or install mihomo, then retry.",
        );
        return None;
    }

    for _ in 0..RUNTIME_START_RETRIES {
        if client.version().await.is_ok() {
            return Some(client);
        }
        sleep(RUNTIME_START_WAIT).await;
    }

    app.status = "Mihomo runtime did not become ready".into();
    None
}

async fn set_runtime_mode(
    paths: &Paths,
    config: &mut AppConfig,
    app: &mut ConfigApp,
    mode: &str,
) -> Result<()> {
    config.runtime_mode = mode.to_string();
    config.save(paths).await?;
    let Some(client) = runtime_mihomo_client(paths, config, app).await else {
        app.status = format!("Mode={mode} saved; runtime not patched");
        return Ok(());
    };
    match client.set_mode(mode).await {
        Ok(()) => {
            app.status = format!("Mode={mode} saved");
            app.refresh_runtime(config).await;
        }
        Err(err) => app.status = format!("Mode saved; runtime patch failed: {err}"),
    }
    Ok(())
}

async fn set_selected_proxy_mode(
    paths: &Paths,
    config: &mut AppConfig,
    app: &mut ConfigApp,
    mode: &str,
) -> Result<()> {
    match main_proxy_kind(config, app.selected_main) {
        MainProxyKind::System => set_runtime_mode(paths, config, app, mode).await,
        MainProxyKind::Service => {
            let Some(service_index) = service_index_for_main_proxy(config, app.selected_main)
            else {
                app.alert("Invalid Proxy", "Select a port proxy first.");
                return Ok(());
            };
            let Some(service) = config.proxy_ports.services.get_mut(service_index) else {
                app.alert("Invalid Proxy", "Select a port proxy first.");
                return Ok(());
            };
            service.mode = mode.to_string();
            config.save(paths).await?;
            app.status = format!("Mode={mode} saved; daemon will apply");
            Ok(())
        }
        MainProxyKind::Http | MainProxyKind::Socks => {
            set_runtime_mode(paths, config, app, mode).await
        }
        MainProxyKind::AddPortProxy => Ok(()),
    }
}

async fn set_selected_proxy_subscription(
    paths: &Paths,
    config: &mut AppConfig,
    app: &mut ConfigApp,
    index: usize,
) -> Result<()> {
    match main_proxy_kind(config, app.selected_main) {
        MainProxyKind::System | MainProxyKind::Http | MainProxyKind::Socks => {
            activate_subscription(paths, config, app, index).await
        }
        MainProxyKind::Service => {
            let name = config.subscriptions[index].name.clone();
            let Some(service_index) = service_index_for_main_proxy(config, app.selected_main)
            else {
                app.alert("Invalid Proxy", "Select a port proxy first.");
                return Ok(());
            };
            let Some(service) = config.proxy_ports.services.get_mut(service_index) else {
                app.alert("Invalid Proxy", "Select a port proxy first.");
                return Ok(());
            };
            service.subscription = Some(name.clone());
            config.save(paths).await?;
            app.status = format!("Subscription={}", display_name(&name));
            Ok(())
        }
        MainProxyKind::AddPortProxy => Ok(()),
    }
}

async fn select_proxy(paths: &Paths, config: &mut AppConfig, app: &mut ConfigApp) -> Result<()> {
    let mode = proxy_mode(config, app).to_string();
    let (group_name, proxy_name) = if mode.eq_ignore_ascii_case("global") {
        let proxies = route_proxy_names(paths, config, app);
        let Some(proxy) = proxies.get(app.selected_proxy) else {
            app.status = "No proxy selected".into();
            return Ok(());
        };
        ("GLOBAL".to_string(), proxy.clone())
    } else {
        let Some(group) = app.current_group() else {
            app.status = "No proxy group selected".into();
            return Ok(());
        };
        let Some(proxy) = group.all.get(app.selected_proxy) else {
            app.status = "No proxy selected".into();
            return Ok(());
        };
        (group.name.clone(), proxy.clone())
    };
    let selection_scope = save_selected_proxy_choice(config, app, &mode, &group_name, &proxy_name);
    config.save(paths).await?;
    if matches!(
        main_proxy_kind(config, app.selected_main),
        MainProxyKind::Service
    ) {
        app.status = format!(
            "{} saved ({selection_scope}); daemon will apply",
            display_name(&proxy_name)
        );
        return Ok(());
    }
    let Some(client) = runtime_mihomo_client(paths, config, app).await else {
        app.status = format!(
            "{} -> {} saved; runtime not patched",
            display_name(&group_name),
            display_name(&proxy_name)
        );
        return Ok(());
    };
    let runtime_group = if mode.eq_ignore_ascii_case("global") {
        "GLOBAL"
    } else {
        group_name.as_str()
    };
    match client.select_proxy(runtime_group, &proxy_name).await {
        Ok(()) => {
            app.status = format!(
                "{} -> {} saved ({selection_scope})",
                display_name(runtime_group),
                display_name(&proxy_name)
            );
            app.refresh_runtime(config).await;
        }
        Err(err) if runtime_group == "GLOBAL" => {
            let fallback_group =
                runtime_group_for_proxy(app, &proxy_name).unwrap_or_else(|| group_name.clone());
            match client.select_proxy(&fallback_group, &proxy_name).await {
                Ok(()) => {
                    app.status = format!(
                        "{} -> {} saved ({selection_scope})",
                        display_name(&fallback_group),
                        display_name(&proxy_name)
                    );
                    app.refresh_runtime(config).await;
                }
                Err(fallback_err) => {
                    app.status = format!(
                        "Proxy selection saved; runtime patch failed: {err}; fallback failed: {fallback_err}"
                    );
                }
            }
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
    if app.delay_check.is_some() {
        draw_delay_check(frame, app);
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

    let visible_rows = area.height.saturating_sub(2) as usize;
    let start = visible_window_start(selected, rows.len(), visible_rows);
    for (index, row) in rows.iter().enumerate().skip(start).take(visible_rows) {
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
    _paths: &Paths,
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

    for (index, row) in rows.iter().enumerate() {
        let selected_row = index == selected;
        let first = if row.action {
            fit_width(&format!("  {}", row.name), content_width)
        } else {
            fit_width(
                &format!(
                    "> {:<3} {}={}  {}",
                    on_off_upper(row.enabled),
                    row.listener,
                    row.listen,
                    row.name
                ),
                content_width,
            )
        };
        let normal_style = if row.action {
            Style::default().fg(Color::Yellow)
        } else {
            Style::default().fg(Color::Cyan)
        };
        lines.push(with_padding(Line::from(Span::styled(
            first,
            if selected_row {
                selected_style()
            } else {
                normal_style
            },
        ))));

        if row.action {
            continue;
        }

        let second = fit_width(
            &format!("  {}", main_proxy_settings_line(config, row)),
            content_width,
        );
        lines.push(with_padding(Line::from(Span::styled(
            second,
            Style::default().fg(Color::Gray),
        ))));

        let third = fit_width(&format!("  {}", proxy_runtime_line(app)), content_width);
        lines.push(with_padding(Line::from(Span::styled(
            third,
            Style::default().fg(Color::DarkGray),
        ))));
    }

    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

fn draw_help(frame: &mut Frame, paths: &Paths, config: &AppConfig, app: &ConfigApp, area: Rect) {
    const HOTKEY_ROWS: u16 = 7;
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
        if row.action {
            lines.extend([
                Line::from(Span::styled(row.name, Style::default().fg(Color::Cyan))),
                Line::from(""),
                Line::from("Action: add port proxy"),
                Line::from("Kind: mixed"),
                Line::from("Port: auto"),
                Line::from("Enter: create and edit"),
            ]);
        } else {
            lines.extend([
                Line::from(Span::styled(row.name, Style::default().fg(Color::Cyan))),
                Line::from(""),
                Line::from(format!("Kind: {}", row.kind)),
                Line::from(format!("State: {}", on_off_upper(row.enabled))),
                Line::from(format!("Listener: {}={}", row.listener, row.listen)),
                Line::from(format!("Port: {}", row.port)),
                Line::from(format!("Mode: {}", row.mode)),
                Line::from(format!("Subscription: {}", row.subscription)),
                Line::from(format!("Config: {}", row.features)),
                Line::from(format!(
                    "Traffic: {}",
                    traffic_speed_summary(app.runtime.traffic.as_ref())
                )),
                Line::from(format!(
                    "Total: {}",
                    traffic_total_summary(app.runtime.traffic.as_ref())
                )),
                Line::from(format!("IP: {}", ip_info_summary(app))),
            ]);
        }
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
        Line::from("O quick on/off"),
        Line::from("Esc back one page"),
        Line::from("←→/Tab switch section"),
        Line::from("F10 save & restart"),
    ];
    frame.render_widget(
        Paragraph::new(with_horizontal_padding(hotkeys)).wrap(Wrap { trim: false }),
        chunks[1],
    );
}

fn setting_rows(paths: &Paths, config: &AppConfig, app: &ConfigApp) -> Vec<SettingRow> {
    match app.page {
        Page::Main => main_proxy_rows(config)
            .into_iter()
            .map(|row| SettingRow {
                label: row.name,
                value: if row.action { String::new() } else { row.port },
                help: row.features,
                kind: if row.action {
                    RowKind::Action(ActionKind::AddPortProxy)
                } else {
                    RowKind::Submenu(Page::ProxyConfig)
                },
            })
            .collect(),
        Page::Subscription => subscription_rows(config),
        Page::SubscriptionDetail => subscription_detail_rows(paths, config, app),
        Page::SubscriptionRuleGroups => subscription_rule_group_rows(config, app),
        Page::SubscriptionProxies => subscription_proxy_rows(paths, config, app),
        Page::SubscriptionRules => subscription_rule_rows(paths, config, app),
        Page::AddSubscription => add_subscription_rows(app),
        Page::Runtime => runtime_rows(config, app),
        Page::Chat => chat_rows(config, app),
        Page::Help => help_rows(),
        Page::ProxyConfig => proxy_config_rows(config, app),
        Page::ProxyGroups => proxy_group_rows(paths, config, app),
        Page::Mode => mode_rows(config, app),
        Page::Dns => dns_rows(config),
    }
}

fn subscription_rows(config: &AppConfig) -> Vec<SettingRow> {
    let mut rows = config
        .subscriptions
        .iter()
        .map(|subscription| SettingRow {
            label: display_name(&subscription.name),
            value: subscription_usage_summary(subscription),
            help: subscription_list_help(subscription),
            kind: RowKind::Submenu(Page::SubscriptionDetail),
        })
        .collect::<Vec<_>>();

    rows.push(SettingRow {
        label: "Add Subscription".into(),
        value: String::new(),
        help: "Open a child page for name, URL, refresh interval, and OK.".into(),
        kind: RowKind::Action(ActionKind::AddSubscription),
    });
    rows
}

fn subscription_detail_rows(paths: &Paths, config: &AppConfig, app: &ConfigApp) -> Vec<SettingRow> {
    let Some(subscription) = selected_subscription(config, app) else {
        return vec![SettingRow {
            label: "No Subscription".into(),
            value: "-".into(),
            help: "Go back and add a subscription first.".into(),
            kind: RowKind::Info,
        }];
    };
    let profile = subscription::profile_path(paths, subscription);
    let summary = subscription_profile_summary(paths, subscription);
    vec![
        SettingRow {
            label: "Overview".into(),
            value: format!(
                "{}  {}",
                subscription_usage_summary(subscription),
                summary.short_counts()
            ),
            help: format!(
                "URL: {} | profile: {} | updated: {}",
                subscription.url,
                profile.display(),
                subscription.updated_at.as_deref().unwrap_or("-")
            ),
            kind: RowKind::Info,
        },
        SettingRow {
            label: "Rule Groups".into(),
            value: format!(
                "{} saved / {} runtime",
                subscription.rule_selections.len(),
                app.runtime.groups.len()
            ),
            help: "Rule-mode group selections belong to the subscription, so multiple proxies can share them.".into(),
            kind: RowKind::Submenu(Page::SubscriptionRuleGroups),
        },
        SettingRow {
            label: "Proxies".into(),
            value: format!("{} nodes", summary.proxies),
            help: "Inspect proxy nodes from the local subscription profile; delay testing uses mihomo runtime.".into(),
            kind: RowKind::Submenu(Page::SubscriptionProxies),
        },
        SettingRow {
            label: "Rules".into(),
            value: summary.rules_value(),
            help: "Read-only overview of rules and providers in the local subscription profile.".into(),
            kind: RowKind::Submenu(Page::SubscriptionRules),
        },
        SettingRow {
            label: "Update Now".into(),
            value: subscription.updated_at.as_deref().unwrap_or("never").to_string(),
            help: "Refresh from URL. Failure records last_error and keeps the existing profile file.".into(),
            kind: RowKind::Action(ActionKind::UpdateSubscription),
        },
        SettingRow {
            label: "Edit".into(),
            value: "pending".into(),
            help: "Edit name, URL, and refresh cadence. This will reuse the Add Subscription form model.".into(),
            kind: RowKind::Action(ActionKind::EditSubscription),
        },
        SettingRow {
            label: "Delete".into(),
            value: "confirm".into(),
            help: "Delete this subscription and its local profile file after confirmation.".into(),
            kind: RowKind::Action(ActionKind::DeleteSubscription),
        },
    ]
}

fn subscription_rule_group_rows(config: &AppConfig, app: &ConfigApp) -> Vec<SettingRow> {
    let Some(subscription) = selected_subscription(config, app) else {
        return empty_rows("No Subscription", "Go back and add a subscription first.");
    };

    if !is_selected_subscription_active(config, subscription) {
        if subscription.rule_selections.is_empty() {
            return vec![SettingRow {
                label: "Runtime View".into(),
                value: "inactive".into(),
                help:
                    "Load this subscription through Proxy settings to inspect runtime rule groups."
                        .into(),
                kind: RowKind::Info,
            }];
        }
        return subscription_rule_selection_rows(subscription);
    }

    if app.runtime.groups.is_empty() && subscription.rule_selections.is_empty() {
        return empty_rows(
            "No Groups",
            "Start mihomo and refresh runtime, or update this subscription profile first.",
        );
    }

    if app.runtime.groups.is_empty() {
        return subscription_rule_selection_rows(subscription);
    }

    app.runtime
        .groups
        .iter()
        .map(|group| {
            let saved = subscription
                .rule_selections
                .get(&group.name)
                .map_or_else(|| display_name(&group.now), |value| display_name(value));
            SettingRow {
                label: display_name(&group.name),
                value: format!("{} -> {}", group.kind, saved),
                help: "Rule mode uses this subscription-level group choice. Select concrete nodes from Proxy Groups.".into(),
                kind: RowKind::Info,
            }
        })
        .collect()
}

fn subscription_rule_selection_rows(subscription: &Subscription) -> Vec<SettingRow> {
    subscription
        .rule_selections
        .iter()
        .map(|(group, proxy)| SettingRow {
            label: display_name(group),
            value: display_name(proxy),
            help: "Saved subscription-level rule-mode selection.".into(),
            kind: RowKind::Info,
        })
        .collect()
}

fn subscription_proxy_rows(paths: &Paths, config: &AppConfig, app: &ConfigApp) -> Vec<SettingRow> {
    let Some(subscription) = selected_subscription(config, app) else {
        return empty_rows("No Subscription", "Go back and add a subscription first.");
    };
    let proxies = subscription_proxy_names_for_view(paths, config, app);
    if proxies.is_empty() {
        return empty_rows(
            "No Proxies",
            "No proxy nodes found in the local subscription profile.",
        );
    }

    let mut rows = proxies
        .into_iter()
        .map(|proxy| SettingRow {
            value: app
                .proxy_delays
                .get(&subscription_proxy_delay_key(subscription, &proxy))
                .cloned()
                .unwrap_or_else(|| {
                    if is_selected_subscription_active(config, subscription) {
                        "Check".into()
                    } else {
                        "profile".into()
                    }
                }),
            label: display_name(&proxy),
            help: if is_selected_subscription_active(config, subscription) {
                "Runs mihomo /proxies/{name}/delay against the default generate_204 URL.".into()
            } else {
                "Loaded from the local subscription profile. Select this subscription and start runtime before testing delay.".into()
            },
            kind: RowKind::Action(ActionKind::TestProxyDelay),
        })
        .collect::<Vec<_>>();

    rows.push(SettingRow {
        label: "Check All".into(),
        value: format!("{} nodes", rows.len()),
        help: "Run delay checks for every proxy in this subscription.".into(),
        kind: RowKind::Action(ActionKind::TestProxyDelay),
    });
    rows
}

fn subscription_rule_rows(paths: &Paths, config: &AppConfig, app: &ConfigApp) -> Vec<SettingRow> {
    let Some(subscription) = selected_subscription(config, app) else {
        return empty_rows("No Subscription", "Go back and add a subscription first.");
    };
    let summary = subscription_profile_summary(paths, subscription);
    vec![
        SettingRow {
            label: "Rules".into(),
            value: summary.rules.to_string(),
            help: "Number of rules in the local subscription profile.".into(),
            kind: RowKind::Info,
        },
        SettingRow {
            label: "Rule Providers".into(),
            value: summary.rule_providers.to_string(),
            help: "Number of rule providers in the local profile.".into(),
            kind: RowKind::Info,
        },
        SettingRow {
            label: "Proxy Groups".into(),
            value: summary.proxy_groups.to_string(),
            help: "Number of proxy groups owned by the subscription profile.".into(),
            kind: RowKind::Info,
        },
        SettingRow {
            label: "Raw Proxies".into(),
            value: summary.proxies.to_string(),
            help: "Number of concrete proxy entries in the subscription profile.".into(),
            kind: RowKind::Info,
        },
        SettingRow {
            label: "Profile".into(),
            value: if summary.exists {
                "local".into()
            } else {
                "missing".into()
            },
            help: subscription::profile_path(paths, subscription)
                .display()
                .to_string(),
            kind: RowKind::Info,
        },
    ]
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
                        RowKind::Toggle(ToggleAction::PortProxy)
                    }
                }
                ProxyConfigField::Subscription => RowKind::Choice(ChoiceAction::Subscription),
                ProxyConfigField::Mode => RowKind::Choice(ChoiceAction::Mode),
                ProxyConfigField::ProxyGroups => RowKind::Submenu(Page::ProxyGroups),
                ProxyConfigField::LocalPort => match main_proxy_kind(config, app.selected_main) {
                    MainProxyKind::System => RowKind::Input(InputMode::MixedPort),
                    MainProxyKind::Http => RowKind::Input(InputMode::HttpPort),
                    MainProxyKind::Socks => RowKind::Input(InputMode::SocksPort),
                    MainProxyKind::Service => RowKind::Input(InputMode::ServicePort),
                    MainProxyKind::AddPortProxy => RowKind::Action(ActionKind::AddPortProxy),
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

fn proxy_group_rows(paths: &Paths, config: &AppConfig, app: &ConfigApp) -> Vec<SettingRow> {
    if proxy_groups_page_is_global_proxy_list(config, app) {
        let proxies = route_proxy_names(paths, config, app);
        if proxies.is_empty() {
            return empty_rows(
                "No Proxies",
                "Update the selected subscription or start runtime before choosing a proxy.",
            );
        }
        let saved = selected_global_proxy(config, app);
        return proxies
            .into_iter()
            .map(|proxy| SettingRow {
                label: display_name(&proxy),
                value: if saved.as_deref() == Some(proxy.as_str()) {
                    "selected".into()
                } else {
                    "select".into()
                },
                help: "Enter saves this proxy for global mode.".into(),
                kind: RowKind::Action(ActionKind::SelectProxy),
            })
            .collect();
    }

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
    let saved_mode = proxy_mode(config, app);
    ModeItem::ALL
        .iter()
        .map(|mode| SettingRow {
            label: mode.label().into(),
            value: if mode.value().eq_ignore_ascii_case(saved_mode) {
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
        Page::SubscriptionDetail => "profile detail / maintenance",
        Page::SubscriptionRuleGroups => "rule-mode group selections",
        Page::SubscriptionProxies => "proxy nodes / delay checks",
        Page::SubscriptionRules => "rules / providers / raw profile",
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
            "Sys Proxy={}  TUN={}  DNS={}  Mode desired={} runtime={}",
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
                MainProxyKind::AddPortProxy => "create".into(),
                _ => row.features.clone(),
            };
            ListItem::new(vec![
                Line::from(Span::styled(
                    format!("{:<18} {:<7} {}", row.name, row.kind, row.listen),
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
            Line::from(format!("State: {}", on_off_upper(row.enabled))),
            Line::from(row.features),
        ]),
        MainProxyKind::AddPortProxy => lines.extend([
            Line::from("Action: add port proxy"),
            Line::from("Enter: create and configure"),
        ]),
        _ => lines.extend([
            Line::from(format!("State: {}", on_off_upper(row.enabled))),
            Line::from(format!("Listener: {}={}", row.listener, row.listen)),
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
                SubscriptionAction::Open(_) => "Action: Open Subscription",
            }),
            Line::from(match action {
                SubscriptionAction::Add => "The form collects name, URL, and refresh interval.",
                SubscriptionAction::Open(_) => "Opens maintenance details for this subscription.",
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
                desired_proxy_selection(config, app, &group.name)
                    .map_or_else(|| "-".into(), display_name)
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
        lines.push(Line::from("TUN is only configured on Global Proxy."));
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
        Line::from(format!("Saved mode: {}", proxy_mode(config, app))),
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

fn draw_delay_check(frame: &mut Frame, app: &ConfigApp) {
    let Some(task) = &app.delay_check else {
        return;
    };

    let progress = &task.progress;
    let area = fixed_rect(56, 8, frame.area());
    frame.render_widget(Clear, area);

    let percent = progress_percent(progress.done, progress.total);
    let bar = progress_bar(
        progress.done,
        progress.total,
        area.width.saturating_sub(12) as usize,
    );
    let action = if progress.finished {
        "Enter OK | Esc close"
    } else {
        "Esc cancel"
    };
    let lines = vec![
        popup_title_line(&progress.title, area.width.saturating_sub(2)),
        Line::from(""),
        Line::from(fit_width(
            &progress.current,
            area.width.saturating_sub(4) as usize,
        )),
        Line::from(vec![
            Span::styled(bar, Style::default().fg(Color::Cyan)),
            Span::raw(format!(" {percent:>3}%")),
        ]),
        Line::from(format!(
            "{}/{}  ok={}  fail={}",
            progress.done, progress.total, progress.ok, progress.failed
        ))
        .alignment(Alignment::Center),
        Line::from(Span::styled(action, Style::default().fg(Color::Gray)))
            .alignment(Alignment::Center),
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

fn progress_percent(done: usize, total: usize) -> usize {
    done.saturating_mul(100).checked_div(total).unwrap_or(100)
}

fn progress_bar(done: usize, total: usize, width: usize) -> String {
    let width = width.max(8);
    let filled = done
        .saturating_mul(width)
        .checked_div(total)
        .unwrap_or(width)
        .min(width);
    format!("[{}{}]", "█".repeat(filled), "░".repeat(width - filled))
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
    Open(usize),
    Add,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum MainProxyKind {
    System,
    Http,
    Socks,
    Service,
    AddPortProxy,
}

struct MainProxyRow {
    name: String,
    kind: String,
    listener: String,
    port: String,
    listen: String,
    mode: String,
    subscription: String,
    features: String,
    enabled: bool,
    action: bool,
}

fn subscription_menu_count(config: &AppConfig) -> usize {
    config.subscriptions.len() + 1
}

fn subscription_dropdown_count(config: &AppConfig) -> usize {
    config.subscriptions.len().max(1)
}

const SUBSCRIPTION_DETAIL_COUNT: usize = 7;
const SUBSCRIPTION_RULE_FALLBACK_COUNT: usize = 5;

const fn subscription_detail_count() -> usize {
    SUBSCRIPTION_DETAIL_COUNT
}

fn subscription_rule_group_count(config: &AppConfig, app: &ConfigApp) -> usize {
    let saved = selected_subscription(config, app)
        .map(|subscription| subscription.rule_selections.len())
        .unwrap_or_default();
    app.runtime.groups.len().max(saved).max(1)
}

fn subscription_proxy_count(paths: &Paths, config: &AppConfig, app: &ConfigApp) -> usize {
    let count = subscription_proxy_names_for_view(paths, config, app).len();
    if count > 0 { count + 1 } else { 1 }
}

fn selected_subscription_action(config: &AppConfig, app: &ConfigApp) -> SubscriptionAction {
    let index = app.selected_subscription;
    if index < config.subscriptions.len() {
        return SubscriptionAction::Open(index);
    }

    SubscriptionAction::Add
}

fn selected_subscription<'a>(config: &'a AppConfig, app: &ConfigApp) -> Option<&'a Subscription> {
    config.subscriptions.get(app.selected_subscription)
}

fn active_subscription(config: &AppConfig) -> Option<&Subscription> {
    let active = config.active_profile.as_ref()?;
    config
        .subscriptions
        .iter()
        .find(|subscription| &subscription.name == active)
}

fn active_subscription_mut(config: &mut AppConfig) -> Option<&mut Subscription> {
    let active = config.active_profile.clone()?;
    config
        .subscriptions
        .iter_mut()
        .find(|subscription| subscription.name == active)
}

fn service_for_main_proxy<'a>(
    config: &'a AppConfig,
    app: &ConfigApp,
) -> Option<&'a PortProxyService> {
    let index = service_index_for_main_proxy(config, app.selected_main)?;
    config.proxy_ports.services.get(index)
}

fn service_for_main_proxy_mut(
    config: &mut AppConfig,
    selected_main: usize,
) -> Option<&mut PortProxyService> {
    let index = service_index_for_main_proxy(config, selected_main)?;
    config.proxy_ports.services.get_mut(index)
}

fn is_selected_subscription_active(config: &AppConfig, subscription: &Subscription) -> bool {
    config.active_profile.as_ref() == Some(&subscription.name)
}

fn proxy_mode<'a>(config: &'a AppConfig, app: &ConfigApp) -> &'a str {
    service_for_main_proxy(config, app)
        .map(|service| service.mode.as_str())
        .unwrap_or(config.runtime_mode.as_str())
}

fn proxy_subscription_name<'a>(config: &'a AppConfig, app: &ConfigApp) -> Option<&'a str> {
    service_for_main_proxy(config, app)
        .and_then(|service| service.subscription.as_deref())
        .or(config.active_profile.as_deref())
}

fn proxy_subscription_index(config: &AppConfig, app: &ConfigApp) -> Option<usize> {
    let name = proxy_subscription_name(config, app)?;
    config
        .subscriptions
        .iter()
        .position(|subscription| subscription.name == name)
}

fn mode_index(mode: &str) -> usize {
    ModeItem::ALL
        .iter()
        .position(|item| item.value().eq_ignore_ascii_case(mode))
        .unwrap_or_default()
}

fn uses_subscription_rule_selections(config: &AppConfig) -> bool {
    config.runtime_mode.eq_ignore_ascii_case("rule")
}

fn desired_proxy_selection<'a>(
    config: &'a AppConfig,
    app: &ConfigApp,
    group: &str,
) -> Option<&'a str> {
    if let Some(service) = service_for_main_proxy(config, app) {
        if service.mode.eq_ignore_ascii_case("rule")
            && let Some(selection) = service.rule_selections.get(group)
        {
            return Some(selection.as_str());
        }
        if service.mode.eq_ignore_ascii_case("global") {
            return service.proxy.as_deref();
        }
        return None;
    }

    if config.runtime_mode.eq_ignore_ascii_case("rule")
        && let Some(selection) = active_subscription(config)
            .and_then(|subscription| subscription.rule_selections.get(group))
    {
        return Some(selection.as_str());
    }
    if config.runtime_mode.eq_ignore_ascii_case("global")
        && let Some(selection) = config.proxy_selections.get("GLOBAL")
    {
        return Some(selection.as_str());
    }
    config.proxy_selections.get(group).map(String::as_str)
}

fn save_selected_proxy_choice(
    config: &mut AppConfig,
    app: &ConfigApp,
    mode: &str,
    group: &str,
    proxy: &str,
) -> String {
    if let Some(service) = service_for_main_proxy_mut(config, app.selected_main) {
        if mode.eq_ignore_ascii_case("rule") {
            service
                .rule_selections
                .insert(group.to_string(), proxy.to_string());
            service.rule = Some(group.to_string());
            return format!("{} rule", service_name(service));
        }

        if mode.eq_ignore_ascii_case("global") {
            service.proxy = Some(proxy.to_string());
            return format!("{} global", service_name(service));
        }

        service.proxy = Some("DIRECT".into());
        return format!("{} direct", service_name(service));
    }

    if mode.eq_ignore_ascii_case("rule") {
        if let Some(subscription) = active_subscription_mut(config) {
            subscription
                .rule_selections
                .insert(group.to_string(), proxy.to_string());
            return format!("{} rule", display_name(&subscription.name));
        }
        config
            .proxy_selections
            .insert(group.to_string(), proxy.to_string());
        return "global rule".into();
    }

    if mode.eq_ignore_ascii_case("global") {
        config
            .proxy_selections
            .insert("GLOBAL".into(), proxy.to_string());
        config
            .proxy_selections
            .insert(group.to_string(), proxy.to_string());
        return "global".into();
    }

    "direct".into()
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
        + 1
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
            name: "Global Proxy".into(),
            kind: "Global".into(),
            listener: "MIX".into(),
            port: config.mixed_port.to_string(),
            listen: format!("{}:{}", config.proxy_host, config.mixed_port),
            mode: config.runtime_mode.clone(),
            subscription,
            features: format!(
                "SYS={} TUN={} DNS={}",
                on_off_upper(config.system_proxy.enabled),
                on_off_upper(config.tun.enable),
                on_off_upper(config.dns.enable)
            ),
            enabled: config.system_proxy.enabled || config.tun.enable,
            action: false,
        },
        MainProxyKind::Http => MainProxyRow {
            name: "HTTP Proxy".into(),
            kind: "Port".into(),
            listener: "HTTP".into(),
            port: config.proxy_ports.http.unwrap_or_default().to_string(),
            listen: format!(
                "{}:{}",
                config.proxy_host,
                config.proxy_ports.http.unwrap_or_default()
            ),
            mode: config.runtime_mode.clone(),
            subscription,
            features: format!(
                "listener=http allow-lan={}",
                on_off(config.proxy_ports.allow_lan)
            ),
            enabled: true,
            action: false,
        },
        MainProxyKind::Socks => MainProxyRow {
            name: "SOCKS Proxy".into(),
            kind: "Port".into(),
            listener: "SOCKS".into(),
            port: config.proxy_ports.socks.unwrap_or_default().to_string(),
            listen: format!(
                "{}:{}",
                config.proxy_host,
                config.proxy_ports.socks.unwrap_or_default()
            ),
            mode: config.runtime_mode.clone(),
            subscription,
            features: format!(
                "listener=socks5 allow-lan={}",
                on_off(config.proxy_ports.allow_lan)
            ),
            enabled: true,
            action: false,
        },
        MainProxyKind::Service => {
            let service_index = service_index_for_main_proxy(config, index).unwrap_or_default();
            let service = config.proxy_ports.services.get(service_index);
            let service_subscription = service
                .and_then(|service| service.subscription.as_deref())
                .map_or_else(|| subscription.clone(), display_name);
            MainProxyRow {
                name: service
                    .map(service_name)
                    .unwrap_or_else(|| "Port Proxy".into()),
                kind: "Port".into(),
                listener: service
                    .map(|service| listener_label(&service.kind))
                    .unwrap_or_else(|| "PORT".into()),
                port: service
                    .map(service_port_value)
                    .unwrap_or_else(|| "-".into()),
                listen: service.map(service_listen).unwrap_or_else(|| "-".into()),
                mode: service
                    .map(|service| service.mode.clone())
                    .unwrap_or_else(|| "global".into()),
                subscription: service_subscription,
                features: service
                    .map(|service| {
                        let route = if service.mode.eq_ignore_ascii_case("global") {
                            service
                                .proxy
                                .as_deref()
                                .map_or("proxy=-", |proxy| proxy)
                                .to_string()
                        } else if service.mode.eq_ignore_ascii_case("rule") {
                            format!("groups={}", service.rule_selections.len())
                        } else {
                            "DIRECT".into()
                        };
                        format!("{} udp={}", route, on_off_upper(service.udp))
                    })
                    .unwrap_or_else(|| "custom".into()),
                enabled: service.map(|service| service.enabled).unwrap_or_default(),
                action: false,
            }
        }
        MainProxyKind::AddPortProxy => MainProxyRow {
            name: "Add Port Proxy".into(),
            kind: "Action".into(),
            listener: "MIX".into(),
            port: "auto".into(),
            listen: "new mixed listener".into(),
            mode: String::new(),
            subscription: "-".into(),
            features: "create a local port proxy".into(),
            enabled: false,
            action: true,
        },
    }
}

fn main_proxy_kind(config: &AppConfig, index: usize) -> MainProxyKind {
    if index == 0 {
        return MainProxyKind::System;
    }
    if index + 1 >= main_proxy_count(config) {
        return MainProxyKind::AddPortProxy;
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
    let service_index = index.checked_sub(cursor)?;
    (service_index < config.proxy_ports.services.len()).then_some(service_index)
}

fn main_proxy_index_for_service(config: &AppConfig, service_index: usize) -> Option<usize> {
    if service_index >= config.proxy_ports.services.len() {
        return None;
    }

    let mut cursor = 1;
    if config.proxy_ports.http.is_some() {
        cursor += 1;
    }
    if config.proxy_ports.socks.is_some() {
        cursor += 1;
    }
    Some(cursor + service_index)
}

fn service_name(service: &PortProxyService) -> String {
    if service.name.trim().is_empty() {
        format!("{} Port", service.kind)
    } else {
        service.name.clone()
    }
}

fn service_listen(service: &PortProxyService) -> String {
    let host = if service.listen.trim().is_empty() {
        "127.0.0.1"
    } else {
        service.listen.trim()
    };
    format!("{host}:{}", service_port_value(service))
}

fn service_port_value(service: &PortProxyService) -> String {
    if service.port == 0 {
        "auto".into()
    } else {
        service.port.to_string()
    }
}

fn next_port_proxy_port(config: &AppConfig) -> u16 {
    let mut port = 7071;
    loop {
        let used = config.proxy_ports.http == Some(port)
            || config.proxy_ports.socks == Some(port)
            || config
                .proxy_ports
                .services
                .iter()
                .any(|service| service.port == port)
            || config.mixed_port == port;
        if !used {
            return port;
        }
        port = port.saturating_add(1);
        if port == u16::MAX {
            return 0;
        }
    }
}

fn listener_label(kind: &str) -> String {
    match kind.trim().to_ascii_lowercase().as_str() {
        "mixed" | "mix" => "MIX".into(),
        "http" => "HTTP".into(),
        "socks" | "socks5" => "SOCKS".into(),
        value if !value.is_empty() => value.to_ascii_uppercase(),
        _ => "PORT".into(),
    }
}

fn main_proxy_name(config: &AppConfig, index: usize) -> String {
    main_proxy_row(config, index).name
}

fn main_proxy_settings_line(config: &AppConfig, row: &MainProxyRow) -> String {
    let mode = row.mode.to_ascii_uppercase();
    if row.kind == "Global" {
        format!(
            "{} {} SYS={} TUN={}",
            mode,
            row.subscription,
            on_off_upper(config.system_proxy.enabled),
            on_off_upper(config.tun.enable)
        )
    } else {
        format!("{mode} {} {}", row.subscription, row.features)
    }
}

fn proxy_server_value(config: &AppConfig, app: &ConfigApp) -> String {
    let mode = proxy_mode(config, app);
    if mode.eq_ignore_ascii_case("direct") {
        return "DIRECT".into();
    }

    if mode.eq_ignore_ascii_case("global") {
        if let Some(service) = service_for_main_proxy(config, app) {
            return service
                .proxy
                .as_deref()
                .map_or_else(|| "choose proxy".into(), display_name);
        }
        return config
            .proxy_selections
            .get("GLOBAL")
            .or_else(|| config.proxy_selections.values().next())
            .map_or_else(|| "choose proxy".into(), |value| display_name(value));
    }

    app.current_group()
        .map(|group| {
            let selected =
                desired_proxy_selection(config, app, &group.name).unwrap_or(group.now.as_str());
            format!(
                "{} -> {}",
                display_name(&group.name),
                display_name(selected)
            )
        })
        .unwrap_or_else(|| "choose group".into())
}

fn proxy_groups_page_is_global_proxy_list(config: &AppConfig, app: &ConfigApp) -> bool {
    proxy_mode(config, app).eq_ignore_ascii_case("global")
}

fn selected_global_proxy(config: &AppConfig, app: &ConfigApp) -> Option<String> {
    if let Some(service) = service_for_main_proxy(config, app) {
        return service.proxy.clone();
    }
    config
        .proxy_selections
        .get("GLOBAL")
        .or_else(|| config.proxy_selections.values().next())
        .cloned()
}

fn route_proxy_names(paths: &Paths, config: &AppConfig, app: &ConfigApp) -> Vec<String> {
    if let Some(subscription) = proxy_subscription_name(config, app).and_then(|name| {
        config
            .subscriptions
            .iter()
            .find(|subscription| subscription.name == name)
    }) {
        let proxies = subscription_profile_proxy_names(paths, subscription);
        if !proxies.is_empty() {
            return proxies;
        }
    }
    runtime_proxy_names(app)
}

fn runtime_group_for_proxy(app: &ConfigApp, proxy: &str) -> Option<String> {
    app.runtime
        .groups
        .iter()
        .find(|group| group.all.iter().any(|candidate| candidate == proxy))
        .map(|group| group.name.clone())
}

fn proxy_config_field_value(
    config: &AppConfig,
    app: &ConfigApp,
    field: ProxyConfigField,
) -> String {
    match field {
        ProxyConfigField::Enabled => {
            if app.selected_main == 0 {
                on_off(config.system_proxy.enabled).into()
            } else {
                on_off(main_proxy_row(config, app.selected_main).enabled).into()
            }
        }
        ProxyConfigField::Subscription => config
            .subscriptions
            .iter()
            .find(|subscription| {
                Some(subscription.name.as_str()) == proxy_subscription_name(config, app)
            })
            .map(|subscription| subscription.name.as_str())
            .map_or_else(|| "-".into(), display_name),
        ProxyConfigField::Mode => {
            let mode = proxy_mode(config, app);
            if app.selected_main == 0 {
                format!(
                    "{} / {}",
                    mode,
                    app.runtime.mode.as_deref().unwrap_or("unknown")
                )
            } else {
                mode.to_string()
            }
        }
        ProxyConfigField::ProxyGroups => proxy_server_value(config, app),
        ProxyConfigField::LocalPort => main_proxy_row(config, app.selected_main).listen,
        ProxyConfigField::OsProxy => on_off(config.system_proxy.enabled).into(),
        ProxyConfigField::Pac => "off".into(),
        ProxyConfigField::Tun => on_off(config.tun.enable).into(),
        ProxyConfigField::Dns => on_off(config.dns.enable).into(),
    }
}

const fn proxy_config_field_help(field: ProxyConfigField) -> &'static str {
    match field {
        ProxyConfigField::Enabled => "Enable or disable the selected proxy entry.",
        ProxyConfigField::Subscription => "Choose the source from an inline dropdown.",
        ProxyConfigField::Mode => "Choose rule, global, or direct behavior.",
        ProxyConfigField::ProxyGroups => {
            "Choose the group and concrete server for the current mode."
        }
        ProxyConfigField::LocalPort => "Edit the local listener port for this proxy.",
        ProxyConfigField::OsProxy => "Point the operating system proxy settings to the mixed port.",
        ProxyConfigField::Pac => "PAC support is planned; it will live on Global Proxy.",
        ProxyConfigField::Tun => {
            "TUN is transparent system traffic and belongs only to Global Proxy."
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

fn subscription_usage_summary(subscription: &Subscription) -> String {
    let used = subscription
        .user_info
        .used()
        .map(format_bytes_short)
        .unwrap_or_else(|| "-".into());
    let total = subscription
        .user_info
        .total
        .map(format_bytes_short)
        .unwrap_or_else(|| "-".into());
    let expire = subscription
        .user_info
        .expire
        .map(format_unix_timestamp)
        .unwrap_or_else(|| "-".into());
    let state = if subscription.last_error.is_some() {
        "error"
    } else {
        "ok"
    };
    format!("used {used}/{total} exp {expire} {state}")
}

fn subscription_list_help(subscription: &Subscription) -> String {
    let error = subscription
        .last_error
        .as_deref()
        .map(|error| format!(" | last error: {error}"))
        .unwrap_or_default();
    format!(
        "Enter opens details. refresh={} updated={} URL: {}{}",
        subscription_refresh_label(subscription.refresh),
        subscription.updated_at.as_deref().unwrap_or("-"),
        subscription.url,
        error
    )
}

fn format_unix_timestamp(value: u64) -> String {
    format!("unix:{value}")
}

fn runtime_proxy_names(app: &ConfigApp) -> Vec<String> {
    let mut proxies = Vec::new();
    for group in &app.runtime.groups {
        for proxy in &group.all {
            if !proxies.iter().any(|existing| existing == proxy) {
                proxies.push(proxy.clone());
            }
        }
    }
    proxies
}

fn subscription_proxy_names_for_view(
    paths: &Paths,
    config: &AppConfig,
    app: &ConfigApp,
) -> Vec<String> {
    let Some(subscription) = selected_subscription(config, app) else {
        return Vec::new();
    };
    let proxies = subscription_profile_proxy_names(paths, subscription);
    if !proxies.is_empty() {
        return proxies;
    }
    if is_selected_subscription_active(config, subscription) {
        return runtime_proxy_names(app);
    }
    Vec::new()
}

fn subscription_proxy_delay_key(subscription: &Subscription, proxy: &str) -> String {
    subscription_proxy_delay_key_for_name(&subscription.name, proxy)
}

fn subscription_proxy_delay_key_for_name(subscription_name: &str, proxy: &str) -> String {
    format!("{subscription_name}::{proxy}")
}

fn subscription_profile_proxy_names(paths: &Paths, subscription: &Subscription) -> Vec<String> {
    let profile = subscription::profile_path(paths, subscription);
    let Ok(content) = std::fs::read_to_string(profile) else {
        return Vec::new();
    };
    let Ok(value) = serde_yaml_ng::from_str::<serde_yaml_ng::Value>(&content) else {
        return Vec::new();
    };
    let Some(proxies) = value
        .as_mapping()
        .and_then(|mapping| mapping.get("proxies"))
        .and_then(serde_yaml_ng::Value::as_sequence)
    else {
        return Vec::new();
    };

    proxies
        .iter()
        .filter_map(|proxy| {
            proxy
                .as_mapping()
                .and_then(|mapping| mapping.get("name"))
                .and_then(serde_yaml_ng::Value::as_str)
                .map(str::to_string)
        })
        .collect()
}

fn empty_rows(label: &str, help: &str) -> Vec<SettingRow> {
    vec![SettingRow {
        label: label.into(),
        value: "-".into(),
        help: help.into(),
        kind: RowKind::Info,
    }]
}

#[derive(Debug, Default)]
struct SubscriptionProfileSummary {
    exists: bool,
    proxies: usize,
    proxy_groups: usize,
    rules: usize,
    rule_providers: usize,
    error: Option<String>,
}

impl SubscriptionProfileSummary {
    fn short_counts(&self) -> String {
        if !self.exists {
            return "profile missing".into();
        }
        if self.error.is_some() {
            return "profile error".into();
        }
        format!("groups={} rules={}", self.proxy_groups, self.rules)
    }

    fn rules_value(&self) -> String {
        if !self.exists {
            return "missing".into();
        }
        if self.error.is_some() {
            return "parse error".into();
        }
        format!("{} rules / {} providers", self.rules, self.rule_providers)
    }
}

fn subscription_profile_summary(
    paths: &Paths,
    subscription: &Subscription,
) -> SubscriptionProfileSummary {
    let profile = subscription::profile_path(paths, subscription);
    let content = match std::fs::read_to_string(&profile) {
        Ok(content) => content,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return SubscriptionProfileSummary::default();
        }
        Err(err) => {
            return SubscriptionProfileSummary {
                exists: true,
                error: Some(err.to_string()),
                ..SubscriptionProfileSummary::default()
            };
        }
    };
    let value: serde_yaml_ng::Value = match serde_yaml_ng::from_str(&content) {
        Ok(value) => value,
        Err(err) => {
            return SubscriptionProfileSummary {
                exists: true,
                error: Some(err.to_string()),
                ..SubscriptionProfileSummary::default()
            };
        }
    };
    let Some(mapping) = value.as_mapping() else {
        return SubscriptionProfileSummary {
            exists: true,
            error: Some("root is not a mapping".into()),
            ..SubscriptionProfileSummary::default()
        };
    };
    SubscriptionProfileSummary {
        exists: true,
        proxies: yaml_sequence_len(mapping.get("proxies")),
        proxy_groups: yaml_sequence_len(mapping.get("proxy-groups")),
        rules: yaml_sequence_len(mapping.get("rules")),
        rule_providers: yaml_mapping_len(mapping.get("rule-providers")),
        error: None,
    }
}

fn yaml_sequence_len(value: Option<&serde_yaml_ng::Value>) -> usize {
    value
        .and_then(serde_yaml_ng::Value::as_sequence)
        .map_or(0, Vec::len)
}

fn yaml_mapping_len(value: Option<&serde_yaml_ng::Value>) -> usize {
    value
        .and_then(serde_yaml_ng::Value::as_mapping)
        .map_or(0, |mapping| mapping.len())
}

fn proxy_runtime_line(app: &ConfigApp) -> String {
    format!(
        "{} {}",
        traffic_speed_summary(app.runtime.traffic.as_ref()),
        ip_info_summary(app)
    )
}

fn traffic_speed_summary(traffic: Option<&TrafficState>) -> String {
    traffic.map_or_else(
        || "↑- ↓-".into(),
        |traffic| {
            format!(
                "↑{}/s ↓{}/s",
                format_bytes_short(traffic.upload_speed),
                format_bytes_short(traffic.download_speed)
            )
        },
    )
}

fn traffic_total_summary(traffic: Option<&TrafficState>) -> String {
    traffic.map_or_else(
        || "-".into(),
        |traffic| {
            format!(
                "{}/{}",
                format_bytes_short(traffic.upload_total),
                format_bytes_short(traffic.download_total)
            )
        },
    )
}

fn ip_info_summary(app: &ConfigApp) -> String {
    if let Some(info) = &app.runtime.ip_info {
        let mut location = [info.country.as_deref(), info.city.as_deref()]
            .into_iter()
            .flatten()
            .filter(|value| !value.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
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
    let upload_total = value
        .get("uploadTotal")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or_default();
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
        upload_total,
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
        city: value
            .get("city")
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

fn visible_window_start(selected: usize, len: usize, visible: usize) -> usize {
    if visible == 0 || len <= visible {
        return 0;
    }
    let half = visible / 2;
    selected.saturating_sub(half).min(len - visible)
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

fn parse_listen_port_or_alert(
    app: &mut ConfigApp,
    value: &str,
    default_host: &str,
    label: &str,
) -> Option<(String, u16)> {
    let value = value.trim();
    if value.is_empty() {
        app.alert("Invalid Port", "Use a port or host:port.");
        return None;
    }
    if let Some((host, port)) = split_host_port_input(value) {
        let port = parse_port_or_alert(app, port, label)?;
        return Some((host.to_string(), port));
    }
    parse_port_or_alert(app, value, label).map(|port| (default_host.to_string(), port))
}

fn parse_service_listen_port_or_alert(
    app: &mut ConfigApp,
    value: &str,
    default_host: &str,
) -> Option<(String, u16)> {
    let value = value.trim();
    let lower = value.to_ascii_lowercase();
    if value.is_empty() || matches!(lower.as_str(), "0" | "auto") {
        return Some((default_host.to_string(), 0));
    }

    parse_listen_port_or_alert(app, value, default_host, "port proxy port")
}

fn split_host_port_input(value: &str) -> Option<(&str, &str)> {
    let (host, port) = value.rsplit_once(':')?;
    if host.trim().is_empty() || port.trim().is_empty() {
        return None;
    }
    Some((host.trim(), port.trim()))
}

fn optional_port_value(port: Option<u16>) -> String {
    port.map_or_else(|| "off".into(), |port| port.to_string())
}

fn is_number_input(input_mode: InputMode) -> bool {
    matches!(input_mode, InputMode::HttpPort | InputMode::SocksPort)
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

const fn on_off_upper(value: bool) -> &'static str {
    if value { "ON" } else { "OFF" }
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

    #[test]
    fn progress_helpers_handle_empty_and_partial_work() {
        assert_eq!(progress_percent(0, 0), 100);
        assert_eq!(progress_percent(45, 180), 25);
        assert_eq!(progress_bar(2, 4, 8), "[████░░░░]");
    }
}
