#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, IsTerminal as _, Stdout};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers,
};
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
use time::macros::format_description;
use time::{OffsetDateTime, UtcOffset};
use tokio::task::JoinHandle;
use tokio::time::sleep;

use crate::agent::{self, AgentEvent, ConfigPatch};
use crate::autostart;
use crate::config::{
    AppConfig, ControllerConfig, Paths, PortProxyService, ProxyProfile, RuntimePaths, Subscription,
    SubscriptionRefresh,
};
use crate::core;
use crate::dns;
use crate::i18n::Language;
use crate::llm::{LlmClient, LlmMessage};
use crate::llm_providers;
use crate::mihomo::{MihomoClient, ProxyGroup};
use crate::port_allocator;
use crate::runtime_profile;
use crate::service;
use crate::subscription;

const TICK_RATE: Duration = Duration::from_millis(200);
const REFRESH_INTERVAL: Duration = Duration::from_secs(3);
const LOG_REFRESH_INTERVAL: Duration = Duration::from_secs(1);
const IPINFO_REFRESH_INTERVAL: Duration = Duration::from_secs(10 * 60);
const IPINFO_RETRY_INTERVAL: Duration = Duration::from_secs(30);
const LABEL_WIDTH: usize = 24;
const APP_TITLE: &str = "ClashTUI Config";
const APP_VERSION: &str = concat!("v", env!("CARGO_PKG_VERSION"));
const DEFAULT_DELAY_TEST_URL: &str = "https://www.gstatic.com/generate_204";
const DEFAULT_DELAY_TEST_TIMEOUT_MS: u64 = 5_000;
const RUNTIME_START_RETRIES: usize = 20;
const RUNTIME_START_WAIT: Duration = Duration::from_millis(250);
const DEFAULT_INPUT_WIDTH: u16 = 58;
const DEFAULT_INPUT_ROWS: usize = 1;
const URL_INPUT_WIDTH: u16 = 96;
const URL_INPUT_ROWS: usize = 4;
const DNS_TEXT_INPUT_WIDTH: u16 = 96;
const DNS_TEXT_INPUT_ROWS: usize = 8;
const PAGE_JUMP_FALLBACK: usize = 8;
const PAGE_JUMP_MAX: usize = 24;
const H_LINE: char = '─';
const V_LINE: char = '│';
const TOP_JOINT: char = '┬';
const BOTTOM_JOINT: char = '┴';
const CHAT_INPUT_WIDTH: usize = 12_000;
const CHAT_PATCH_DIFF_CONTEXT: usize = 2;
const CHAT_PATCH_DIFF_MAX_LINES: usize = 160;
const PATCH_DIFF_REMOVE_BG: Color = Color::Rgb(96, 28, 28);
const PATCH_DIFF_ADD_BG: Color = Color::Rgb(24, 88, 44);

pub async fn run(paths: &Paths, config: &mut AppConfig, language: Language) -> Result<()> {
    paths.ensure().await?;
    let provider_warning = llm_providers::init_from_file(&paths.llm_providers_file);
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        print_config(paths, config)?;
        return Ok(());
    }

    let mut draft = config.clone();
    let mut terminal = TerminalGuard::enter()?;
    let mut app = ConfigApp::new_with_language(&draft, language);
    if let Some(warning) = provider_warning {
        app.status = warning;
    }
    app.start_runtime_refresh(paths, &draft);
    app.start_subscription_profile_refresh(paths, &draft);
    let mut last_refresh = Instant::now();

    while !app.should_quit {
        app.poll_delay_check();
        app.poll_runtime_refresh(&draft);
        app.poll_subscription_profile_refresh();
        app.poll_proxy_log_refresh();
        app.poll_chat(&mut draft);
        app.poll_assistant_test();
        app.start_proxy_log_refresh(paths, &draft);
        app.poll_runtime_command(paths, &draft).await;
        terminal
            .terminal
            .draw(|frame| draw(frame, paths, &draft, &app))?;
        if event::poll(TICK_RATE)? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    handle_key(paths, &mut draft, &mut app, key).await?;
                }
                Event::Paste(value) => {
                    handle_paste(paths, &mut draft, &mut app, &value).await?;
                }
                _ => {}
            }
        }
        if last_refresh.elapsed() >= REFRESH_INTERVAL && app.runtime_command.is_none() {
            app.start_runtime_refresh(paths, &draft);
            last_refresh = Instant::now();
        }
        app.poll_delay_check();
        app.poll_runtime_refresh(&draft);
        app.poll_subscription_profile_refresh();
        app.poll_proxy_log_refresh();
        app.poll_chat(&mut draft);
        app.poll_assistant_test();
        app.poll_runtime_command(paths, &draft).await;
    }

    if !app.dirty {
        *config = draft;
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Page {
    Main,
    Profile,
    ProfileConfig,
    Subscription,
    SubscriptionDetail,
    SubscriptionRuleGroups,
    SubscriptionProxies,
    SubscriptionRules,
    AddSubscription,
    Runtime,
    Chat,
    Exit,
    ProxyConfig,
    ProxyConnections,
    ProxyLogs,
    ProxyGroups,
    Mode,
    Dns,
}

impl Page {
    const SECTION_ROOTS: [Self; 7] = [
        Self::Main,
        Self::Profile,
        Self::Subscription,
        Self::Dns,
        Self::Runtime,
        Self::Chat,
        Self::Exit,
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
        self.title_for(Language::En)
    }

    const fn title_for(self, language: Language) -> &'static str {
        if language.is_zh_cn() {
            return match self {
                Self::Main => "主页",
                Self::Profile => "Profile",
                Self::ProfileConfig => "Profile 设置",
                Self::Subscription => "订阅",
                Self::SubscriptionDetail => "订阅详情",
                Self::SubscriptionRuleGroups => "Rule Groups",
                Self::SubscriptionProxies => "Proxies",
                Self::SubscriptionRules => "Rules",
                Self::AddSubscription => "添加订阅",
                Self::Runtime => "Runtime",
                Self::Chat => "Chat",
                Self::Exit => "退出",
                Self::ProxyConfig => "Proxy",
                Self::ProxyConnections => "Connections",
                Self::ProxyLogs => "Logs",
                Self::ProxyGroups => "Proxy Groups",
                Self::Mode => "Mode",
                Self::Dns => "DNS",
            };
        }
        match self {
            Self::Main => "Main",
            Self::Profile => "Profile",
            Self::ProfileConfig => "Profile Config",
            Self::Subscription => "Subscription",
            Self::SubscriptionDetail => "Subscription Detail",
            Self::SubscriptionRuleGroups => "Rule Groups",
            Self::SubscriptionProxies => "Proxies",
            Self::SubscriptionRules => "Rules",
            Self::AddSubscription => "Add Subscription",
            Self::Runtime => "Runtime",
            Self::Chat => "Chat",
            Self::Exit => "Exit",
            Self::ProxyConfig => "Proxy",
            Self::ProxyConnections => "Connections",
            Self::ProxyLogs => "Logs",
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
            Self::Main
            | Self::ProxyConfig
            | Self::ProxyConnections
            | Self::ProxyLogs
            | Self::ProxyGroups
            | Self::Mode => 0,
            Self::Profile | Self::ProfileConfig => 1,
            Self::Subscription
            | Self::SubscriptionDetail
            | Self::SubscriptionRuleGroups
            | Self::SubscriptionProxies
            | Self::SubscriptionRules
            | Self::AddSubscription => 2,
            Self::Dns => 3,
            Self::Runtime => 4,
            Self::Chat => 5,
            Self::Exit => 6,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RuntimeItem {
    Service,
    Autostart,
    Logs,
    CorePath,
    CoreUpdate,
    Controller,
    Refresh,
    LlmSection,
    LlmProvider,
    LlmBaseUrl,
    LlmModel,
    LlmApiKey,
    LlmProvidersUpdate,
    TestAssistant,
}

impl RuntimeItem {
    const ALL: [Self; 14] = [
        Self::Service,
        Self::Autostart,
        Self::Logs,
        Self::CorePath,
        Self::CoreUpdate,
        Self::Controller,
        Self::Refresh,
        Self::LlmSection,
        Self::LlmProvider,
        Self::LlmBaseUrl,
        Self::LlmModel,
        Self::LlmApiKey,
        Self::LlmProvidersUpdate,
        Self::TestAssistant,
    ];

    const fn label(self) -> &'static str {
        self.label_for(Language::En)
    }

    const fn label_for(self, language: Language) -> &'static str {
        if language.is_zh_cn() {
            return match self {
                Self::Service => "Service",
                Self::Autostart => "自启动",
                Self::Logs => "Logs",
                Self::CorePath => "Mihomo Core",
                Self::CoreUpdate => "更新 Core",
                Self::Controller => "Controller",
                Self::Refresh => "刷新 Runtime",
                Self::LlmSection => "LLM",
                Self::LlmProvider => "LLM Provider",
                Self::LlmBaseUrl => "LLM Base URL",
                Self::LlmModel => "LLM Model",
                Self::LlmApiKey => "LLM API Key",
                Self::LlmProvidersUpdate => "更新 LLM Providers",
                Self::TestAssistant => "Test Assistant",
            };
        }
        match self {
            Self::Service => "Service",
            Self::Autostart => "Autostart",
            Self::Logs => "Logs",
            Self::CorePath => "Mihomo Core",
            Self::CoreUpdate => "Update Core",
            Self::Controller => "Controller",
            Self::Refresh => "Refresh Runtime",
            Self::LlmSection => "LLM",
            Self::LlmProvider => "LLM Provider",
            Self::LlmBaseUrl => "LLM Base URL",
            Self::LlmModel => "LLM Model",
            Self::LlmApiKey => "LLM API Key",
            Self::LlmProvidersUpdate => "Update LLM Providers",
            Self::TestAssistant => "Test Assistant",
        }
    }

    const fn is_section(self) -> bool {
        matches!(self, Self::LlmSection)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExitItem {
    RuntimeSection,
    StartRuntime,
    StopRuntime,
    ReloadRuntime,
    RestartRuntime,
    SaveSection,
    SaveConfig,
    SaveRestart,
    SaveRestartExit,
    ExitSection,
    ExitWithoutSaving,
    LoadDefaults,
    Exit,
}

impl ExitItem {
    const ALL: [Self; 13] = [
        Self::SaveSection,
        Self::SaveConfig,
        Self::SaveRestart,
        Self::SaveRestartExit,
        Self::RuntimeSection,
        Self::StartRuntime,
        Self::StopRuntime,
        Self::ReloadRuntime,
        Self::RestartRuntime,
        Self::ExitSection,
        Self::ExitWithoutSaving,
        Self::LoadDefaults,
        Self::Exit,
    ];

    const fn label(self) -> &'static str {
        self.label_for(Language::En)
    }

    const fn label_for(self, language: Language) -> &'static str {
        if language.is_zh_cn() {
            return match self {
                Self::RuntimeSection => "Runtime",
                Self::StartRuntime => "启动",
                Self::StopRuntime => "停止",
                Self::ReloadRuntime => "Reload",
                Self::RestartRuntime => "重启",
                Self::SaveSection => "保存",
                Self::SaveConfig => "保存",
                Self::SaveRestart => "保存并重启",
                Self::SaveRestartExit => "保存、重启并退出",
                Self::ExitSection => "退出",
                Self::ExitWithoutSaving => "不保存退出",
                Self::LoadDefaults => "加载默认值",
                Self::Exit => "退出",
            };
        }
        match self {
            Self::RuntimeSection => "Runtime",
            Self::StartRuntime => "Start",
            Self::StopRuntime => "Stop",
            Self::ReloadRuntime => "Reload",
            Self::RestartRuntime => "Restart",
            Self::SaveSection => "Save",
            Self::SaveConfig => "Save",
            Self::SaveRestart => "Save & Restart",
            Self::SaveRestartExit => "Save, Restart & Exit",
            Self::ExitSection => "Exit",
            Self::ExitWithoutSaving => "Exit Without Saving",
            Self::LoadDefaults => "Load Defaults",
            Self::Exit => "Exit",
        }
    }

    const fn value(self) -> &'static str {
        match self {
            Self::RuntimeSection | Self::SaveSection | Self::ExitSection => "",
            Self::StartRuntime => "start",
            Self::StopRuntime => "stop",
            Self::ReloadRuntime => "reload",
            Self::RestartRuntime => "restart",
            Self::SaveConfig => "disk",
            Self::SaveRestart => "F10",
            Self::SaveRestartExit => "exit",
            Self::ExitWithoutSaving => "discard",
            Self::LoadDefaults => "F9",
            Self::Exit => "now",
        }
    }

    const fn help(self) -> &'static str {
        self.help_for(Language::En)
    }

    const fn help_for(self, language: Language) -> &'static str {
        if language.is_zh_cn() {
            return match self {
                Self::RuntimeSection => "Runtime 操作使用磁盘上已保存的 config。",
                Self::StartRuntime => "启动 clashtui daemon 和 mihomo Runtime。",
                Self::StopRuntime => "停止 clashtui 管理的 Runtime。",
                Self::ReloadRuntime => "用已保存的 config reload mihomo，不停止进程。",
                Self::RestartRuntime => "使用已保存的 config 重启 Runtime。",
                Self::SaveSection => "先保存当前 config 修改的操作。",
                Self::SaveConfig => "保存 config 修改，不重启 Runtime。",
                Self::SaveRestart => "保存 config 并重启当前 clashtui Runtime。",
                Self::SaveRestartExit => "保存 config、重启 Runtime，然后关闭 TUI。",
                Self::ExitSection => "关闭 config UI 或重置默认值。",
                Self::ExitWithoutSaving => "丢弃未保存修改并关闭 TUI。",
                Self::LoadDefaults => "确认后加载 setup 默认值。",
                Self::Exit => "立即关闭 config UI，不再确认。",
            };
        }
        match self {
            Self::RuntimeSection => "Runtime operations use the saved config on disk.",
            Self::StartRuntime => "Start the clashtui daemon and mihomo runtime.",
            Self::StopRuntime => "Stop the clashtui-owned runtime.",
            Self::ReloadRuntime => "Reload mihomo from the saved config without stopping it.",
            Self::RestartRuntime => "Restart the runtime using the saved config.",
            Self::SaveSection => "Actions that first persist pending config edits.",
            Self::SaveConfig => "Save config edits without restarting runtime.",
            Self::SaveRestart => "Save config and restart the current clashtui-owned runtime.",
            Self::SaveRestartExit => "Save config, restart runtime, then close the TUI.",
            Self::ExitSection => "Close the config UI or reset editable defaults.",
            Self::ExitWithoutSaving => "Discard pending edits and close the TUI.",
            Self::LoadDefaults => "Load setup defaults after confirmation.",
            Self::Exit => "Close the config UI immediately without confirmation.",
        }
    }

    const fn is_section(self) -> bool {
        matches!(
            self,
            Self::RuntimeSection | Self::SaveSection | Self::ExitSection
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProxyConfigField {
    ServiceStatus,
    TrafficStatus,
    Connections,
    Enabled,
    Profile,
    Subscription,
    Mode,
    ProxyGroups,
    LocalPort,
    Delete,
    OsProxy,
    Pac,
    Tun,
    Logs,
}

impl ProxyConfigField {
    const SYSTEM_ALL: [Self; 10] = [
        Self::ServiceStatus,
        Self::TrafficStatus,
        Self::Connections,
        Self::Enabled,
        Self::Profile,
        Self::LocalPort,
        Self::OsProxy,
        Self::Pac,
        Self::Tun,
        Self::Logs,
    ];

    const PORT_ALL: [Self; 10] = [
        Self::ServiceStatus,
        Self::TrafficStatus,
        Self::Connections,
        Self::Enabled,
        Self::Subscription,
        Self::Mode,
        Self::ProxyGroups,
        Self::LocalPort,
        Self::Delete,
        Self::Logs,
    ];

    const fn label(self) -> &'static str {
        match self {
            Self::ServiceStatus => "Service",
            Self::TrafficStatus => "Traffic",
            Self::Connections => "Connections",
            Self::Enabled => "Enabled",
            Self::Profile => "Profile",
            Self::Subscription => "Subscription",
            Self::Mode => "Mode",
            Self::ProxyGroups => "Proxy Server",
            Self::LocalPort => "Local Port",
            Self::Delete => "Delete",
            Self::OsProxy => "Sys Proxy",
            Self::Pac => "PAC",
            Self::Tun => "TUN",
            Self::Logs => "Logs",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProfileConfigField {
    Name,
    Active,
    Subscription,
    Mode,
    ProxyGroups,
    Activate,
    Delete,
}

impl ProfileConfigField {
    const ALL: [Self; 7] = [
        Self::Name,
        Self::Active,
        Self::Subscription,
        Self::Mode,
        Self::ProxyGroups,
        Self::Activate,
        Self::Delete,
    ];

    const fn label(self) -> &'static str {
        match self {
            Self::Name => "Name",
            Self::Active => "Active",
            Self::Subscription => "Subscription",
            Self::Mode => "Mode",
            Self::ProxyGroups => "Proxy Server",
            Self::Activate => "Activate",
            Self::Delete => "Delete",
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
    NameserverPolicy,
    DirectNameserver,
    DirectFollowPolicy,
    Nameserver,
    Fallback,
    FakeIpFilter,
}

impl DnsItem {
    const ALL: [Self; 10] = [
        Self::Enabled,
        Self::Listen,
        Self::LanDomains,
        Self::LanNameserver,
        Self::NameserverPolicy,
        Self::DirectNameserver,
        Self::DirectFollowPolicy,
        Self::Nameserver,
        Self::Fallback,
        Self::FakeIpFilter,
    ];

    const fn label(self) -> &'static str {
        self.label_for(Language::En)
    }

    const fn label_for(self, language: Language) -> &'static str {
        if language.is_zh_cn() {
            return match self {
                Self::Enabled => "DNS",
                Self::Listen => "Listen",
                Self::LanDomains => "LAN Domains",
                Self::LanNameserver => "LAN DNS",
                Self::NameserverPolicy => "DNS Policy",
                Self::DirectNameserver => "Direct DNS",
                Self::DirectFollowPolicy => "Direct 跟随 policy",
                Self::Nameserver => "Default DNS",
                Self::Fallback => "Fallback DNS",
                Self::FakeIpFilter => "Fake-IP Filter",
            };
        }
        match self {
            Self::Enabled => "DNS",
            Self::Listen => "Listen",
            Self::LanDomains => "LAN Domains",
            Self::LanNameserver => "LAN DNS",
            Self::NameserverPolicy => "DNS Policy",
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
    ProxyProfileName,
    SubscriptionName,
    SubscriptionUrl,
    CorePath,
    Controller,
    LlmBaseUrl,
    LlmModel,
    LlmApiKey,
    MixedPort,
    HttpPort,
    SocksPort,
    ServicePort,
    DnsListen,
    DnsLanDomains,
    DnsLanNameserver,
    DnsNameserverPolicy,
    DnsDirectNameserver,
    DnsNameserver,
    DnsFallback,
    DnsFakeIpFilter,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InputFocus {
    Editor,
    Save,
    Cancel,
}

impl InputMode {
    const fn title(self) -> &'static str {
        self.title_for(Language::En)
    }

    const fn title_for(self, language: Language) -> &'static str {
        if language.is_zh_cn() {
            return match self {
                Self::Normal => "",
                Self::ProxyProfileName => "Profile 名称",
                Self::SubscriptionName => "订阅名称",
                Self::SubscriptionUrl => "订阅 URL",
                Self::CorePath => "Mihomo Core 路径",
                Self::Controller => "Controller URL",
                Self::LlmBaseUrl => "LLM Base URL",
                Self::LlmModel => "LLM Model",
                Self::LlmApiKey => "LLM API Key",
                Self::MixedPort => "Mixed Port",
                Self::HttpPort => "HTTP Port",
                Self::SocksPort => "SOCKS Port",
                Self::ServicePort => "Port Proxy Port",
                Self::DnsListen => "DNS Listen",
                Self::DnsLanDomains => "LAN Domains",
                Self::DnsLanNameserver => "LAN DNS",
                Self::DnsNameserverPolicy => "DNS Policy",
                Self::DnsDirectNameserver => "Direct DNS",
                Self::DnsNameserver => "Default DNS",
                Self::DnsFallback => "Fallback DNS",
                Self::DnsFakeIpFilter => "Fake-IP Filter",
            };
        }
        match self {
            Self::Normal => "",
            Self::ProxyProfileName => "Profile Name",
            Self::SubscriptionName => "Subscription Name",
            Self::SubscriptionUrl => "Subscription URL",
            Self::CorePath => "Mihomo Core Path",
            Self::Controller => "Controller URL",
            Self::LlmBaseUrl => "LLM Base URL",
            Self::LlmModel => "LLM Model",
            Self::LlmApiKey => "LLM API Key",
            Self::MixedPort => "Mixed Port",
            Self::HttpPort => "HTTP Port",
            Self::SocksPort => "SOCKS Port",
            Self::ServicePort => "Port Proxy Port",
            Self::DnsListen => "DNS Listen",
            Self::DnsLanDomains => "LAN Domains",
            Self::DnsLanNameserver => "LAN DNS",
            Self::DnsNameserverPolicy => "DNS Policy",
            Self::DnsDirectNameserver => "Direct DNS",
            Self::DnsNameserver => "Default DNS",
            Self::DnsFallback => "Fallback DNS",
            Self::DnsFakeIpFilter => "Fake-IP Filter",
        }
    }
}

#[derive(Clone, Default)]
struct RuntimeState {
    version: Option<String>,
    mode: Option<String>,
    groups: Vec<ProxyGroup>,
    traffic: Option<TrafficState>,
    connections: Option<ConnectionState>,
    process: ProcessState,
    ip_info: Option<IpInfoState>,
    ip_info_error: Option<String>,
    error: Option<String>,
}

#[derive(Clone, Default)]
struct TrafficState {
    upload_total: Option<u64>,
    download_total: Option<u64>,
    upload_speed: Option<u64>,
    download_speed: Option<u64>,
    sampled_at: Option<Instant>,
}

#[derive(Clone, Default)]
struct ConnectionState {
    active: usize,
    items: Vec<ConnectionInfo>,
}

#[derive(Clone, Default)]
struct ConnectionInfo {
    id: String,
    network: String,
    inbound: String,
    host: String,
    source: String,
    destination: String,
    upload: u64,
    download: u64,
    start: String,
    chains: Vec<String>,
    rule: String,
    rule_payload: String,
    process: String,
}

#[derive(Clone, Default)]
struct ProcessState {
    pid: Option<u32>,
    running: bool,
    cpu_percent: Option<f32>,
    mem_percent: Option<f32>,
    rss_bytes: Option<u64>,
}

#[derive(Clone)]
struct CompactRow {
    primary: String,
    secondary: Option<String>,
}

#[derive(Clone)]
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
    DeleteProxyProfile,
    DeletePortProxy,
}

impl ConfirmAction {
    const fn title(self) -> &'static str {
        self.title_for(Language::En)
    }

    const fn title_for(self, language: Language) -> &'static str {
        if language.is_zh_cn() {
            return match self {
                Self::ExitWithoutSaving => "不保存退出？",
                Self::LoadDefaults => "加载默认设置？",
                Self::SaveRestart => "保存并重启？",
                Self::SaveRestartExit => "保存、重启并退出？",
                Self::DeleteSubscription => "删除订阅？",
                Self::DeleteProxyProfile => "删除 Profile？",
                Self::DeletePortProxy => "删除 Port Proxy？",
            };
        }
        match self {
            Self::ExitWithoutSaving => "Exit Without Saving?",
            Self::LoadDefaults => "Load Setup Defaults?",
            Self::SaveRestart => "Save And Restart?",
            Self::SaveRestartExit => "Save, Restart, And Exit?",
            Self::DeleteSubscription => "Delete Subscription?",
            Self::DeleteProxyProfile => "Delete Profile?",
            Self::DeletePortProxy => "Delete Port Proxy?",
        }
    }

    const fn message(self) -> &'static [&'static str] {
        self.message_for(Language::En)
    }

    const fn message_for(self, language: Language) -> &'static [&'static str] {
        if language.is_zh_cn() {
            return match self {
                Self::ExitWithoutSaving => &[
                    "不保存当前修改，直接退出 ClashTUI Config？",
                    "未保存的修改会被丢弃。",
                ],
                Self::LoadDefaults => &[
                    "加载可配置字段的 setup 默认值？",
                    "此操作在当前 preview 中尚未实现。",
                ],
                Self::SaveRestart => &["保存 config 并立即重启 mihomo？"],
                Self::SaveRestartExit => &["保存 config、重启 mihomo，然后退出？"],
                Self::DeleteSubscription => &["删除此订阅？"],
                Self::DeleteProxyProfile => &["删除此 Profile？"],
                Self::DeletePortProxy => &["删除此 Port Proxy？"],
            };
        }
        match self {
            Self::ExitWithoutSaving => &[
                "Exit ClashTUI Config without saving pending changes?",
                "Pending edits will be discarded.",
            ],
            Self::LoadDefaults => &[
                "Load setup defaults for configurable fields?",
                "This action is not implemented in this preview.",
            ],
            Self::SaveRestart => &["Save config and restart mihomo now?"],
            Self::SaveRestartExit => &["Save config, restart mihomo, then exit?"],
            Self::DeleteSubscription => &["Delete this subscription?"],
            Self::DeleteProxyProfile => &["Delete this profile?"],
            Self::DeletePortProxy => &["Delete this port proxy?"],
        }
    }

    const fn yes_label(self) -> &'static str {
        self.yes_label_for(Language::En)
    }

    const fn yes_label_for(self, language: Language) -> &'static str {
        if language.is_zh_cn() {
            return match self {
                Self::ExitWithoutSaving => "确认退出",
                Self::LoadDefaults => "确认加载",
                Self::SaveRestart => "确认保存并重启",
                Self::SaveRestartExit => "确认保存重启并退出",
                Self::DeleteSubscription => "确认删除",
                Self::DeleteProxyProfile => "确认删除",
                Self::DeletePortProxy => "确认删除",
            };
        }
        match self {
            Self::ExitWithoutSaving => "Yes, Exit",
            Self::LoadDefaults => "Yes, Load Defaults",
            Self::SaveRestart => "Yes, Save & Restart",
            Self::SaveRestartExit => "Yes, Save & Restart & Exit",
            Self::DeleteSubscription => "Yes, Delete",
            Self::DeleteProxyProfile => "Yes, Delete",
            Self::DeletePortProxy => "Yes, Delete",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Dropdown {
    ProxyProfile,
    ProxySubscription,
    Mode,
    SubscriptionRefresh,
    CoreSource,
    LlmProvider,
    LlmModel,
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
    Section,
    SoftSection,
    Submenu(Page),
    StatusSubmenu(Page),
    Toggle(ToggleAction),
    Choice(ChoiceAction),
    Input(InputMode),
    Action(ActionKind),
    Status,
    Info,
}

#[derive(Debug, Clone, Copy)]
enum ToggleAction {
    SystemProxy,
    PortProxy,
    Autostart,
    Tun,
    Dns,
    DnsDirectFollowPolicy,
}

#[derive(Debug, Clone, Copy)]
enum ChoiceAction {
    Profile,
    Subscription,
    Mode,
    SubscriptionRefresh,
    CoreSource,
    LlmProvider,
    LlmModel,
}

#[derive(Debug, Clone, Copy)]
enum ActionKind {
    AddPortProxy,
    AddSubscription,
    SaveSubscription,
    UpdateSubscription,
    DeleteSubscription,
    DeletePortProxy,
    EditSubscription,
    TestProxyDelay,
    TestAllProxyDelays,
    Service,
    Logs,
    RefreshRuntime,
    UpdateLlmProviders,
    TestAssistant,
    UpdateCore,
    StartRuntime,
    StopRuntime,
    ReloadRuntime,
    RestartRuntime,
    LoadDefaults,
    SaveConfig,
    SaveRestart,
    SaveRestartExit,
    ExitWithoutSaving,
    Exit,
    SelectProxyGroup,
    SelectProxy,
}

#[derive(Debug, Clone)]
struct Alert {
    title: String,
    message: String,
}

#[derive(Debug, Clone)]
struct RuleGroupSelection {
    subscription_index: usize,
    group_name: String,
}

struct DelayCheckTask {
    receiver: Receiver<DelayCheckEvent>,
    handle: JoinHandle<()>,
    progress: DelayCheckProgress,
}

struct RuntimeRefreshTask {
    receiver: Receiver<RuntimeRefreshResult>,
    handle: JoinHandle<()>,
}

struct SubscriptionProfileRefreshTask {
    receiver: Receiver<SubscriptionProfileCache>,
    handle: JoinHandle<()>,
}

struct ProxyLogRefreshTask {
    key: String,
    receiver: Receiver<ProxyLogRefreshResult>,
    handle: JoinHandle<()>,
}

struct ProxyLogRefreshResult {
    key: String,
    rows: Vec<CompactRow>,
}

#[derive(Clone, Default)]
struct SubscriptionProfileCache {
    profiles: BTreeMap<String, CachedSubscriptionProfile>,
    loaded: bool,
}

#[derive(Clone, Default)]
struct CachedSubscriptionProfile {
    summary: SubscriptionProfileSummary,
    proxies: Vec<String>,
    groups: Vec<ProxyGroup>,
    rule_rows: Vec<SettingRow>,
}

#[derive(Debug, Clone, Copy)]
enum RuntimeCommand {
    Start,
    Stop,
    Reload,
    Restart,
    UpdateCore,
}

impl RuntimeCommand {
    const fn title(self) -> &'static str {
        match self {
            Self::Start => "Starting Runtime",
            Self::Stop => "Stopping Runtime",
            Self::Reload => "Reloading Runtime",
            Self::Restart => "Restarting Runtime",
            Self::UpdateCore => "Updating Mihomo Core",
        }
    }

    const fn message(self) -> &'static str {
        match self {
            Self::Start => "Starting mihomo...",
            Self::Stop => "Stopping mihomo...",
            Self::Reload => "Reloading mihomo...",
            Self::Restart => "Restarting mihomo...",
            Self::UpdateCore => "Updating mihomo core...",
        }
    }
}

struct RuntimeCommandTask {
    receiver: Receiver<RuntimeCommandResult>,
    handle: JoinHandle<()>,
    started_at: Instant,
    command: RuntimeCommand,
    success: String,
    exit_after: bool,
}

struct ChatTask {
    receiver: Receiver<AgentEvent>,
    handle: JoinHandle<()>,
}

struct AssistantTestTask {
    receiver: Receiver<AssistantTestEvent>,
    handle: JoinHandle<()>,
    started_at: Instant,
    response: String,
    error: Option<String>,
    finished: bool,
}

enum AssistantTestEvent {
    Content(String),
    Error(String),
    Done,
}

#[derive(Default)]
struct ChatState {
    entries: Vec<ChatEntry>,
    input: String,
    scroll: usize,
    task: Option<ChatTask>,
    usage: ChatUsage,
    last_run_usage: agent::AgentUsage,
}

#[derive(Default)]
struct ChatUsage {
    prompt_tokens: usize,
    completion_tokens: usize,
    context_tokens: usize,
    context_chars: usize,
    context_messages: usize,
    turns: usize,
    tool_calls: usize,
    estimated: bool,
}

#[derive(Clone)]
struct ChatEntry {
    kind: ChatEntryKind,
    content: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ChatEntryKind {
    User,
    Assistant,
    Tool,
    Patch,
    Error,
}

struct RuntimeRefreshResult {
    runtime: RuntimeState,
    proxy_runtimes: BTreeMap<String, RuntimeState>,
    refreshed_ip_info: bool,
}

#[derive(Debug)]
struct RuntimeCommandResult {
    success: bool,
    message: Option<String>,
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
    language: Language,
    page: Page,
    section: Page,
    history: Vec<Location>,
    proxy_pane: ProxyPane,
    selected_main: usize,
    selected_profile: usize,
    selected_profile_field: usize,
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
    selected_proxy_connection: usize,
    selected_proxy_log: usize,
    selected_mode: usize,
    selected_dns: usize,
    selected_exit: usize,
    input_mode: InputMode,
    input_focus: InputFocus,
    dropdown: Option<Dropdown>,
    input: String,
    input_cursor: usize,
    input_desired_column: Option<usize>,
    subscription_form: SubscriptionForm,
    runtime: RuntimeState,
    runtime_checked: bool,
    proxy_runtimes: BTreeMap<String, RuntimeState>,
    proxy_delays: BTreeMap<String, String>,
    proxy_log_cache: BTreeMap<String, Vec<CompactRow>>,
    proxy_log_refreshed_at: BTreeMap<String, Instant>,
    subscription_profiles: SubscriptionProfileCache,
    delay_check: Option<DelayCheckTask>,
    runtime_refresh: Option<RuntimeRefreshTask>,
    subscription_profile_refresh: Option<SubscriptionProfileRefreshTask>,
    proxy_log_refresh: Option<ProxyLogRefreshTask>,
    runtime_command: Option<RuntimeCommandTask>,
    rule_group_selection: Option<RuleGroupSelection>,
    last_ip_info_refresh: Option<Instant>,
    chat: ChatState,
    assistant_test: Option<AssistantTestTask>,
    status: String,
    should_quit: bool,
    dirty: bool,
    confirm: Option<ConfirmAction>,
    confirm_yes: bool,
    alert: Option<Alert>,
}

impl ConfigApp {
    fn new(config: &AppConfig) -> Self {
        Self::new_with_language(config, Language::En)
    }

    fn new_with_language(config: &AppConfig, language: Language) -> Self {
        Self {
            language,
            page: Page::Main,
            section: Page::Main,
            history: Vec::new(),
            proxy_pane: ProxyPane::Groups,
            selected_main: 0,
            selected_profile: active_proxy_profile_index(config).unwrap_or_default(),
            selected_profile_field: 0,
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
            selected_proxy_connection: 0,
            selected_proxy_log: 0,
            selected_mode: runtime_mode_index(config),
            selected_dns: 0,
            selected_exit: first_selectable_exit_index(),
            input_mode: InputMode::Normal,
            input_focus: InputFocus::Editor,
            dropdown: None,
            input: String::new(),
            input_cursor: 0,
            input_desired_column: None,
            subscription_form: SubscriptionForm::default(),
            runtime: RuntimeState::default(),
            runtime_checked: false,
            proxy_runtimes: BTreeMap::new(),
            proxy_delays: BTreeMap::new(),
            proxy_log_cache: BTreeMap::new(),
            proxy_log_refreshed_at: BTreeMap::new(),
            subscription_profiles: SubscriptionProfileCache::default(),
            delay_check: None,
            runtime_refresh: None,
            subscription_profile_refresh: None,
            proxy_log_refresh: None,
            runtime_command: None,
            rule_group_selection: None,
            last_ip_info_refresh: None,
            chat: ChatState::default(),
            assistant_test: None,
            status: String::new(),
            should_quit: false,
            dirty: false,
            confirm: None,
            confirm_yes: false,
            alert: None,
        }
    }

    fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    fn mark_saved(&mut self) {
        self.dirty = false;
    }

    fn refresh_runtime(&mut self, paths: &Paths, config: &AppConfig) {
        if let Some(task) = self.runtime_refresh.take() {
            task.handle.abort();
        }
        self.start_runtime_refresh(paths, config);
    }

    fn start_runtime_refresh(&mut self, paths: &Paths, config: &AppConfig) {
        if self.runtime_refresh.is_some() {
            return;
        }
        let paths = paths.clone();
        let config = config.clone();
        let refresh_ip_info = self.should_refresh_ip_info();
        let (sender, receiver) = mpsc::channel();
        let handle = tokio::spawn(async move {
            let result = fetch_runtime_refresh(paths, config, refresh_ip_info).await;
            let _ = sender.send(result);
        });
        self.runtime_refresh = Some(RuntimeRefreshTask { receiver, handle });
    }

    fn poll_runtime_refresh(&mut self, config: &AppConfig) {
        let Some(task) = self.runtime_refresh.as_mut() else {
            return;
        };
        match task.receiver.try_recv() {
            Ok(result) => {
                self.runtime_refresh = None;
                self.apply_runtime_refresh(config, result);
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                self.runtime_refresh = None;
                self.runtime.error = Some("runtime refresh task stopped".into());
            }
        }
    }

    fn start_subscription_profile_refresh(&mut self, paths: &Paths, config: &AppConfig) {
        if self.subscription_profile_refresh.is_some() {
            return;
        }
        let paths = paths.clone();
        let config = config.clone();
        let (sender, receiver) = mpsc::channel();
        let handle = tokio::spawn(async move {
            let result = tokio::task::spawn_blocking(move || {
                fetch_subscription_profile_cache(&paths, &config)
            })
            .await
            .unwrap_or_else(|_| SubscriptionProfileCache {
                loaded: true,
                ..SubscriptionProfileCache::default()
            });
            let _ = sender.send(result);
        });
        self.subscription_profile_refresh =
            Some(SubscriptionProfileRefreshTask { receiver, handle });
    }

    fn restart_subscription_profile_refresh(&mut self, paths: &Paths, config: &AppConfig) {
        if let Some(task) = self.subscription_profile_refresh.take() {
            task.handle.abort();
        }
        self.start_subscription_profile_refresh(paths, config);
    }

    fn poll_subscription_profile_refresh(&mut self) {
        let Some(task) = self.subscription_profile_refresh.as_mut() else {
            return;
        };
        match task.receiver.try_recv() {
            Ok(cache) => {
                self.subscription_profile_refresh = None;
                self.subscription_profiles = cache;
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                self.subscription_profile_refresh = None;
                self.subscription_profiles.loaded = true;
            }
        }
    }

    fn start_proxy_log_refresh(&mut self, paths: &Paths, config: &AppConfig) {
        if self.page != Page::ProxyLogs {
            return;
        }
        let key = main_proxy_runtime_key(config, self.selected_main);
        if self
            .proxy_log_refresh
            .as_ref()
            .is_some_and(|task| task.key == key)
        {
            return;
        }
        if self
            .proxy_log_refreshed_at
            .get(&key)
            .is_some_and(|last| last.elapsed() < LOG_REFRESH_INTERVAL)
        {
            return;
        }
        if let Some(task) = self.proxy_log_refresh.take() {
            task.handle.abort();
        }

        let paths = paths.clone();
        let config = config.clone();
        let selected_main = self.selected_main;
        let task_key = key.clone();
        let (sender, receiver) = mpsc::channel();
        let handle = tokio::spawn(async move {
            let rows = tokio::task::spawn_blocking(move || {
                fetch_proxy_log_rows(&paths, &config, selected_main)
            })
            .await
            .unwrap_or_else(|_| {
                vec![CompactRow {
                    primary: "  1  log refresh failed".into(),
                    secondary: None,
                }]
            });
            let _ = sender.send(ProxyLogRefreshResult {
                key: task_key,
                rows,
            });
        });
        self.proxy_log_refresh = Some(ProxyLogRefreshTask {
            key,
            receiver,
            handle,
        });
    }

    fn poll_proxy_log_refresh(&mut self) {
        let Some(task) = self.proxy_log_refresh.as_mut() else {
            return;
        };
        match task.receiver.try_recv() {
            Ok(result) => {
                self.proxy_log_refresh = None;
                self.proxy_log_refreshed_at
                    .insert(result.key.clone(), Instant::now());
                self.proxy_log_cache.insert(result.key, result.rows);
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                self.proxy_log_refresh = None;
            }
        }
    }

    fn start_runtime_command(
        &mut self,
        paths: &Paths,
        config: &AppConfig,
        command: RuntimeCommand,
        success: &str,
        exit_after: bool,
    ) {
        if self.runtime_command.is_some() {
            return;
        }
        if let Some(task) = self.runtime_refresh.take() {
            task.handle.abort();
        }

        let paths = paths.clone();
        let config = config.clone();
        let (sender, receiver) = mpsc::channel();
        let handle = tokio::spawn(async move {
            let result = run_runtime_command_task(paths, config, command).await;
            let _ = sender.send(result);
        });
        self.runtime_command = Some(RuntimeCommandTask {
            receiver,
            handle,
            started_at: Instant::now(),
            command,
            success: success.into(),
            exit_after,
        });
        self.status = format!("{}...", command.title());
    }

    async fn poll_runtime_command(&mut self, paths: &Paths, config: &AppConfig) {
        let Some(task) = self.runtime_command.as_mut() else {
            return;
        };
        let result = match task.receiver.try_recv() {
            Ok(result) => result,
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => RuntimeCommandResult {
                success: false,
                message: Some("runtime task stopped before reporting a result".into()),
            },
        };
        let Some(task) = self.runtime_command.take() else {
            return;
        };
        task.handle.abort();

        if result.success {
            self.status = result.message.unwrap_or(task.success);
            if task.exit_after {
                self.should_quit = true;
            } else {
                self.start_runtime_refresh(paths, config);
            }
            return;
        }

        let message = result
            .message
            .unwrap_or_else(|| "restart failed without output".into());
        self.alert("Restart Failed", message.clone());
        self.status = format!(
            "{}; restart failed: {}",
            task.success,
            first_output_line(&message).unwrap_or("see restart output")
        );
    }

    fn start_chat_agent(
        &mut self,
        paths: &Paths,
        config: &AppConfig,
        conversation: Vec<agent::ConversationMessage>,
    ) {
        if self.chat.task.is_some() {
            self.status = text(
                self.language,
                "Assistant is already running",
                "Assistant 正在运行",
            )
            .into();
            return;
        }
        let paths = paths.clone();
        let config = config.clone();
        let language = self.language;
        let (sender, receiver) = mpsc::channel();
        let handle = tokio::spawn(async move {
            agent::run_agent(paths, config, conversation, language, sender).await;
        });
        self.chat.last_run_usage = agent::AgentUsage::default();
        self.chat.task = Some(ChatTask { receiver, handle });
        self.chat.entries.push(ChatEntry {
            kind: ChatEntryKind::Assistant,
            content: String::new(),
        });
        self.status = text(self.language, "Assistant thinking", "Assistant 思考中").into();
    }

    fn poll_chat(&mut self, config: &mut AppConfig) {
        let mut events = Vec::new();
        let mut disconnected = false;
        if let Some(task) = self.chat.task.as_mut() {
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
            self.apply_chat_event(config, event);
        }

        if disconnected {
            self.chat.task = None;
        }
    }

    fn apply_chat_event(&mut self, config: &mut AppConfig, event: AgentEvent) {
        match event {
            AgentEvent::Content(part) => {
                if !matches!(
                    self.chat.entries.last().map(|entry| entry.kind),
                    Some(ChatEntryKind::Assistant)
                ) {
                    self.chat.entries.push(ChatEntry {
                        kind: ChatEntryKind::Assistant,
                        content: String::new(),
                    });
                }
                if let Some(entry) = self.chat.entries.last_mut() {
                    entry.content.push_str(&part);
                }
                self.status = text(self.language, "Assistant streaming", "Assistant 输出中").into();
            }
            AgentEvent::Tool(message) => {
                self.chat.entries.push(ChatEntry {
                    kind: ChatEntryKind::Tool,
                    content: message.clone(),
                });
                self.status = message;
            }
            AgentEvent::Usage(usage) => {
                self.apply_chat_usage(usage);
            }
            AgentEvent::PatchReady(patch) => {
                if let Err(err) = apply_chat_patch_to_draft(config, self, &patch) {
                    let message = err.to_string();
                    self.chat.entries.push(ChatEntry {
                        kind: ChatEntryKind::Error,
                        content: message.clone(),
                    });
                    self.status = message;
                    self.chat.task = None;
                }
            }
            AgentEvent::Error(message) => {
                self.chat.entries.push(ChatEntry {
                    kind: ChatEntryKind::Error,
                    content: message.clone(),
                });
                self.status = message;
                self.chat.task = None;
            }
            AgentEvent::Done => {
                self.chat.task = None;
                if matches!(
                    self.chat.entries.last().map(|entry| entry.kind),
                    Some(ChatEntryKind::Assistant) | None
                ) {
                    self.status = text(self.language, "Assistant done", "Assistant 已完成").into();
                }
            }
        }
    }

    fn apply_chat_usage(&mut self, usage: agent::AgentUsage) {
        self.chat.usage.prompt_tokens = self.chat.usage.prompt_tokens.saturating_add(
            usage
                .prompt_tokens
                .saturating_sub(self.chat.last_run_usage.prompt_tokens),
        );
        self.chat.usage.completion_tokens = self.chat.usage.completion_tokens.saturating_add(
            usage
                .completion_tokens
                .saturating_sub(self.chat.last_run_usage.completion_tokens),
        );
        self.chat.usage.context_tokens = usage.context_tokens;
        self.chat.usage.context_chars = usage.context_chars;
        self.chat.usage.context_messages = usage.context_messages;
        self.chat.usage.estimated |= usage.estimated;
        self.chat.usage.turns = self
            .chat
            .usage
            .turns
            .saturating_add(usage.turns.saturating_sub(self.chat.last_run_usage.turns));
        self.chat.usage.tool_calls = self.chat.usage.tool_calls.saturating_add(
            usage
                .tool_calls
                .saturating_sub(self.chat.last_run_usage.tool_calls),
        );
        self.chat.last_run_usage = usage;
    }

    fn cancel_chat(&mut self) {
        if let Some(task) = self.chat.task.take() {
            task.handle.abort();
            self.chat.entries.push(ChatEntry {
                kind: ChatEntryKind::Tool,
                content: text(self.language, "assistant canceled", "assistant 已取消").into(),
            });
            self.status = text(self.language, "Assistant canceled", "Assistant 已取消").into();
        }
    }

    fn start_assistant_test(&mut self, paths: &Paths, config: &AppConfig) {
        if self.assistant_test.is_some() {
            self.status = text(
                self.language,
                "Assistant test is already running",
                "Assistant test 正在运行",
            )
            .into();
            return;
        }
        let paths = paths.clone();
        let config = config.clone();
        let language = self.language;
        let (sender, receiver) = mpsc::channel();
        let handle = tokio::spawn(async move {
            run_assistant_test_task(paths, config, language, sender).await;
        });
        self.assistant_test = Some(AssistantTestTask {
            receiver,
            handle,
            started_at: Instant::now(),
            response: String::new(),
            error: None,
            finished: false,
        });
        self.status = text(
            self.language,
            "Testing assistant connection",
            "正在测试 Assistant 连接",
        )
        .into();
    }

    fn poll_assistant_test(&mut self) {
        let mut events = Vec::new();
        let mut disconnected = false;
        if let Some(task) = self.assistant_test.as_mut() {
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

        if let Some(task) = self.assistant_test.as_mut() {
            for event in events {
                match event {
                    AssistantTestEvent::Content(part) => {
                        task.response.push_str(&part);
                        self.status = text(
                            self.language,
                            "Assistant test streaming",
                            "Assistant test 输出中",
                        )
                        .into();
                    }
                    AssistantTestEvent::Error(message) => {
                        task.error = Some(message);
                        task.finished = true;
                        self.status = text(
                            self.language,
                            "Assistant test failed",
                            "Assistant test 失败",
                        )
                        .into();
                    }
                    AssistantTestEvent::Done => {
                        task.finished = true;
                        if task.error.is_none() {
                            self.status = text(
                                self.language,
                                "Assistant test finished",
                                "Assistant test 已完成",
                            )
                            .into();
                        }
                    }
                }
            }

            if disconnected {
                task.finished = true;
                if task.response.trim().is_empty() && task.error.is_none() {
                    task.error = Some(
                        text(
                            self.language,
                            "assistant test stopped before returning output",
                            "assistant test 在返回内容前停止",
                        )
                        .into(),
                    );
                    self.status = text(
                        self.language,
                        "Assistant test failed",
                        "Assistant test 失败",
                    )
                    .into();
                }
            }
        }
    }

    fn assistant_test_finished(&self) -> bool {
        self.assistant_test
            .as_ref()
            .is_some_and(|task| task.finished)
    }

    fn close_assistant_test(&mut self) {
        if self.assistant_test_finished() {
            self.assistant_test = None;
        }
    }

    fn cancel_assistant_test(&mut self) {
        if let Some(task) = self.assistant_test.take() {
            task.handle.abort();
            self.status = text(
                self.language,
                "Assistant test canceled",
                "Assistant test 已取消",
            )
            .into();
        }
    }

    fn apply_runtime_refresh(&mut self, config: &AppConfig, mut result: RuntimeRefreshResult) {
        let proxy_selection = self.runtime_proxy_selection_for_refresh(config);
        estimate_runtime_traffic_speeds(Some(&self.runtime), &mut result.runtime);
        for (key, runtime) in &mut result.proxy_runtimes {
            estimate_runtime_traffic_speeds(self.proxy_runtimes.get(key), runtime);
        }

        if !result.refreshed_ip_info {
            preserve_ip_info(&self.runtime, &mut result.runtime);
            for (key, runtime) in &mut result.proxy_runtimes {
                if let Some(previous) = self.proxy_runtimes.get(key) {
                    preserve_ip_info(previous, runtime);
                }
            }
        }

        let refreshed_ip_info = result.refreshed_ip_info;
        self.runtime = result.runtime;
        self.proxy_runtimes = result.proxy_runtimes;
        self.runtime_checked = true;
        self.restore_runtime_proxy_selection_after_refresh(config, proxy_selection);

        if refreshed_ip_info {
            self.last_ip_info_refresh = Some(Instant::now());
        }
    }

    fn should_refresh_ip_info(&self) -> bool {
        match self.last_ip_info_refresh {
            Some(last_refresh) => {
                let elapsed = last_refresh.elapsed();
                elapsed >= IPINFO_REFRESH_INTERVAL
                    || (elapsed >= IPINFO_RETRY_INTERVAL && self.has_failed_ip_info())
            }
            None => true,
        }
    }

    fn has_failed_ip_info(&self) -> bool {
        self.runtime.ip_info_error.is_some()
            || self
                .proxy_runtimes
                .values()
                .any(|runtime| runtime.ip_info_error.is_some())
    }

    fn start_delay_check(
        &mut self,
        subscription_name: String,
        provider_name: String,
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
            provider_name,
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

    fn current_group<'a>(&'a self, config: &AppConfig) -> Option<&'a ProxyGroup> {
        selected_mihomo_runtime(config, self)
            .groups
            .get(self.selected_group)
    }

    fn selected_runtime_item(&self) -> RuntimeItem {
        let index = nearest_selectable_runtime_index(self.selected_runtime);
        RuntimeItem::ALL
            .get(index)
            .copied()
            .unwrap_or(RuntimeItem::Service)
    }

    fn selected_dns_item(&self) -> DnsItem {
        DnsItem::ALL
            .get(self.selected_dns)
            .copied()
            .unwrap_or(DnsItem::Enabled)
    }

    fn selected_exit_item(&self) -> ExitItem {
        let selected_exit = nearest_selectable_exit_index(self.selected_exit);
        ExitItem::ALL
            .get(selected_exit)
            .copied()
            .unwrap_or(ExitItem::StartRuntime)
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

    fn selected_profile_config_field(&self) -> ProfileConfigField {
        ProfileConfigField::ALL
            .get(self.selected_profile_field)
            .copied()
            .unwrap_or(ProfileConfigField::Name)
    }

    fn clamp_selection(&mut self, paths: &Paths, config: &AppConfig) {
        clamp_index(&mut self.selected_main, main_proxy_count(config));
        match self.page {
            Page::Main => clamp_index(&mut self.selected_main, main_proxy_count(config)),
            Page::Profile => {
                clamp_index(&mut self.selected_profile, proxy_profile_menu_count(config))
            }
            Page::ProfileConfig => {
                clamp_index(&mut self.selected_profile, config.proxy_profiles.len());
                clamp_index(
                    &mut self.selected_profile_field,
                    ProfileConfigField::ALL.len(),
                );
            }
            Page::Subscription => clamp_index(
                &mut self.selected_subscription,
                subscription_menu_count(config),
            ),
            Page::SubscriptionDetail => clamp_index(
                &mut self.selected_subscription_detail,
                subscription_detail_count(),
            ),
            Page::SubscriptionRuleGroups => {
                let count = subscription_rule_group_count(paths, config, self);
                clamp_index(&mut self.selected_subscription_rule_group, count);
            }
            Page::SubscriptionProxies => {
                let count = subscription_proxy_count(paths, config, self);
                clamp_index(&mut self.selected_subscription_proxy, count);
            }
            Page::SubscriptionRules => {
                let count = subscription_rule_count(paths, config, self);
                clamp_index(&mut self.selected_subscription_rule, count);
            }
            Page::AddSubscription => clamp_index(
                &mut self.subscription_form.selected,
                SubscriptionFormField::ALL.len(),
            ),
            Page::Runtime => {
                self.selected_runtime = nearest_selectable_runtime_index(self.selected_runtime);
            }
            Page::Exit => {
                self.selected_exit = nearest_selectable_exit_index(self.selected_exit);
            }
            Page::ProxyConfig => {
                let field_count = self.proxy_config_fields().len();
                clamp_index(&mut self.selected_proxy_field, field_count);
            }
            Page::ProxyConnections => {
                let count = proxy_connection_rows(config, self).len().max(1);
                clamp_index(&mut self.selected_proxy_connection, count);
            }
            Page::ProxyLogs => {
                let count = proxy_log_rows(paths, config, self).len().max(1);
                clamp_index(&mut self.selected_proxy_log, count);
            }
            Page::Mode => clamp_index(&mut self.selected_mode, ModeItem::ALL.len()),
            Page::Dns => clamp_index(&mut self.selected_dns, DnsItem::ALL.len()),
            Page::Chat | Page::ProxyGroups => {}
        }
        let dropdown_count = self.dropdown_item_count(config);
        if self.dropdown.is_some() {
            clamp_index(&mut self.selected_dropdown, dropdown_count);
        }
        if self.page == Page::ProxyGroups && self.rule_group_selection.is_some() {
            let proxy_count = subscription_rule_proxy_count(paths, config, self);
            clamp_index(&mut self.selected_proxy, proxy_count);
        } else if self.page == Page::ProxyGroups
            && proxy_groups_page_is_global_proxy_list(config, self)
        {
            let proxy_count = route_proxy_names(paths, config, self).len();
            clamp_index(&mut self.selected_proxy, proxy_count);
        } else if self.page == Page::ProxyGroups {
            self.clamp_runtime_selection(config);
        }
    }

    fn clamp_runtime_selection(&mut self, config: &AppConfig) {
        let groups_len = selected_mihomo_runtime(config, self).groups.len();
        if groups_len == 0 {
            self.selected_group = 0;
            self.selected_proxy = 0;
            return;
        }
        if self.selected_group >= groups_len {
            self.selected_group = groups_len - 1;
        }
        let proxy_count = selected_mihomo_runtime(config, self).groups[self.selected_group]
            .all
            .len();
        if proxy_count == 0 {
            self.selected_proxy = 0;
        } else if self.selected_proxy >= proxy_count {
            self.selected_proxy = selected_mihomo_runtime(config, self)
                .groups
                .get(self.selected_group)
                .and_then(|group| group.all.iter().position(|proxy| proxy == &group.now))
                .unwrap_or(0);
        }
    }

    fn uses_runtime_proxy_group_selection(&self, config: &AppConfig) -> bool {
        self.page == Page::ProxyGroups
            && self.rule_group_selection.is_none()
            && !proxy_groups_page_is_global_proxy_list(config, self)
    }

    fn runtime_proxy_selection_for_refresh(
        &self,
        config: &AppConfig,
    ) -> Option<(String, Option<String>)> {
        if !self.uses_runtime_proxy_group_selection(config) {
            return None;
        }

        let group = self.current_group(config)?;
        Some((
            group.name.clone(),
            group.all.get(self.selected_proxy).cloned(),
        ))
    }

    fn restore_runtime_proxy_selection_after_refresh(
        &mut self,
        config: &AppConfig,
        selection: Option<(String, Option<String>)>,
    ) {
        if !self.uses_runtime_proxy_group_selection(config) {
            return;
        }
        if selected_mihomo_runtime(config, self).groups.is_empty() {
            return;
        }

        if let Some((group_name, proxy_name)) = selection {
            let restored = {
                let runtime = selected_mihomo_runtime(config, self);
                runtime
                    .groups
                    .iter()
                    .position(|group| group.name == group_name)
                    .map(|group_index| {
                        let group = &runtime.groups[group_index];
                        let proxy_index = proxy_name.as_deref().and_then(|proxy| {
                            group.all.iter().position(|candidate| candidate == proxy)
                        });
                        let fallback_proxy_index =
                            current_proxy_index(Some(group)).unwrap_or_default();
                        (
                            group_index,
                            proxy_index,
                            group.all.len(),
                            fallback_proxy_index,
                        )
                    })
            };

            if let Some((group_index, proxy_index, proxy_count, fallback_proxy_index)) = restored {
                self.selected_group = group_index;
                if let Some(proxy_index) = proxy_index {
                    self.selected_proxy = proxy_index;
                    return;
                }
                if proxy_count == 0 {
                    return;
                }
                if self.selected_proxy >= proxy_count {
                    self.selected_proxy = fallback_proxy_index;
                }
                return;
            }
        }

        self.clamp_runtime_selection(config);
    }

    fn selected_index(&self) -> usize {
        match self.page {
            Page::Main => self.selected_main,
            Page::Profile => self.selected_profile,
            Page::ProfileConfig => self.selected_profile_field,
            Page::Subscription => self.selected_subscription,
            Page::SubscriptionDetail => self.selected_subscription_detail,
            Page::SubscriptionRuleGroups => self.selected_subscription_rule_group,
            Page::SubscriptionProxies => self.selected_subscription_proxy,
            Page::SubscriptionRules => self.selected_subscription_rule,
            Page::AddSubscription => self.subscription_form.selected,
            Page::Runtime => self.selected_runtime,
            Page::Exit => self.selected_exit,
            Page::ProxyConfig => self.selected_proxy_field,
            Page::ProxyConnections => self.selected_proxy_connection,
            Page::ProxyLogs => self.selected_proxy_log,
            Page::ProxyGroups if self.rule_group_selection.is_some() => self.selected_proxy,
            Page::ProxyGroups => match self.proxy_pane {
                ProxyPane::Groups => self.selected_group,
                ProxyPane::Proxies => self.selected_proxy,
            },
            Page::Mode => self.selected_mode,
            Page::Dns => self.selected_dns,
            Page::Chat => 0,
        }
    }

    fn selection_count(&self, paths: &Paths, config: &AppConfig) -> usize {
        match self.page {
            Page::Main => main_proxy_count(config),
            Page::Profile => proxy_profile_menu_count(config),
            Page::ProfileConfig => ProfileConfigField::ALL.len(),
            Page::Subscription => subscription_menu_count(config),
            Page::SubscriptionDetail => subscription_detail_count(),
            Page::SubscriptionRuleGroups => subscription_rule_group_count(paths, config, self),
            Page::SubscriptionProxies => subscription_proxy_count(paths, config, self),
            Page::SubscriptionRules => subscription_rule_count(paths, config, self),
            Page::AddSubscription => SubscriptionFormField::ALL.len(),
            Page::Runtime => RuntimeItem::ALL.len(),
            Page::Exit => ExitItem::ALL.len(),
            Page::ProxyConfig => self.proxy_config_fields().len(),
            Page::ProxyConnections => proxy_connection_rows(config, self).len().max(1),
            Page::ProxyLogs => proxy_log_rows(paths, config, self).len().max(1),
            Page::ProxyGroups if self.rule_group_selection.is_some() => {
                subscription_rule_proxy_count(paths, config, self)
            }
            Page::ProxyGroups => match self.proxy_pane {
                ProxyPane::Groups => selected_mihomo_runtime(config, self).groups.len(),
                ProxyPane::Proxies if proxy_groups_page_is_global_proxy_list(config, self) => {
                    route_proxy_names(paths, config, self).len()
                }
                ProxyPane::Proxies => self
                    .current_group(config)
                    .map_or(0, |group| group.all.len()),
            },
            Page::Mode => ModeItem::ALL.len(),
            Page::Dns => DnsItem::ALL.len(),
            Page::Chat => 0,
        }
    }

    fn set_selected_index(&mut self, selected: usize) {
        match self.page {
            Page::Main => self.selected_main = selected,
            Page::Profile => self.selected_profile = selected,
            Page::ProfileConfig => self.selected_profile_field = selected,
            Page::Subscription => self.selected_subscription = selected,
            Page::SubscriptionDetail => self.selected_subscription_detail = selected,
            Page::SubscriptionRuleGroups => self.selected_subscription_rule_group = selected,
            Page::SubscriptionProxies => self.selected_subscription_proxy = selected,
            Page::SubscriptionRules => self.selected_subscription_rule = selected,
            Page::AddSubscription => self.subscription_form.selected = selected,
            Page::Runtime => self.selected_runtime = nearest_selectable_runtime_index(selected),
            Page::Exit => self.selected_exit = nearest_selectable_exit_index(selected),
            Page::ProxyConfig => self.selected_proxy_field = selected,
            Page::ProxyConnections => self.selected_proxy_connection = selected,
            Page::ProxyLogs => self.selected_proxy_log = selected,
            Page::ProxyGroups if self.rule_group_selection.is_some() => {
                self.selected_proxy = selected;
            }
            Page::ProxyGroups => match self.proxy_pane {
                ProxyPane::Groups => self.selected_group = selected,
                ProxyPane::Proxies => self.selected_proxy = selected,
            },
            Page::Mode => self.selected_mode = selected,
            Page::Dns => self.selected_dns = selected,
            Page::Chat => {}
        }
    }

    fn move_page_next(&mut self, paths: &Paths, config: &AppConfig) {
        self.move_page(paths, config, true);
    }

    fn move_page_prev(&mut self, paths: &Paths, config: &AppConfig) {
        self.move_page(paths, config, false);
    }

    fn move_page(&mut self, paths: &Paths, config: &AppConfig, forward: bool) {
        let len = self.selection_count(paths, config);
        if len == 0 {
            return;
        }
        let selected = self.selected_index().min(len - 1);
        let next = if forward {
            page_down_index(selected, len, terminal_page_step())
        } else {
            page_up_index(selected, terminal_page_step())
        };
        if self.page == Page::Exit {
            self.selected_exit = selectable_exit_index_from(next, forward);
        } else {
            self.set_selected_index(next);
        }
        if self.page == Page::ProxyGroups
            && self.rule_group_selection.is_none()
            && self.proxy_pane == ProxyPane::Groups
        {
            self.selected_proxy =
                current_proxy_index(self.current_group(config)).unwrap_or_default();
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
        if self.page != Page::ProxyGroups {
            self.rule_group_selection = None;
        }
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
        if page != Page::ProxyGroups {
            self.rule_group_selection = None;
        }
        self.status = status.into();
    }

    fn go_back(&mut self) -> bool {
        self.dropdown = None;
        if let Some(location) = self.history.pop() {
            self.restore_location(location);
            self.status = if self.language.is_zh_cn() {
                format!("返回 {}", self.page.title_for(self.language))
            } else {
                format!("Back to {}", self.page.title())
            };
            return true;
        }
        false
    }

    fn back_or_exit_screen(&mut self) {
        if self.go_back() {
            return;
        }
        if self.page == Page::Exit {
            self.open_confirm(ConfirmAction::ExitWithoutSaving);
        } else {
            self.switch_section(Page::Exit);
            self.status = text(self.language, "Choose an exit action", "选择一个退出操作").into();
        }
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
            self.status = text(
                self.language,
                "Back to section root before switching sections",
                "切换 section 前请先返回 section root",
            )
            .into();
            return;
        }
        self.section = section;
        self.page = section;
        self.set_selected_index(0);
        if section == Page::Exit {
            self.selected_exit = 1;
        }
        self.rule_group_selection = None;
        self.status.clear();
    }

    fn next_page(&mut self) {
        self.switch_section(self.section.next());
    }

    fn prev_page(&mut self) {
        self.switch_section(self.section.prev());
    }

    fn move_right(&mut self, config: &AppConfig) {
        if self.page == Page::ProxyGroups
            && (self.rule_group_selection.is_some()
                || proxy_groups_page_is_global_proxy_list(config, self))
        {
            self.status = text(
                self.language,
                "Select a proxy, then press Enter",
                "选择一个 proxy，然后按 Enter",
            )
            .into();
        } else if self.page == Page::ProxyGroups && self.proxy_pane == ProxyPane::Groups {
            self.proxy_pane = ProxyPane::Proxies;
            self.selected_proxy =
                current_proxy_index(self.current_group(config)).unwrap_or_default();
            self.status = text(self.language, "Proxy pane: Proxies", "Proxy pane: Proxies").into();
        } else {
            self.next_page();
        }
    }

    fn move_left(&mut self, config: &AppConfig) {
        if self.page == Page::ProxyGroups
            && (self.rule_group_selection.is_some()
                || proxy_groups_page_is_global_proxy_list(config, self))
        {
            self.status = text(
                self.language,
                "Select a proxy, then press Enter",
                "选择一个 proxy，然后按 Enter",
            )
            .into();
        } else if self.page == Page::ProxyGroups && self.proxy_pane == ProxyPane::Proxies {
            self.proxy_pane = ProxyPane::Groups;
            self.status = text(self.language, "Proxy pane: Groups", "Proxy pane: Groups").into();
        } else {
            self.prev_page();
        }
    }

    fn move_next(&mut self, paths: &Paths, config: &AppConfig) {
        match self.page {
            Page::Main => {
                self.selected_main = next_index(self.selected_main, main_proxy_count(config))
            }
            Page::Profile => {
                self.selected_profile =
                    next_index(self.selected_profile, proxy_profile_menu_count(config))
            }
            Page::ProfileConfig => {
                self.selected_profile_field =
                    next_index(self.selected_profile_field, ProfileConfigField::ALL.len())
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
                    subscription_rule_group_count(paths, config, self),
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
                    subscription_rule_count(paths, config, self),
                )
            }
            Page::AddSubscription => self.subscription_form.next_field(),
            Page::ProxyConfig => {
                self.selected_proxy_field =
                    next_index(self.selected_proxy_field, self.proxy_config_fields().len())
            }
            Page::ProxyConnections => {
                self.selected_proxy_connection = next_index(
                    self.selected_proxy_connection,
                    proxy_connection_rows(config, self).len().max(1),
                )
            }
            Page::ProxyLogs => {
                self.selected_proxy_log = next_index(
                    self.selected_proxy_log,
                    proxy_log_rows(paths, config, self).len().max(1),
                )
            }
            Page::ProxyGroups if self.rule_group_selection.is_some() => {
                self.selected_proxy = next_index(
                    self.selected_proxy,
                    subscription_rule_proxy_count(paths, config, self),
                )
            }
            Page::ProxyGroups => match self.proxy_pane {
                ProxyPane::Groups => self.select_next_group(config),
                ProxyPane::Proxies if proxy_groups_page_is_global_proxy_list(config, self) => {
                    self.selected_proxy = next_index(
                        self.selected_proxy,
                        route_proxy_names(paths, config, self).len(),
                    )
                }
                ProxyPane::Proxies => self.select_next_proxy(config),
            },
            Page::Mode => self.selected_mode = next_index(self.selected_mode, ModeItem::ALL.len()),
            Page::Dns => self.selected_dns = next_index(self.selected_dns, DnsItem::ALL.len()),
            Page::Runtime => {
                self.selected_runtime = next_selectable_runtime_index(self.selected_runtime)
            }
            Page::Exit => {
                self.selected_exit = next_selectable_exit_index(self.selected_exit);
            }
            Page::Chat => {}
        }
    }

    fn move_prev(&mut self, paths: &Paths, config: &AppConfig) {
        match self.page {
            Page::Main => {
                self.selected_main = prev_index(self.selected_main, main_proxy_count(config))
            }
            Page::Profile => {
                self.selected_profile =
                    prev_index(self.selected_profile, proxy_profile_menu_count(config))
            }
            Page::ProfileConfig => {
                self.selected_profile_field =
                    prev_index(self.selected_profile_field, ProfileConfigField::ALL.len())
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
                    subscription_rule_group_count(paths, config, self),
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
                    subscription_rule_count(paths, config, self),
                )
            }
            Page::AddSubscription => self.subscription_form.prev_field(),
            Page::ProxyConfig => {
                self.selected_proxy_field =
                    prev_index(self.selected_proxy_field, self.proxy_config_fields().len())
            }
            Page::ProxyConnections => {
                self.selected_proxy_connection = prev_index(
                    self.selected_proxy_connection,
                    proxy_connection_rows(config, self).len().max(1),
                )
            }
            Page::ProxyLogs => {
                self.selected_proxy_log = prev_index(
                    self.selected_proxy_log,
                    proxy_log_rows(paths, config, self).len().max(1),
                )
            }
            Page::ProxyGroups if self.rule_group_selection.is_some() => {
                self.selected_proxy = prev_index(
                    self.selected_proxy,
                    subscription_rule_proxy_count(paths, config, self),
                )
            }
            Page::ProxyGroups => match self.proxy_pane {
                ProxyPane::Groups => self.select_prev_group(config),
                ProxyPane::Proxies if proxy_groups_page_is_global_proxy_list(config, self) => {
                    self.selected_proxy = prev_index(
                        self.selected_proxy,
                        route_proxy_names(paths, config, self).len(),
                    )
                }
                ProxyPane::Proxies => self.select_prev_proxy(config),
            },
            Page::Mode => self.selected_mode = prev_index(self.selected_mode, ModeItem::ALL.len()),
            Page::Dns => self.selected_dns = prev_index(self.selected_dns, DnsItem::ALL.len()),
            Page::Runtime => {
                self.selected_runtime = prev_selectable_runtime_index(self.selected_runtime)
            }
            Page::Exit => {
                self.selected_exit = prev_selectable_exit_index(self.selected_exit);
            }
            Page::Chat => {}
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

    fn select_next_group(&mut self, config: &AppConfig) {
        let groups_len = selected_mihomo_runtime(config, self).groups.len();
        if groups_len == 0 {
            return;
        }
        self.selected_group = (self.selected_group + 1) % groups_len;
        self.selected_proxy = current_proxy_index(self.current_group(config)).unwrap_or_default();
    }

    fn select_prev_group(&mut self, config: &AppConfig) {
        let groups_len = selected_mihomo_runtime(config, self).groups.len();
        if groups_len == 0 {
            return;
        }
        self.selected_group = if self.selected_group == 0 {
            groups_len - 1
        } else {
            self.selected_group - 1
        };
        self.selected_proxy = current_proxy_index(self.current_group(config)).unwrap_or_default();
    }

    fn select_next_proxy(&mut self, config: &AppConfig) {
        let Some(group) = self.current_group(config) else {
            return;
        };
        if group.all.is_empty() {
            return;
        }
        self.selected_proxy = (self.selected_proxy + 1) % group.all.len();
    }

    fn select_prev_proxy(&mut self, config: &AppConfig) {
        let Some(group) = self.current_group(config) else {
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
        self.selected_dropdown = if self.page == Page::ProfileConfig {
            proxy_profile_subscription_index(config, self).unwrap_or_default()
        } else {
            proxy_subscription_index(config, self).unwrap_or_default()
        };
        self.status = text(
            self.language,
            "Choose subscription, then press Enter",
            "选择订阅，然后按 Enter",
        )
        .into();
    }

    fn open_proxy_profile_dropdown(&mut self, config: &AppConfig) {
        self.dropdown = Some(Dropdown::ProxyProfile);
        self.selected_dropdown = active_proxy_profile_index(config).unwrap_or_default();
        self.status = text(
            self.language,
            "Choose profile, then press Enter",
            "选择 Profile，然后按 Enter",
        )
        .into();
    }

    fn open_mode_dropdown(&mut self, config: &AppConfig) {
        self.dropdown = Some(Dropdown::Mode);
        self.selected_dropdown = mode_index(proxy_mode(config, self));
        self.status = text(
            self.language,
            "Choose mode, then press Enter",
            "选择 Mode，然后按 Enter",
        )
        .into();
    }

    fn open_core_source_dropdown(&mut self, config: &AppConfig) {
        self.dropdown = Some(Dropdown::CoreSource);
        self.selected_dropdown = core_source_index(core::selected_core_source(config));
        self.status = text(
            self.language,
            "Choose mihomo core, then press Enter",
            "选择 mihomo core，然后按 Enter",
        )
        .into();
    }

    fn open_llm_provider_dropdown(&mut self, config: &AppConfig) {
        self.dropdown = Some(Dropdown::LlmProvider);
        self.selected_dropdown = llm_provider_index(&config.llm.provider);
        self.status = text(
            self.language,
            "Choose LLM provider, then press Enter",
            "选择 LLM provider，然后按 Enter",
        )
        .into();
    }

    fn open_llm_model_dropdown(&mut self, config: &AppConfig) {
        self.dropdown = Some(Dropdown::LlmModel);
        self.selected_dropdown = llm_model_index(config);
        self.status = text(
            self.language,
            "Choose LLM model, then press Enter",
            "选择 LLM model，然后按 Enter",
        )
        .into();
    }

    fn open_subscription_refresh_dropdown(&mut self, config: &AppConfig) {
        self.dropdown = Some(Dropdown::SubscriptionRefresh);
        let refresh = if self.page == Page::SubscriptionDetail {
            selected_subscription(config, self)
                .map_or(self.subscription_form.refresh, |sub| sub.refresh)
        } else {
            self.subscription_form.refresh
        };
        self.selected_dropdown = subscription_refresh_index(refresh);
        self.status = text(
            self.language,
            "Choose refresh interval, then press Enter",
            "选择刷新间隔，然后按 Enter",
        )
        .into();
    }

    fn close_dropdown(&mut self) {
        self.dropdown = None;
        self.status = text(self.language, "Canceled", "已取消").into();
    }

    fn dropdown_item_count(&self, config: &AppConfig) -> usize {
        match self.dropdown {
            Some(Dropdown::ProxyProfile) => config.proxy_profiles.len().max(1),
            Some(Dropdown::ProxySubscription) => subscription_dropdown_count(config),
            Some(Dropdown::Mode) => ModeItem::ALL.len(),
            Some(Dropdown::SubscriptionRefresh) => SUBSCRIPTION_REFRESH_OPTIONS.len(),
            Some(Dropdown::CoreSource) => core::CoreSource::ALL.len(),
            Some(Dropdown::LlmProvider) => llm_providers::presets().len(),
            Some(Dropdown::LlmModel) => llm_model_dropdown_options(config).len(),
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

    fn select_next_dropdown_page(&mut self, config: &AppConfig) {
        let count = self.dropdown_item_count(config);
        if count == 0 {
            return;
        }
        self.selected_dropdown = page_down_index(
            self.selected_dropdown.min(count - 1),
            count,
            terminal_page_step(),
        );
    }

    fn select_prev_dropdown_page(&mut self, config: &AppConfig) {
        let count = self.dropdown_item_count(config);
        if count == 0 {
            return;
        }
        self.selected_dropdown =
            page_up_index(self.selected_dropdown.min(count - 1), terminal_page_step());
    }

    const fn is_input(&self) -> bool {
        !matches!(self.input_mode, InputMode::Normal)
    }

    fn cancel_input(&mut self) {
        self.input_mode = InputMode::Normal;
        self.input_focus = InputFocus::Editor;
        self.input.clear();
        self.input_cursor = 0;
        self.input_desired_column = None;
        self.status = "Canceled".into();
    }

    fn begin_input(
        &mut self,
        mode: InputMode,
        value: impl Into<String>,
        status: impl Into<String>,
    ) {
        self.input_mode = mode;
        self.input_focus = InputFocus::Editor;
        self.input = value.into();
        self.input_cursor = self.input.len();
        self.input_desired_column = None;
        self.status = status.into();
    }

    fn finish_input(&mut self) {
        self.input.clear();
        self.input_cursor = 0;
        self.input_desired_column = None;
        self.input_mode = InputMode::Normal;
        self.input_focus = InputFocus::Editor;
    }

    fn next_input_focus(&mut self) {
        self.input_focus = match self.input_focus {
            InputFocus::Editor => InputFocus::Save,
            InputFocus::Save => InputFocus::Cancel,
            InputFocus::Cancel => InputFocus::Editor,
        };
    }

    fn prev_input_focus(&mut self) {
        self.input_focus = match self.input_focus {
            InputFocus::Editor => InputFocus::Cancel,
            InputFocus::Save => InputFocus::Editor,
            InputFocus::Cancel => InputFocus::Save,
        };
    }

    fn open_confirm(&mut self, action: ConfirmAction) {
        self.confirm = Some(action);
        self.confirm_yes = false;
        self.status = if self.language.is_zh_cn() {
            format!("确认：{}", action.title_for(self.language))
        } else {
            format!("Confirm: {}", action.title())
        };
    }

    fn cancel_confirm(&mut self) {
        self.confirm = None;
        self.confirm_yes = false;
        self.status = text(self.language, "Canceled", "已取消").into();
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
    if app.runtime_command.is_some() {
        return Ok(());
    }

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

    if app.assistant_test.is_some() {
        app.poll_assistant_test();
        match key.code {
            KeyCode::Esc if app.assistant_test_finished() => app.close_assistant_test(),
            KeyCode::Enter | KeyCode::Char(' ') if app.assistant_test_finished() => {
                app.close_assistant_test()
            }
            KeyCode::Esc => app.cancel_assistant_test(),
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

    if app.page == Page::Chat {
        return handle_chat_key(paths, config, app, key).await;
    }

    match key.code {
        KeyCode::F(9) => app.open_confirm(ConfirmAction::LoadDefaults),
        KeyCode::F(10) => app.open_confirm(ConfirmAction::SaveRestart),
        KeyCode::Esc => app.back_or_exit_screen(),
        KeyCode::Tab => app.next_page(),
        KeyCode::BackTab => app.prev_page(),
        KeyCode::Right => app.move_right(config),
        KeyCode::Left => app.move_left(config),
        KeyCode::Down => app.move_next(paths, config),
        KeyCode::Up => app.move_prev(paths, config),
        KeyCode::PageDown => app.move_page_next(paths, config),
        KeyCode::PageUp => app.move_page_prev(paths, config),
        KeyCode::Char('o') | KeyCode::Char('O') => {
            if !quick_delay_check(paths, config, app).await? {
                toggle_selected_main_proxy(paths, config, app).await?
            }
        }
        KeyCode::Char('c') | KeyCode::Char('C') if app.page == Page::Main => {
            open_selected_main_proxy_config(config, app);
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
        KeyCode::PageDown => app.select_next_dropdown_page(config),
        KeyCode::PageUp => app.select_prev_dropdown_page(config),
        KeyCode::Enter | KeyCode::Char(' ') => submit_dropdown(paths, config, app).await?,
        _ => {}
    }
    app.clamp_selection(paths, config);
    Ok(())
}

async fn handle_paste(
    paths: &Paths,
    config: &mut AppConfig,
    app: &mut ConfigApp,
    value: &str,
) -> Result<()> {
    if app.runtime_command.is_some()
        || app.alert.is_some()
        || app.confirm.is_some()
        || app.assistant_test.is_some()
    {
        return Ok(());
    }
    let value = normalize_pasted_text(value);
    if app.is_input() {
        insert_input_text(app, &value);
    } else if app.page == Page::Chat {
        push_chat_input(app, &value);
    } else if app.page == Page::AddSubscription {
        handle_subscription_form_paste(paths, config, app, &value).await?;
    }
    Ok(())
}

fn normalize_pasted_text(value: &str) -> String {
    value.replace("\r\n", "\n").replace('\r', "\n")
}

async fn handle_input_key(
    paths: &Paths,
    config: &mut AppConfig,
    app: &mut ConfigApp,
    key: KeyEvent,
) -> Result<()> {
    if app.input_focus != InputFocus::Editor {
        match key.code {
            KeyCode::Esc => app.cancel_input(),
            KeyCode::Tab | KeyCode::Right | KeyCode::Down => app.next_input_focus(),
            KeyCode::BackTab | KeyCode::Left | KeyCode::Up => app.prev_input_focus(),
            KeyCode::Enter | KeyCode::Char(' ') => match app.input_focus {
                InputFocus::Save => submit_input(paths, config, app).await?,
                InputFocus::Cancel => app.cancel_input(),
                InputFocus::Editor => {}
            },
            _ => {}
        }
        return Ok(());
    }

    match key.code {
        KeyCode::Esc => app.cancel_input(),
        KeyCode::Tab => app.next_input_focus(),
        KeyCode::BackTab => app.prev_input_focus(),
        KeyCode::Char('s') | KeyCode::Char('S')
            if key.modifiers.contains(KeyModifiers::CONTROL) =>
        {
            submit_input(paths, config, app).await?
        }
        KeyCode::Backspace => delete_input_char_before_cursor(app),
        KeyCode::Delete => delete_input_char_at_cursor(app),
        KeyCode::Left => move_input_cursor_left(app),
        KeyCode::Right => move_input_cursor_right(app),
        KeyCode::Home => move_input_cursor_line_start(app),
        KeyCode::End => move_input_cursor_line_end(app),
        KeyCode::Up if is_multiline_input(app.input_mode) => move_input_cursor_line(app, -1),
        KeyCode::Down if is_multiline_input(app.input_mode) => move_input_cursor_line(app, 1),
        KeyCode::Enter if should_insert_input_newline(app.input_mode, key.modifiers) => {
            insert_input_text(app, "\n");
        }
        KeyCode::Char('j')
            if is_multiline_input(app.input_mode)
                && key.modifiers.contains(KeyModifiers::CONTROL) =>
        {
            insert_input_text(app, "\n");
        }
        KeyCode::Enter if is_multiline_input(app.input_mode) => insert_input_text(app, "\n"),
        KeyCode::Enter => {}
        KeyCode::Up if is_number_input(app.input_mode) => {
            adjust_number_input(&mut app.input, &mut app.input_cursor, 1);
        }
        KeyCode::Down if is_number_input(app.input_mode) => {
            adjust_number_input(&mut app.input, &mut app.input_cursor, -1);
        }
        KeyCode::Char(value) if is_number_input(app.input_mode) && value.is_ascii_digit() => {
            push_number_digit(&mut app.input, &mut app.input_cursor, value);
        }
        KeyCode::Char(value) => insert_input_char(app, value),
        _ => {}
    }
    Ok(())
}

async fn handle_chat_key(
    paths: &Paths,
    config: &mut AppConfig,
    app: &mut ConfigApp,
    key: KeyEvent,
) -> Result<()> {
    match key.code {
        KeyCode::F(10) => app.open_confirm(ConfirmAction::SaveRestart),
        KeyCode::Esc if app.chat.task.is_some() => app.cancel_chat(),
        KeyCode::Esc => app.back_or_exit_screen(),
        KeyCode::Tab => app.next_page(),
        KeyCode::BackTab => app.prev_page(),
        KeyCode::Right => app.move_right(config),
        KeyCode::Left => app.move_left(config),
        KeyCode::PageUp => {
            app.chat.scroll = app.chat.scroll.saturating_add(6);
        }
        KeyCode::PageDown => {
            app.chat.scroll = app.chat.scroll.saturating_sub(6);
        }
        KeyCode::Char('s') | KeyCode::Char('S')
            if key.modifiers.contains(KeyModifiers::CONTROL) =>
        {
            app.status = text(
                app.language,
                "Chat patches apply to draft automatically; press F10 to save and restart",
                "Chat patch 会自动应用到 draft；按 F10 保存并重启",
            )
            .into();
        }
        KeyCode::Char('u') | KeyCode::Char('U')
            if key.modifiers.contains(KeyModifiers::CONTROL) =>
        {
            app.chat.input.clear();
        }
        KeyCode::Char('j') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            push_chat_input(app, "\n");
        }
        KeyCode::Enter
            if key
                .modifiers
                .intersects(KeyModifiers::ALT | KeyModifiers::CONTROL | KeyModifiers::SHIFT) =>
        {
            push_chat_input(app, "\n");
        }
        KeyCode::Enter => send_chat_message(paths, config, app),
        KeyCode::Backspace => {
            app.chat.input.pop();
        }
        KeyCode::Char(value) if app.chat.input.len() < CHAT_INPUT_WIDTH => {
            app.chat.input.push(value);
        }
        _ => {}
    }
    Ok(())
}

fn push_chat_input(app: &mut ConfigApp, value: &str) {
    if app.chat.input.len().saturating_add(value.len()) <= CHAT_INPUT_WIDTH {
        app.chat.input.push_str(value);
    }
}

fn send_chat_message(paths: &Paths, config: &AppConfig, app: &mut ConfigApp) {
    if app.chat.task.is_some() {
        app.status = text(
            app.language,
            "Assistant is still running",
            "Assistant 仍在运行",
        )
        .into();
        return;
    }
    let message = app.chat.input.trim().to_string();
    if message.is_empty() {
        app.status = text(app.language, "Type a message first", "请先输入消息").into();
        return;
    }
    app.chat.input.clear();
    app.chat.entries.push(ChatEntry {
        kind: ChatEntryKind::User,
        content: message.clone(),
    });
    let conversation = chat_conversation_context(&app.chat.entries);
    app.start_chat_agent(paths, config, conversation);
}

fn chat_conversation_context(entries: &[ChatEntry]) -> Vec<agent::ConversationMessage> {
    entries
        .iter()
        .filter_map(|entry| {
            let content = entry.content.trim();
            if content.is_empty() {
                return None;
            }
            match entry.kind {
                ChatEntryKind::User => Some(agent::ConversationMessage::user(content.to_string())),
                ChatEntryKind::Assistant => {
                    Some(agent::ConversationMessage::assistant(content.to_string()))
                }
                ChatEntryKind::Patch => Some(agent::ConversationMessage::assistant(format!(
                    "Applied config patch:\n{content}"
                ))),
                ChatEntryKind::Tool | ChatEntryKind::Error => None,
            }
        })
        .collect()
}

fn apply_chat_patch_to_draft(
    config: &mut AppConfig,
    app: &mut ConfigApp,
    patch: &ConfigPatch,
) -> Result<()> {
    let updated = agent::apply_config_patch(config, patch)?;
    let diff = config_patch_diff(config, &updated)?;
    *config = updated;
    app.mark_dirty();
    app.chat.entries.push(ChatEntry {
        kind: ChatEntryKind::Patch,
        content: format!(
            "Applied to draft: {}\n{}\n\nPress F10 to save and restart the service.",
            patch.summary, diff
        ),
    });
    app.status = text(
        app.language,
        "Patch applied to draft; press F10 to save and restart",
        "Patch 已应用到 draft；按 F10 保存并重启",
    )
    .into();
    Ok(())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TextDiffKind {
    Equal,
    Remove,
    Add,
}

struct TextDiffLine {
    kind: TextDiffKind,
    text: String,
}

fn config_patch_diff(before: &AppConfig, after: &AppConfig) -> Result<String> {
    let before_yaml = serde_yaml_ng::to_string(before)?;
    let after_yaml = serde_yaml_ng::to_string(after)?;
    let before_lines = before_yaml.lines().collect::<Vec<_>>();
    let after_lines = after_yaml.lines().collect::<Vec<_>>();
    let diff = line_diff(&before_lines, &after_lines);
    let lines = contextual_diff_lines(&diff, CHAT_PATCH_DIFF_CONTEXT, CHAT_PATCH_DIFF_MAX_LINES);
    if lines.is_empty() {
        return Ok("(no YAML changes)".into());
    }

    Ok(lines.join("\n"))
}

fn line_diff(before: &[&str], after: &[&str]) -> Vec<TextDiffLine> {
    let rows = before.len();
    let cols = after.len() + 1;
    let mut lcs = vec![0usize; (rows + 1) * cols];

    for i in (0..before.len()).rev() {
        for j in (0..after.len()).rev() {
            let index = i * cols + j;
            lcs[index] = if before[i] == after[j] {
                lcs[(i + 1) * cols + j + 1] + 1
            } else {
                lcs[(i + 1) * cols + j].max(lcs[i * cols + j + 1])
            };
        }
    }

    let mut output = Vec::new();
    let mut i = 0usize;
    let mut j = 0usize;
    while i < before.len() && j < after.len() {
        if before[i] == after[j] {
            output.push(TextDiffLine {
                kind: TextDiffKind::Equal,
                text: before[i].to_string(),
            });
            i += 1;
            j += 1;
        } else if lcs[(i + 1) * cols + j] >= lcs[i * cols + j + 1] {
            output.push(TextDiffLine {
                kind: TextDiffKind::Remove,
                text: before[i].to_string(),
            });
            i += 1;
        } else {
            output.push(TextDiffLine {
                kind: TextDiffKind::Add,
                text: after[j].to_string(),
            });
            j += 1;
        }
    }

    output.extend(before[i..].iter().map(|line| TextDiffLine {
        kind: TextDiffKind::Remove,
        text: (*line).to_string(),
    }));
    output.extend(after[j..].iter().map(|line| TextDiffLine {
        kind: TextDiffKind::Add,
        text: (*line).to_string(),
    }));
    output
}

fn contextual_diff_lines(diff: &[TextDiffLine], context: usize, max_lines: usize) -> Vec<String> {
    let mut keep = vec![false; diff.len()];
    for (index, line) in diff.iter().enumerate() {
        if line.kind == TextDiffKind::Equal {
            continue;
        }
        let start = index.saturating_sub(context);
        let end = (index + context + 1).min(diff.len());
        for value in &mut keep[start..end] {
            *value = true;
        }
    }

    let mut output = Vec::new();
    let mut skipped = false;
    for (index, line) in diff.iter().enumerate() {
        if !keep[index] {
            skipped = true;
            continue;
        }
        if skipped && !output.is_empty() {
            output.push(" ...".to_string());
        }
        skipped = false;
        output.push(format!("{}{}", diff_prefix(line.kind), line.text));
        if output.len() >= max_lines {
            output.push(" ... [diff truncated]".to_string());
            break;
        }
    }
    output
}

fn diff_prefix(kind: TextDiffKind) -> &'static str {
    match kind {
        TextDiffKind::Equal => " ",
        TextDiffKind::Remove => "-",
        TextDiffKind::Add => "+",
    }
}

async fn run_assistant_test_task(
    paths: Paths,
    config: AppConfig,
    language: Language,
    sender: mpsc::Sender<AssistantTestEvent>,
) {
    if let Err(err) = run_assistant_test_inner(paths, config, language, &sender).await {
        let _ = sender.send(AssistantTestEvent::Error(err.to_string()));
    }
    let _ = sender.send(AssistantTestEvent::Done);
}

async fn run_assistant_test_inner(
    paths: Paths,
    config: AppConfig,
    language: Language,
    sender: &mpsc::Sender<AssistantTestEvent>,
) -> Result<()> {
    let api_key = agent::resolve_api_key(&paths, &config).await?;
    if config.llm.model.trim().is_empty() {
        anyhow::bail!("LLM model is not configured");
    }

    let client = LlmClient::new(&config.llm.base_url, api_key);
    let completion = client
        .stream_chat_completion(
            &config.llm.model,
            &[
                LlmMessage::system(text(
                    language,
                    "Reply to the user's greeting in one short sentence. Keep technical terms unchanged.",
                    "用一句简短中文回复用户问候，保留专业术语。",
                )),
                LlmMessage::user("hello"),
            ],
            &[],
            |part| {
                let _ = sender.send(AssistantTestEvent::Content(part));
            },
        )
        .await?;

    if completion.content.trim().is_empty() {
        anyhow::bail!("assistant test returned an empty response");
    }
    Ok(())
}

async fn handle_subscription_form_paste(
    _paths: &Paths,
    _config: &mut AppConfig,
    app: &mut ConfigApp,
    value: &str,
) -> Result<()> {
    match app.subscription_form.selected_field() {
        SubscriptionFormField::Name => app.subscription_form.name.push_str(value),
        SubscriptionFormField::Url => app.subscription_form.url.push_str(value),
        SubscriptionFormField::Refresh | SubscriptionFormField::Ok => {}
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
            app.should_quit = true;
        }
        ConfirmAction::LoadDefaults => {
            app.status = "Load defaults is not implemented in this preview".into();
        }
        ConfirmAction::SaveRestart => {
            config.save(paths).await?;
            app.mark_saved();
            app.start_runtime_command(
                paths,
                config,
                RuntimeCommand::Restart,
                "Saved and restarted runtime",
                false,
            );
        }
        ConfirmAction::SaveRestartExit => {
            config.save(paths).await?;
            app.mark_saved();
            app.start_runtime_command(
                paths,
                config,
                RuntimeCommand::Restart,
                "Saved and restarted runtime",
                true,
            );
        }
        ConfirmAction::DeleteSubscription => {
            delete_selected_subscription(paths, config, app).await?;
        }
        ConfirmAction::DeleteProxyProfile => {
            delete_selected_proxy_profile(config, app).await?;
        }
        ConfirmAction::DeletePortProxy => {
            delete_selected_port_proxy(paths, config, app).await?;
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
        Page::Main => match main_proxy_kind(config, app.selected_main) {
            MainProxyKind::System => {
                open_selected_main_proxy_config(config, app);
            }
            MainProxyKind::AddPortProxy => {
                add_port_proxy(paths, config, app).await?;
            }
            MainProxyKind::Service => {
                open_selected_main_proxy_config(config, app);
            }
        },
        Page::Profile => submit_profile_selection(config, app).await?,
        Page::ProfileConfig => submit_profile_config_item(paths, config, app).await?,
        Page::Subscription => submit_subscription_selection(paths, config, app).await?,
        Page::SubscriptionDetail => submit_subscription_detail(paths, config, app).await?,
        Page::SubscriptionRuleGroups => submit_subscription_rule_group(paths, config, app).await?,
        Page::SubscriptionProxies => {
            test_selected_subscription_proxy(paths, config, app).await?;
        }
        Page::SubscriptionRules => {
            app.status = "Rules are read-only in this view".into();
        }
        Page::AddSubscription => submit_add_subscription_selection(paths, config, app).await?,
        Page::ProxyConfig => submit_proxy_config_item(paths, config, app).await?,
        Page::ProxyConnections => {
            app.status = "Connections are read-only snapshots from mihomo".into();
        }
        Page::ProxyLogs => {
            app.status = "Logs are read-only".into();
        }
        Page::ProxyGroups if app.rule_group_selection.is_some() => {
            select_proxy(paths, config, app).await?
        }
        Page::ProxyGroups => match app.proxy_pane {
            ProxyPane::Groups => {
                app.proxy_pane = ProxyPane::Proxies;
                app.selected_proxy =
                    current_proxy_index(app.current_group(config)).unwrap_or_default();
                app.status = "Select a proxy, then press Enter".into();
            }
            ProxyPane::Proxies => select_proxy(paths, config, app).await?,
        },
        Page::Mode => submit_mode_selection(paths, config, app).await?,
        Page::Dns => submit_dns_item(paths, config, app).await?,
        Page::Runtime => submit_runtime_item(paths, config, app).await?,
        Page::Chat => {
            send_chat_message(paths, config, app);
        }
        Page::Exit => submit_exit_item(paths, config, app).await?,
    }
    Ok(())
}

async fn submit_exit_item(
    paths: &Paths,
    config: &mut AppConfig,
    app: &mut ConfigApp,
) -> Result<()> {
    match app.selected_exit_item() {
        ExitItem::RuntimeSection | ExitItem::SaveSection | ExitItem::ExitSection => {
            app.status = app.selected_exit_item().help().into();
        }
        ExitItem::StartRuntime => app.start_runtime_command(
            paths,
            config,
            RuntimeCommand::Start,
            "Runtime started",
            false,
        ),
        ExitItem::StopRuntime => app.start_runtime_command(
            paths,
            config,
            RuntimeCommand::Stop,
            "Runtime stopped",
            false,
        ),
        ExitItem::ReloadRuntime => app.start_runtime_command(
            paths,
            config,
            RuntimeCommand::Reload,
            "Runtime reloaded",
            false,
        ),
        ExitItem::RestartRuntime => app.start_runtime_command(
            paths,
            config,
            RuntimeCommand::Restart,
            "Runtime restarted",
            false,
        ),
        ExitItem::SaveConfig => {
            config.save(paths).await?;
            app.mark_saved();
            app.status = format!("Saved config to {}", paths.config_file.display());
        }
        ExitItem::SaveRestart => app.open_confirm(ConfirmAction::SaveRestart),
        ExitItem::SaveRestartExit => app.open_confirm(ConfirmAction::SaveRestartExit),
        ExitItem::ExitWithoutSaving => app.open_confirm(ConfirmAction::ExitWithoutSaving),
        ExitItem::LoadDefaults => app.open_confirm(ConfirmAction::LoadDefaults),
        ExitItem::Exit => {
            app.should_quit = true;
        }
    }
    Ok(())
}

async fn submit_profile_selection(config: &mut AppConfig, app: &mut ConfigApp) -> Result<()> {
    match selected_proxy_profile_action(config, app) {
        ProxyProfileAction::Open(index) => {
            app.selected_profile = index;
            app.enter_page(
                Page::ProfileConfig,
                format!(
                    "Profile: {}",
                    display_name(&config.proxy_profiles[index].name)
                ),
            );
        }
        ProxyProfileAction::Add => add_proxy_profile(config, app),
    }
    Ok(())
}

async fn submit_profile_config_item(
    paths: &Paths,
    config: &mut AppConfig,
    app: &mut ConfigApp,
) -> Result<()> {
    match app.selected_profile_config_field() {
        ProfileConfigField::Name => begin_proxy_profile_name_input(config, app),
        ProfileConfigField::Active | ProfileConfigField::Activate => {
            activate_selected_proxy_profile(config, app);
        }
        ProfileConfigField::Subscription => {
            app.open_subscription_dropdown(config);
        }
        ProfileConfigField::Mode => {
            app.open_mode_dropdown(config);
        }
        ProfileConfigField::ProxyGroups => {
            app.rule_group_selection = None;
            if proxy_mode(config, app).eq_ignore_ascii_case("global") {
                app.proxy_pane = ProxyPane::Proxies;
                app.selected_proxy = 0;
                app.enter_page(Page::ProxyGroups, "Choose a proxy server");
            } else {
                app.proxy_pane = ProxyPane::Groups;
                app.enter_page(Page::ProxyGroups, "Choose a group, then choose a proxy");
            }
        }
        ProfileConfigField::Delete => {
            if is_default_proxy_profile(config, app.selected_profile) {
                app.alert("Default Profile", "The default profile cannot be deleted.");
            } else {
                app.open_confirm(ConfirmAction::DeleteProxyProfile);
            }
        }
    }
    let _ = paths;
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
        1 => begin_selected_subscription_url_input(config, app),
        2 => app.open_subscription_refresh_dropdown(config),
        3 => app.enter_page(Page::SubscriptionRuleGroups, "Subscription rule groups"),
        4 => app.enter_page(Page::SubscriptionProxies, "Subscription proxies"),
        5 => app.enter_page(Page::SubscriptionRules, "Subscription rules"),
        6 => update_selected_subscription(paths, config, app).await?,
        7 => app.open_confirm(ConfirmAction::DeleteSubscription),
        _ => {}
    }
    Ok(())
}

async fn submit_subscription_rule_group(
    paths: &Paths,
    config: &mut AppConfig,
    app: &mut ConfigApp,
) -> Result<()> {
    let Some(subscription) = selected_subscription(config, app) else {
        app.alert("No Subscription", "Select a subscription first.");
        return Ok(());
    };
    let groups = subscription_rule_groups_for_view(paths, config, app);
    let Some(group) = groups.get(app.selected_subscription_rule_group) else {
        app.status = "No rule group selected".into();
        return Ok(());
    };
    if group.all.is_empty() {
        app.alert(
            "No Proxies",
            "This rule group has no concrete proxy nodes in the local profile.",
        );
        return Ok(());
    }

    let selected_proxy = subscription
        .rule_selections
        .get(&group.name)
        .and_then(|proxy| group.all.iter().position(|candidate| candidate == proxy))
        .or_else(|| current_proxy_index(Some(group)))
        .unwrap_or_default();
    app.rule_group_selection = Some(RuleGroupSelection {
        subscription_index: app.selected_subscription,
        group_name: group.name.clone(),
    });
    app.proxy_pane = ProxyPane::Proxies;
    app.enter_page(
        Page::ProxyGroups,
        format!(
            "Choose selected outbound for group {}",
            display_name(&group.name)
        ),
    );
    app.selected_proxy = selected_proxy;
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
        SubscriptionFormField::Refresh => app.open_subscription_refresh_dropdown(config),
        SubscriptionFormField::Ok => submit_subscription_form(paths, config, app).await?,
    }
    Ok(())
}

async fn submit_dropdown(paths: &Paths, config: &mut AppConfig, app: &mut ConfigApp) -> Result<()> {
    match app.dropdown {
        Some(Dropdown::ProxyProfile) => {
            submit_proxy_profile_dropdown(config, app).await?;
        }
        Some(Dropdown::ProxySubscription) => {
            submit_subscription_dropdown(paths, config, app).await?;
        }
        Some(Dropdown::Mode) => {
            submit_mode_dropdown(paths, config, app).await?;
        }
        Some(Dropdown::SubscriptionRefresh) => {
            submit_subscription_refresh_dropdown(paths, config, app).await?;
        }
        Some(Dropdown::CoreSource) => {
            submit_core_source_dropdown(config, app);
        }
        Some(Dropdown::LlmProvider) => {
            submit_llm_provider_dropdown(config, app);
        }
        Some(Dropdown::LlmModel) => {
            submit_llm_model_dropdown(config, app);
        }
        None => {}
    }
    Ok(())
}

async fn submit_proxy_profile_dropdown(config: &mut AppConfig, app: &mut ConfigApp) -> Result<()> {
    app.dropdown = None;
    if config.proxy_profiles.is_empty() {
        app.alert("No Profile", "No proxy profiles are configured.");
        return Ok(());
    }

    let index = app
        .selected_dropdown
        .min(config.proxy_profiles.len().saturating_sub(1));
    let Some(name) = config
        .proxy_profiles
        .get(index)
        .map(|profile| profile.name.clone())
    else {
        app.alert("No Profile", "Select a profile first.");
        return Ok(());
    };
    config.activate_proxy_profile(&name);
    app.selected_profile = index;
    app.mark_dirty();
    app.status = format!("Activated profile {} pending save", display_name(&name));
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
    if app.page == Page::ProfileConfig {
        set_selected_profile_subscription(config, app, index).await
    } else {
        set_selected_proxy_subscription(paths, config, app, index).await
    }
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
    if app.page == Page::ProfileConfig {
        set_selected_profile_mode(config, app, mode.value()).await?;
    } else {
        set_selected_proxy_mode(paths, config, app, mode.value()).await?;
    }
    app.status = format!("Mode={} pending save", mode.label());
    Ok(())
}

async fn submit_subscription_refresh_dropdown(
    _paths: &Paths,
    config: &mut AppConfig,
    app: &mut ConfigApp,
) -> Result<()> {
    app.dropdown = None;
    let refresh = subscription_refresh_from_index(app.selected_dropdown);
    if app.page == Page::SubscriptionDetail {
        let Some(subscription) = config.subscriptions.get_mut(app.selected_subscription) else {
            app.status = "No subscription selected".into();
            return Ok(());
        };
        subscription.refresh = refresh;
        app.mark_dirty();
        app.status = format!(
            "Refresh={} pending save",
            subscription_refresh_label(refresh)
        );
    } else {
        app.subscription_form.refresh = refresh;
        app.status = "Refresh pending save".into();
    }
    Ok(())
}

fn submit_core_source_dropdown(config: &mut AppConfig, app: &mut ConfigApp) {
    app.dropdown = None;
    let index = app
        .selected_dropdown
        .min(core::CoreSource::ALL.len().saturating_sub(1));
    let source = core::CoreSource::ALL
        .get(index)
        .copied()
        .unwrap_or(core::CoreSource::Auto);
    config.mihomo.core = source.value().into();
    app.mark_dirty();
    if source == core::CoreSource::Custom {
        begin_core_path_input(config, app);
    } else {
        app.status = format!("Mihomo core={} pending save", source.label());
    }
}

fn submit_llm_provider_dropdown(config: &mut AppConfig, app: &mut ConfigApp) {
    app.dropdown = None;
    let presets = llm_providers::presets();
    if presets.is_empty() {
        app.status = "No LLM providers configured".into();
        return;
    }
    let index = app.selected_dropdown.min(presets.len().saturating_sub(1));
    let Some(preset) = presets.get(index) else {
        app.status = "No LLM providers configured".into();
        return;
    };
    config.llm.provider.clone_from(&preset.id);
    config.llm.api_key_env.clone_from(&preset.api_key_env);
    if !preset.base_url.is_empty() {
        config.llm.base_url.clone_from(&preset.base_url);
    }
    if !preset.default_model.is_empty() {
        config.llm.model.clone_from(&preset.default_model);
    }
    app.mark_dirty();
    app.status = format!("LLM provider={} pending save", preset.label);
}

fn submit_llm_model_dropdown(config: &mut AppConfig, app: &mut ConfigApp) {
    app.dropdown = None;
    let options = llm_model_options(config);
    if options.is_empty() || app.selected_dropdown >= options.len() {
        begin_llm_model_input(config, app);
        return;
    }
    let index = app.selected_dropdown.min(options.len().saturating_sub(1));
    let Some(model) = options.get(index) else {
        return;
    };
    config.llm.model.clone_from(model);
    app.mark_dirty();
    app.status = format!("LLM model={model} pending save");
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
    let profile_context = mode_selection_targets_profile(app);
    if profile_context {
        set_selected_profile_mode(config, app, mode.value()).await?;
    } else {
        set_selected_proxy_mode(paths, config, app, mode.value()).await?;
    }
    app.return_to_previous_or(
        if profile_context {
            Page::ProfileConfig
        } else {
            Page::ProxyConfig
        },
        format!("Mode={} pending save", mode.label()),
    );
    Ok(())
}

async fn submit_runtime_item(
    paths: &Paths,
    config: &mut AppConfig,
    app: &mut ConfigApp,
) -> Result<()> {
    match app.selected_runtime_item() {
        RuntimeItem::Service => {
            let status = service::status()?;
            app.status = format!(
                "Service installed={} reachable={} core_running={}",
                status.installed, status.reachable, status.core_running
            );
        }
        RuntimeItem::Autostart => toggle_autostart(paths, config, app).await?,
        RuntimeItem::Logs => app.status = "Logs view is not implemented in this preview".into(),
        RuntimeItem::CorePath => app.open_core_source_dropdown(config),
        RuntimeItem::CoreUpdate => app.start_runtime_command(
            paths,
            config,
            RuntimeCommand::UpdateCore,
            "Mihomo core update finished; restart to use",
            false,
        ),
        RuntimeItem::Controller => begin_controller_input(config, app),
        RuntimeItem::Refresh => refresh_runtime(paths, config, app),
        RuntimeItem::LlmSection => app.status = "LLM assistant settings".into(),
        RuntimeItem::LlmProvider => app.open_llm_provider_dropdown(config),
        RuntimeItem::LlmBaseUrl => begin_llm_base_url_input(config, app),
        RuntimeItem::LlmModel => app.open_llm_model_dropdown(config),
        RuntimeItem::LlmApiKey => begin_llm_api_key_input(app),
        RuntimeItem::LlmProvidersUpdate => update_llm_providers(paths, config, app)?,
        RuntimeItem::TestAssistant => app.start_assistant_test(paths, config),
    }
    Ok(())
}

async fn submit_proxy_config_item(
    paths: &Paths,
    config: &mut AppConfig,
    app: &mut ConfigApp,
) -> Result<()> {
    match app.selected_proxy_config_field() {
        ProxyConfigField::ServiceStatus | ProxyConfigField::TrafficStatus => {
            app.status = "Status is read-only".into();
        }
        ProxyConfigField::Connections => {
            app.selected_proxy_connection = 0;
            app.enter_page(Page::ProxyConnections, "Runtime connections");
        }
        ProxyConfigField::Enabled => {
            toggle_selected_main_proxy(paths, config, app).await?;
        }
        ProxyConfigField::Profile => {
            app.open_proxy_profile_dropdown(config);
        }
        ProxyConfigField::Subscription => {
            app.open_subscription_dropdown(config);
        }
        ProxyConfigField::Mode => {
            app.open_mode_dropdown(config);
        }
        ProxyConfigField::ProxyGroups => {
            app.rule_group_selection = None;
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
            MainProxyKind::Service => begin_service_port_input(config, app),
            MainProxyKind::AddPortProxy => add_port_proxy(paths, config, app).await?,
        },
        ProxyConfigField::Delete => {
            if main_proxy_kind(config, app.selected_main) == MainProxyKind::Service {
                app.open_confirm(ConfirmAction::DeletePortProxy);
            } else {
                app.alert("Invalid Proxy", "Select a port proxy first.");
            }
        }
        ProxyConfigField::OsProxy => toggle_system_proxy(paths, config, app).await?,
        ProxyConfigField::Pac => {
            app.status = "PAC configuration is not implemented in this preview".into()
        }
        ProxyConfigField::Tun => toggle_tun(paths, config, app).await?,
        ProxyConfigField::Logs => {
            app.selected_proxy_log = 0;
            app.enter_page(Page::ProxyLogs, "Runtime logs");
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
        DnsItem::NameserverPolicy => begin_dns_nameserver_policy_input(config, app),
        DnsItem::DirectNameserver => begin_dns_direct_nameserver_input(config, app),
        DnsItem::DirectFollowPolicy => {
            config.dns.direct_nameserver_follow_policy =
                !config.dns.direct_nameserver_follow_policy;
            save_dns_config(paths, config, app, "Direct DNS policy").await?;
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
        InputMode::ProxyProfileName => {
            rename_selected_proxy_profile(config, app, &value);
            app.finish_input();
        }
        InputMode::SubscriptionName => {
            app.subscription_form.name = value;
            app.finish_input();
            app.status = "Name pending save".into();
        }
        InputMode::SubscriptionUrl => {
            if app.page == Page::SubscriptionDetail {
                if value.is_empty() {
                    app.alert("Required Field", "URL is required.");
                    return Ok(());
                }
                let Some(subscription) = config.subscriptions.get_mut(app.selected_subscription)
                else {
                    app.finish_input();
                    app.alert("No Subscription", "Select a subscription first.");
                    return Ok(());
                };
                subscription.url = value;
                app.mark_dirty();
            } else {
                app.subscription_form.url = value;
            }
            app.finish_input();
            app.status = "URL pending save".into();
        }
        InputMode::CorePath => {
            config.core_path = if value.is_empty() { None } else { Some(value) };
            config.mihomo.core = core::CORE_SOURCE_CUSTOM.into();
            app.mark_dirty();
            app.finish_input();
            app.status = "Core path pending save".into();
        }
        InputMode::Controller => {
            if !value.is_empty() {
                config.controller.url = value;
                config.port_allocation.auto_controller = false;
                app.mark_dirty();
                app.status = "Controller pending save".into();
            }
            app.finish_input();
        }
        InputMode::LlmBaseUrl => {
            if !value.is_empty() {
                config.llm.base_url = value.trim_end_matches('/').to_string();
                app.mark_dirty();
                app.status = "LLM base URL pending save".into();
            }
            app.finish_input();
        }
        InputMode::LlmModel => {
            if !value.is_empty() {
                let added = llm_providers::save_model(
                    &paths.llm_providers_file,
                    &config.llm.provider,
                    &value,
                )?;
                llm_providers::reload_from_file(&paths.llm_providers_file)?;
                config.llm.model = value;
                app.mark_dirty();
                app.status = if added {
                    "LLM model added to providers and pending save".into()
                } else {
                    "LLM model pending save".into()
                };
            }
            app.finish_input();
        }
        InputMode::LlmApiKey => {
            if !value.is_empty() {
                agent::save_api_key(paths, config, &value).await?;
                app.status = "LLM API key saved to providers file".into();
            }
            app.finish_input();
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
            app.mark_dirty();
            app.status = format!(
                "MIX={}:{} pending save",
                config.proxy_host, config.mixed_port
            );
            app.finish_input();
        }
        InputMode::HttpPort => {
            let Some(port) = parse_optional_port_or_alert(app, &value, "HTTP port") else {
                return Ok(());
            };
            config.proxy_ports.http = port;
            app.mark_dirty();
            app.status = format!(
                "HTTP port={} pending save",
                optional_port_value(config.proxy_ports.http)
            );
            app.finish_input();
        }
        InputMode::SocksPort => {
            let Some(port) = parse_optional_port_or_alert(app, &value, "SOCKS port") else {
                return Ok(());
            };
            config.proxy_ports.socks = port;
            app.mark_dirty();
            app.status = format!(
                "SOCKS port={} pending save",
                optional_port_value(config.proxy_ports.socks)
            );
            app.finish_input();
        }
        InputMode::ServicePort => {
            let Some(service_index) = service_index_for_main_proxy(config, app.selected_main)
            else {
                app.finish_input();
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
                app.finish_input();
                app.alert("Invalid Proxy", "Select a port proxy first.");
                return Ok(());
            };
            service.listen = listen_host;
            service.port = port;
            let listen = service_listen(service);
            app.mark_dirty();
            app.status = format!("Port proxy={listen} pending save");
            app.finish_input();
        }
        InputMode::DnsListen => {
            if !value.is_empty() {
                config.dns.listen = value;
                config.port_allocation.auto_dns = false;
                save_dns_config(paths, config, app, "DNS listen").await?;
            }
            app.finish_input();
        }
        InputMode::DnsLanDomains => {
            config.dns.lan_domains = split_list(&value);
            save_dns_config(paths, config, app, "LAN domains").await?;
            app.finish_input();
        }
        InputMode::DnsLanNameserver => {
            config.dns.lan_nameserver = split_list(&value);
            save_dns_config(paths, config, app, "LAN DNS").await?;
            app.finish_input();
        }
        InputMode::DnsNameserverPolicy => {
            let Some(policy) = parse_nameserver_policy_or_alert(app, &value) else {
                return Ok(());
            };
            config.dns.nameserver_policy = policy;
            save_dns_config(paths, config, app, "DNS policy").await?;
            app.finish_input();
        }
        InputMode::DnsDirectNameserver => {
            config.dns.direct_nameserver = split_list(&value);
            save_dns_config(paths, config, app, "Direct DNS").await?;
            app.finish_input();
        }
        InputMode::DnsNameserver => {
            config.dns.nameserver = split_list(&value);
            save_dns_config(paths, config, app, "Default DNS").await?;
            app.finish_input();
        }
        InputMode::DnsFallback => {
            config.dns.fallback = split_list(&value);
            save_dns_config(paths, config, app, "Fallback DNS").await?;
            app.finish_input();
        }
        InputMode::DnsFakeIpFilter => {
            config.dns.fake_ip_filter = split_list(&value);
            save_dns_config(paths, config, app, "Fake-IP filter").await?;
            app.finish_input();
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

fn begin_proxy_profile_name_input(config: &AppConfig, app: &mut ConfigApp) {
    let value = selected_proxy_profile(config, app)
        .map(|profile| profile.name.clone())
        .unwrap_or_default();
    app.begin_input(InputMode::ProxyProfileName, value, "Edit profile name");
}

fn open_selected_main_proxy_config(config: &AppConfig, app: &mut ConfigApp) {
    if main_proxy_kind(config, app.selected_main) == MainProxyKind::AddPortProxy {
        app.status = "Create the port proxy before configuring it.".into();
        return;
    }
    app.selected_proxy_field = 0;
    app.enter_page(
        Page::ProxyConfig,
        format!("Configuring {}", main_proxy_name(config, app.selected_main)),
    );
}

fn begin_subscription_name_input(app: &mut ConfigApp) {
    app.begin_input(
        InputMode::SubscriptionName,
        app.subscription_form.name.clone(),
        "Edit subscription name",
    );
}

fn begin_subscription_url_input(app: &mut ConfigApp) {
    app.begin_input(
        InputMode::SubscriptionUrl,
        app.subscription_form.url.clone(),
        "Edit subscription URL",
    );
}

fn begin_selected_subscription_url_input(config: &AppConfig, app: &mut ConfigApp) {
    let Some(subscription) = selected_subscription(config, app) else {
        app.alert("No Subscription", "Select a subscription first.");
        return;
    };
    app.begin_input(
        InputMode::SubscriptionUrl,
        subscription.url.clone(),
        "Edit subscription URL",
    );
}

fn begin_core_path_input(config: &AppConfig, app: &mut ConfigApp) {
    app.begin_input(
        InputMode::CorePath,
        config.core_path.clone().unwrap_or_default(),
        "Edit mihomo core path",
    );
}

fn begin_controller_input(config: &AppConfig, app: &mut ConfigApp) {
    app.begin_input(
        InputMode::Controller,
        config.controller.url.clone(),
        "Edit controller URL",
    );
}

fn begin_llm_base_url_input(config: &AppConfig, app: &mut ConfigApp) {
    app.begin_input(
        InputMode::LlmBaseUrl,
        config.llm.base_url.clone(),
        "Edit OpenAI-compatible base URL",
    );
}

fn begin_llm_model_input(config: &AppConfig, app: &mut ConfigApp) {
    app.begin_input(
        InputMode::LlmModel,
        config.llm.model.clone(),
        "Edit LLM model",
    );
}

fn begin_llm_api_key_input(app: &mut ConfigApp) {
    app.begin_input(
        InputMode::LlmApiKey,
        String::new(),
        "Paste LLM API key; it will be saved in llm-providers.yaml",
    );
}

fn begin_mixed_port_input(config: &AppConfig, app: &mut ConfigApp) {
    app.begin_input(
        InputMode::MixedPort,
        format!("{}:{}", config.proxy_host, config.mixed_port),
        "Edit mixed listen address; use 0.0.0.0:7070 for LAN",
    );
}

fn begin_http_port_input(config: &AppConfig, app: &mut ConfigApp) {
    app.begin_input(
        InputMode::HttpPort,
        optional_port_value(config.proxy_ports.http),
        "Edit HTTP port; empty/off disables it",
    );
}

fn begin_socks_port_input(config: &AppConfig, app: &mut ConfigApp) {
    app.begin_input(
        InputMode::SocksPort,
        optional_port_value(config.proxy_ports.socks),
        "Edit SOCKS port; empty/off disables it",
    );
}

fn begin_service_port_input(config: &AppConfig, app: &mut ConfigApp) {
    let value = service_index_for_main_proxy(config, app.selected_main)
        .and_then(|index| config.proxy_ports.services.get(index))
        .map(service_listen)
        .unwrap_or_else(|| "auto".into());
    app.begin_input(
        InputMode::ServicePort,
        value,
        "Edit port proxy listen address; empty/auto uses allocator",
    );
}

fn begin_dns_listen_input(config: &AppConfig, app: &mut ConfigApp) {
    app.page = Page::Dns;
    app.begin_input(
        InputMode::DnsListen,
        config.dns.listen.clone(),
        "Edit DNS listen address",
    );
}

fn begin_dns_lan_domains_input(config: &AppConfig, app: &mut ConfigApp) {
    app.page = Page::Dns;
    app.begin_input(
        InputMode::DnsLanDomains,
        join_multiline_list(&config.dns.lan_domains),
        "Edit LAN domain suffixes",
    );
}

fn begin_dns_lan_nameserver_input(config: &AppConfig, app: &mut ConfigApp) {
    app.page = Page::Dns;
    app.begin_input(
        InputMode::DnsLanNameserver,
        join_multiline_list(&config.dns.lan_nameserver),
        "Edit LAN DNS servers",
    );
}

fn begin_dns_nameserver_policy_input(config: &AppConfig, app: &mut ConfigApp) {
    app.page = Page::Dns;
    app.begin_input(
        InputMode::DnsNameserverPolicy,
        format_multiline_nameserver_policy(&config.dns.nameserver_policy),
        "Edit DNS policy entries",
    );
}

fn begin_dns_direct_nameserver_input(config: &AppConfig, app: &mut ConfigApp) {
    app.page = Page::Dns;
    app.begin_input(
        InputMode::DnsDirectNameserver,
        join_multiline_list(&config.dns.direct_nameserver),
        "Edit DIRECT DNS servers",
    );
}

fn begin_dns_nameserver_input(config: &AppConfig, app: &mut ConfigApp) {
    app.page = Page::Dns;
    app.begin_input(
        InputMode::DnsNameserver,
        join_multiline_list(&config.dns.nameserver),
        "Edit default DNS servers",
    );
}

fn begin_dns_fallback_input(config: &AppConfig, app: &mut ConfigApp) {
    app.page = Page::Dns;
    app.begin_input(
        InputMode::DnsFallback,
        join_multiline_list(&config.dns.fallback),
        "Edit fallback DNS servers",
    );
}

fn begin_dns_fake_ip_filter_input(config: &AppConfig, app: &mut ConfigApp) {
    app.page = Page::Dns;
    app.begin_input(
        InputMode::DnsFakeIpFilter,
        join_multiline_list(&config.dns.fake_ip_filter),
        "Edit fake-IP filter",
    );
}

fn refresh_runtime(paths: &Paths, config: &AppConfig, app: &mut ConfigApp) {
    app.refresh_runtime(paths, config);
    app.restart_subscription_profile_refresh(paths, config);
    app.status = "Refresh scheduled".into();
}

fn update_llm_providers(paths: &Paths, config: &mut AppConfig, app: &mut ConfigApp) -> Result<()> {
    let report = llm_providers::update_local_from_bundled(&paths.llm_providers_file)?;
    if let Some(provider) = llm_providers::provider(&config.llm.provider) {
        let base_url_changed =
            !provider.base_url.is_empty() && config.llm.base_url != provider.base_url;
        let api_key_env_changed =
            !provider.api_key_env.is_empty() && config.llm.api_key_env != provider.api_key_env;
        let model_changed =
            config.llm.model.trim().is_empty() && !provider.default_model.is_empty();

        if base_url_changed {
            config.llm.base_url = provider.base_url;
        }
        if api_key_env_changed {
            config.llm.api_key_env = provider.api_key_env;
        }
        if model_changed {
            config.llm.model = provider.default_model;
        }
        if base_url_changed || api_key_env_changed || model_changed {
            app.mark_dirty();
        }
    }
    app.status = format!(
        "LLM providers updated: +{} providers, {} custom providers, {} custom models kept",
        report.added_providers, report.preserved_custom_providers, report.preserved_custom_models
    );
    Ok(())
}

async fn toggle_system_proxy(
    _paths: &Paths,
    config: &mut AppConfig,
    app: &mut ConfigApp,
) -> Result<()> {
    config.system_proxy.enabled = !config.system_proxy.enabled;
    app.mark_dirty();
    app.status = format!(
        "System proxy={} pending save",
        on_off(config.system_proxy.enabled)
    );
    Ok(())
}

async fn toggle_autostart(
    _paths: &Paths,
    config: &mut AppConfig,
    app: &mut ConfigApp,
) -> Result<()> {
    config.autostart.enabled = !config.autostart.enabled;
    app.mark_dirty();
    app.status = format!(
        "Autostart={} pending save",
        on_off(config.autostart.enabled)
    );
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
        MainProxyKind::System => {
            app.status =
                "Main mihomo proxy is managed by runtime; toggle Sys Proxy separately".into();
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
            app.mark_dirty();
            app.status = format!("{name}={} pending save", on_off(enabled));
        }
        MainProxyKind::AddPortProxy => {
            app.status = "Press Enter to add a port proxy".into();
        }
    }

    app.clamp_selection(paths, config);
    Ok(())
}

async fn add_port_proxy(_paths: &Paths, config: &mut AppConfig, app: &mut ConfigApp) -> Result<()> {
    let service_index = config.proxy_ports.services.len();
    let service = PortProxyService {
        name: format!("Port Proxy {}", service_index + 1),
        port: next_port_proxy_port(config),
        subscription: config.active_profile.clone(),
        mode: "global".into(),
        ..PortProxyService::default()
    };
    config.proxy_ports.services.push(service);
    app.mark_dirty();

    app.selected_main = main_proxy_index_for_service(config, service_index).unwrap_or(0);
    app.selected_proxy_field = 0;
    app.enter_page(
        Page::ProxyConfig,
        format!("Added {}", main_proxy_name(config, app.selected_main)),
    );
    Ok(())
}

async fn delete_selected_port_proxy(
    _paths: &Paths,
    config: &mut AppConfig,
    app: &mut ConfigApp,
) -> Result<()> {
    let Some(service_index) = service_index_for_main_proxy(config, app.selected_main) else {
        app.alert("Invalid Proxy", "Select a port proxy first.");
        return Ok(());
    };
    if service_index >= config.proxy_ports.services.len() {
        app.alert("Invalid Proxy", "Select a port proxy first.");
        return Ok(());
    }

    let removed = config.proxy_ports.services.remove(service_index);
    let removed_name = service_name(&removed);
    app.mark_dirty();
    let status = format!("Deleted {removed_name}; pending save");

    let service_count = config.proxy_ports.services.len();
    app.selected_main = if service_count == 0 {
        0
    } else {
        let max_service_main = service_count;
        app.selected_main.min(max_service_main)
    };
    app.selected_proxy_field = 0;
    app.proxy_pane = ProxyPane::Groups;
    app.selected_group = 0;
    app.selected_proxy = 0;
    app.rule_group_selection = None;
    app.proxy_runtimes.retain(|key, _| key == "global");
    app.history.clear();
    app.replace_page_with_status(Page::Main, status);
    Ok(())
}

async fn toggle_tun(_paths: &Paths, config: &mut AppConfig, app: &mut ConfigApp) -> Result<()> {
    config.tun.enable = !config.tun.enable;
    app.mark_dirty();
    app.status = format!("TUN={} pending save", on_off(config.tun.enable));
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
    _paths: &Paths,
    _config: &AppConfig,
    app: &mut ConfigApp,
    status: &str,
) -> Result<()> {
    app.mark_dirty();
    app.status = format!("{status}; pending save");
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
    app.selected_subscription = config.subscriptions.len() - 1;
    app.finish_input();
    app.subscription_form.clear();

    match subscription::update_preserving_last_good(paths, config, app.selected_subscription).await
    {
        Ok(path) => {
            app.status = format!(
                "Added {}; downloaded {}; pending save",
                display_name(&name),
                path.display()
            )
        }
        Err(err) => {
            app.status = format!(
                "Added {}; download failed: {err}; pending save",
                display_name(&name)
            )
        }
    }
    app.mark_dirty();
    app.restart_subscription_profile_refresh(paths, config);
    if app.page == Page::AddSubscription {
        let status = app.status.clone();
        app.return_to_previous_or(Page::Subscription, status);
    }
    Ok(())
}

async fn activate_subscription(
    _paths: &Paths,
    config: &mut AppConfig,
    app: &mut ConfigApp,
    index: usize,
) -> Result<()> {
    let Some(sub) = config.subscriptions.get(index) else {
        app.status = "No subscription selected".into();
        return Ok(());
    };
    config.active_profile = Some(sub.name.clone());
    app.mark_dirty();
    app.status = format!(
        "Active subscription={} pending save",
        display_name(&sub.name)
    );
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
            app.mark_dirty();
            app.status = format!(
                "Updated {}; profile={}; pending save",
                display_name(&name),
                path.display()
            );
        }
        Err(err) => {
            app.mark_dirty();
            app.status = format!(
                "Update failed for {}; last good kept: {err}; pending save",
                display_name(&name)
            );
        }
    }
    app.restart_subscription_profile_refresh(paths, config);
    Ok(())
}

async fn delete_selected_subscription(
    _paths: &Paths,
    config: &mut AppConfig,
    app: &mut ConfigApp,
) -> Result<()> {
    let index = app.selected_subscription;
    if index >= config.subscriptions.len() {
        app.alert("No Subscription", "Select a subscription first.");
        return Ok(());
    }

    let removed = config.subscriptions.remove(index);
    let status = format!("Deleted {}; pending save", display_name(&removed.name));

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
    app.mark_dirty();
    app.restart_subscription_profile_refresh(_paths, config);

    app.return_to_previous_or(Page::Subscription, status);
    Ok(())
}

async fn test_selected_subscription_proxy(
    paths: &Paths,
    config: &mut AppConfig,
    app: &mut ConfigApp,
) -> Result<()> {
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
    test_subscription_proxy_delay(paths, config, app, subscription, proxy.clone()).await
}

async fn test_subscription_proxy_delay(
    paths: &Paths,
    config: &mut AppConfig,
    app: &mut ConfigApp,
    subscription: Subscription,
    proxy: String,
) -> Result<()> {
    let Some((client, provider_name)) = subscription_delay_client(
        paths,
        config,
        app,
        &subscription,
        std::slice::from_ref(&proxy),
    )
    .await
    else {
        return Ok(());
    };
    match client
        .provider_proxy_delay(
            &provider_name,
            &proxy,
            DEFAULT_DELAY_TEST_URL,
            DEFAULT_DELAY_TEST_TIMEOUT_MS,
        )
        .await
    {
        Ok(delay) => {
            app.proxy_delays.insert(
                subscription_proxy_delay_key(&subscription, &proxy),
                format!("{delay}ms"),
            );
            app.status = format!("{} {}ms", display_name(&proxy), delay);
        }
        Err(err) => {
            app.proxy_delays.insert(
                subscription_proxy_delay_key(&subscription, &proxy),
                "fail".into(),
            );
            app.status = format!("{} check failed: {err}", display_name(&proxy));
        }
    }
    Ok(())
}

async fn quick_delay_check(
    paths: &Paths,
    config: &mut AppConfig,
    app: &mut ConfigApp,
) -> Result<bool> {
    let target = match app.page {
        Page::SubscriptionProxies => {
            test_selected_subscription_proxy(paths, config, app).await?;
            return Ok(true);
        }
        Page::Profile | Page::ProfileConfig => selected_profile_delay_target(config, app),
        Page::ProxyConfig => selected_proxy_config_delay_target(config, app),
        Page::ProxyGroups => selected_proxy_group_delay_target(paths, config, app),
        _ => return Ok(false),
    };

    let Some((subscription, proxy)) = target else {
        app.alert(
            "No Proxy",
            "Select a subscription-backed proxy before running a delay check.",
        );
        return Ok(true);
    };
    test_subscription_proxy_delay(paths, config, app, subscription, proxy).await?;
    Ok(true)
}

async fn test_all_subscription_proxies(
    paths: &Paths,
    config: &mut AppConfig,
    app: &mut ConfigApp,
) -> Result<()> {
    let Some(subscription) = selected_subscription(config, app).cloned() else {
        app.alert("No Subscription", "Select a subscription first.");
        return Ok(());
    };
    let proxies = subscription_proxy_names_for_view(paths, config, app);
    if proxies.is_empty() {
        app.status = "No proxies to check".into();
        return Ok(());
    }
    let Some((client, provider_name)) =
        subscription_delay_client(paths, config, app, &subscription, &proxies).await
    else {
        return Ok(());
    };
    app.start_delay_check(subscription.name, provider_name, client, proxies);
    Ok(())
}

async fn run_delay_check_task(
    sender: mpsc::Sender<DelayCheckEvent>,
    subscription_name: String,
    provider_name: String,
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
            .provider_proxy_delay(
                &provider_name,
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

async fn subscription_delay_client(
    paths: &Paths,
    config: &mut AppConfig,
    app: &mut ConfigApp,
    subscription: &Subscription,
    proxies: &[String],
) -> Option<(MihomoClient, String)> {
    let client = runtime_mihomo_client(paths, config, app).await?;
    let provider_name = runtime_profile::subscription_check_provider_name(subscription);
    match subscription_check_provider_has_proxies(&client, &provider_name, proxies).await {
        Ok(true) => return Some((client, provider_name)),
        Ok(false) => {}
        Err(err) => {
            app.status = format!("Delay check provider lookup failed: {err}");
        }
    }

    if let Err(err) = reload_runtime_from_config(paths, config).await {
        app.status = format!("Delay check runtime reload failed: {err:#}");
        app.alert(
            "Delay Check Unavailable",
            "Mihomo could not reload the hidden subscription check provider.",
        );
        return None;
    }

    match subscription_check_provider_has_proxies(&client, &provider_name, proxies).await {
        Ok(true) => Some((client, provider_name)),
        Ok(false) => {
            app.status = format!(
                "{} check provider is not loaded",
                display_name(&subscription.name)
            );
            app.alert(
                "Delay Check Unavailable",
                "The selected subscription has no loaded hidden check provider.",
            );
            None
        }
        Err(err) => {
            app.status = format!("Delay check provider lookup failed: {err}");
            app.alert(
                "Delay Check Unavailable",
                "Mihomo did not report the hidden subscription check provider.",
            );
            None
        }
    }
}

async fn subscription_check_provider_has_proxies(
    client: &MihomoClient,
    provider_name: &str,
    proxies: &[String],
) -> Result<bool> {
    let Some(loaded) = client.proxy_provider_proxy_names(provider_name).await? else {
        return Ok(false);
    };
    let loaded = loaded.into_iter().collect::<BTreeSet<_>>();
    Ok(proxies.iter().all(|proxy| loaded.contains(proxy)))
}

async fn wait_for_runtime_after_restart(config: &AppConfig) -> bool {
    let client = MihomoClient::new(&config.controller);
    for _ in 0..RUNTIME_START_RETRIES {
        if client.version().await.is_ok() {
            return true;
        }
        sleep(RUNTIME_START_WAIT).await;
    }
    false
}

fn run_runtime_cli_command(command: RuntimeCommand) -> Result<std::process::Output> {
    let arg = match command {
        RuntimeCommand::Start => "start",
        RuntimeCommand::Stop => "stop",
        RuntimeCommand::Restart => "restart",
        RuntimeCommand::Reload => anyhow::bail!("reload is handled through mihomo controller"),
        RuntimeCommand::UpdateCore => anyhow::bail!("core update is handled in the config UI"),
    };
    let exe = std::env::current_exe().context("failed to locate current executable")?;
    Command::new(exe)
        .arg(arg)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .with_context(|| format!("failed to execute clashtui {arg}"))
}

async fn run_runtime_command_task(
    paths: Paths,
    config: AppConfig,
    command: RuntimeCommand,
) -> RuntimeCommandResult {
    if matches!(command, RuntimeCommand::Reload) {
        return match reload_runtime_from_saved_config(&paths).await {
            Ok(()) => RuntimeCommandResult {
                success: true,
                message: None,
            },
            Err(err) => RuntimeCommandResult {
                success: false,
                message: Some(format!("{err:#}")),
            },
        };
    }

    if matches!(command, RuntimeCommand::UpdateCore) {
        return match core::update_managed_core(&paths, &config).await {
            Ok(install) => RuntimeCommandResult {
                success: true,
                message: Some(if install.updated {
                    format!(
                        "{} updated to {}; restart to use {}",
                        install.source.label(),
                        install.version,
                        install.path.display()
                    )
                } else {
                    format!(
                        "{} already current at {}; path={}",
                        install.source.label(),
                        install.version,
                        install.path.display()
                    )
                }),
            },
            Err(err) => RuntimeCommandResult {
                success: false,
                message: Some(format!("{err:#}")),
            },
        };
    }

    match tokio::task::spawn_blocking(move || run_runtime_cli_command(command)).await {
        Ok(Ok(output)) if output.status.success() => RuntimeCommandResult {
            success: true,
            message: None,
        },
        Ok(Ok(output)) => {
            if matches!(command, RuntimeCommand::Start | RuntimeCommand::Restart)
                && wait_for_runtime_after_restart(&config).await
            {
                RuntimeCommandResult {
                    success: true,
                    message: None,
                }
            } else {
                RuntimeCommandResult {
                    success: false,
                    message: Some(restart_output_message(&output)),
                }
            }
        }
        Ok(Err(err)) => RuntimeCommandResult {
            success: false,
            message: Some(format!("{err:#}")),
        },
        Err(err) => RuntimeCommandResult {
            success: false,
            message: Some(format!("runtime task failed: {err}")),
        },
    }
}

async fn reload_runtime_from_saved_config(paths: &Paths) -> Result<()> {
    let config = AppConfig::load_or_init(paths).await?;
    reload_runtime_from_config(paths, &config).await
}

async fn reload_runtime_from_config(paths: &Paths, config: &AppConfig) -> Result<()> {
    let runtime_config = if config.use_single_runtime() {
        runtime_profile::write_single_runtime_config(paths, config).await?
    } else {
        runtime_profile::write_current_config(paths, config).await?
    };
    let client = MihomoClient::new(&config.controller);
    client.reload_config(&runtime_config).await?;
    client.set_mode(&config.runtime_mode).await?;
    if !wait_for_runtime_after_restart(config).await {
        anyhow::bail!("mihomo runtime did not become ready after reload");
    }
    Ok(())
}

fn restart_output_message(output: &std::process::Output) -> String {
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let mut message = format!("status: {}", output.status);
    if !stdout.is_empty() {
        message.push_str("\n\nstdout:\n");
        message.push_str(&stdout);
    }
    if !stderr.is_empty() {
        message.push_str("\n\nstderr:\n");
        message.push_str(&stderr);
    }
    if message.chars().count() > 4000 {
        let tail = message
            .chars()
            .rev()
            .take(4000)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<String>();
        message = format!("...{tail}");
    }
    message
}

fn first_output_line(message: &str) -> Option<&str> {
    message.lines().map(str::trim).find(|line| {
        !line.is_empty() && !line.starts_with("status:") && *line != "stdout:" && *line != "stderr:"
    })
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
    config: &mut AppConfig,
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
    _paths: &Paths,
    config: &mut AppConfig,
    app: &mut ConfigApp,
    mode: &str,
) -> Result<()> {
    config.runtime_mode = mode.to_string();
    app.mark_dirty();
    app.status = format!("Mode={mode} pending save");
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
            app.mark_dirty();
            app.status = format!("Mode={mode} pending save");
            Ok(())
        }
        MainProxyKind::AddPortProxy => Ok(()),
    }
}

async fn set_selected_profile_mode(
    config: &mut AppConfig,
    app: &mut ConfigApp,
    mode: &str,
) -> Result<()> {
    let Some(profile) = selected_proxy_profile_mut(config, app) else {
        app.alert("No Profile", "Select a profile first.");
        return Ok(());
    };
    profile.mode = mode.to_string();
    sync_active_profile_if_selected(config, app.selected_profile);
    app.mark_dirty();
    app.status = format!("Profile mode={mode} pending save");
    Ok(())
}

async fn set_selected_proxy_subscription(
    paths: &Paths,
    config: &mut AppConfig,
    app: &mut ConfigApp,
    index: usize,
) -> Result<()> {
    match main_proxy_kind(config, app.selected_main) {
        MainProxyKind::System => activate_subscription(paths, config, app, index).await,
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
            app.mark_dirty();
            app.status = format!("Subscription={} pending save", display_name(&name));
            Ok(())
        }
        MainProxyKind::AddPortProxy => Ok(()),
    }
}

async fn set_selected_profile_subscription(
    config: &mut AppConfig,
    app: &mut ConfigApp,
    index: usize,
) -> Result<()> {
    let name = config.subscriptions[index].name.clone();
    let Some(profile) = selected_proxy_profile_mut(config, app) else {
        app.alert("No Profile", "Select a profile first.");
        return Ok(());
    };
    profile.subscription = Some(name.clone());
    sync_active_profile_if_selected(config, app.selected_profile);
    app.mark_dirty();
    app.status = format!("Profile subscription={} pending save", display_name(&name));
    Ok(())
}

fn add_proxy_profile(config: &mut AppConfig, app: &mut ConfigApp) {
    let name = next_proxy_profile_name(config);
    let mut profile = config
        .active_proxy_profile()
        .cloned()
        .unwrap_or_else(|| ProxyProfile::from_global_config("default", config));
    profile.name = name.clone();
    config.proxy_profiles.push(profile);
    app.selected_profile = config.proxy_profiles.len().saturating_sub(1);
    app.selected_profile_field = 0;
    app.mark_dirty();
    app.enter_page(
        Page::ProfileConfig,
        format!("Profile: {}", display_name(&name)),
    );
}

fn activate_selected_proxy_profile(config: &mut AppConfig, app: &mut ConfigApp) {
    let Some(name) = config
        .proxy_profiles
        .get(app.selected_profile)
        .map(|profile| profile.name.clone())
    else {
        app.alert("No Profile", "Select a profile first.");
        return;
    };
    config.activate_proxy_profile(&name);
    app.mark_dirty();
    app.status = format!("Activated profile {}", display_name(&name));
}

async fn delete_selected_proxy_profile(config: &mut AppConfig, app: &mut ConfigApp) -> Result<()> {
    if is_default_proxy_profile(config, app.selected_profile) || config.proxy_profiles.len() <= 1 {
        app.alert("Default Profile", "The default profile cannot be deleted.");
        return Ok(());
    }
    if app.selected_profile >= config.proxy_profiles.len() {
        app.alert("No Profile", "Select a profile first.");
        return Ok(());
    }

    let removed = config.proxy_profiles.remove(app.selected_profile);
    if config.active_proxy_profile == removed.name {
        let fallback = config
            .proxy_profiles
            .first()
            .map(|profile| profile.name.clone())
            .unwrap_or_else(|| "default".into());
        config.activate_proxy_profile(&fallback);
    }
    app.selected_profile = app
        .selected_profile
        .min(config.proxy_profiles.len().saturating_sub(1));
    app.selected_profile_field = 0;
    app.mark_dirty();
    app.return_to_previous_or(
        Page::Profile,
        format!(
            "Deleted profile {}; pending save",
            display_name(&removed.name)
        ),
    );
    Ok(())
}

fn rename_selected_proxy_profile(config: &mut AppConfig, app: &mut ConfigApp, value: &str) {
    let name = value.trim();
    if name.is_empty() {
        app.alert("Required Field", "Profile name is required.");
        return;
    }
    if config
        .proxy_profiles
        .iter()
        .enumerate()
        .any(|(index, profile)| index != app.selected_profile && profile.name == name)
    {
        app.alert("Duplicate Name", format!("{name} already exists."));
        return;
    }
    let Some(profile) = config.proxy_profiles.get_mut(app.selected_profile) else {
        app.alert("No Profile", "Select a profile first.");
        return;
    };
    let old_name = profile.name.clone();
    profile.name = name.to_string();
    if config.active_proxy_profile == old_name {
        config.active_proxy_profile = name.to_string();
    }
    app.mark_dirty();
    app.status = format!("Profile={} pending save", display_name(name));
}

fn sync_active_profile_if_selected(config: &mut AppConfig, index: usize) {
    let Some(profile) = config.proxy_profiles.get(index) else {
        return;
    };
    if profile.name == config.active_proxy_profile {
        config.apply_active_proxy_profile();
    }
}

fn next_proxy_profile_name(config: &AppConfig) -> String {
    for index in 2.. {
        let name = format!("profile {index}");
        if !config
            .proxy_profiles
            .iter()
            .any(|profile| profile.name == name)
        {
            return name;
        }
    }
    "profile".into()
}

async fn select_proxy(paths: &Paths, config: &mut AppConfig, app: &mut ConfigApp) -> Result<()> {
    if app.rule_group_selection.is_some() {
        return select_subscription_rule_proxy(paths, config, app).await;
    }

    let mode = proxy_mode(config, app).to_string();
    let (group_name, proxy_name) = if mode.eq_ignore_ascii_case("global") {
        let proxies = route_proxy_names(paths, config, app);
        let Some(proxy) = proxies.get(app.selected_proxy) else {
            app.status = "No proxy selected".into();
            return Ok(());
        };
        ("GLOBAL".to_string(), proxy.clone())
    } else {
        let Some(group) = app.current_group(config) else {
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
    app.mark_dirty();
    app.status = format!(
        "{} -> {} pending save ({selection_scope})",
        display_name(&group_name),
        display_name(&proxy_name)
    );
    Ok(())
}

async fn select_subscription_rule_proxy(
    paths: &Paths,
    config: &mut AppConfig,
    app: &mut ConfigApp,
) -> Result<()> {
    let Some(context) = app.rule_group_selection.clone() else {
        app.status = "No rule group selection active".into();
        return Ok(());
    };
    let Some(subscription) = config.subscriptions.get(context.subscription_index) else {
        app.rule_group_selection = None;
        app.alert(
            "No Subscription",
            "The selected subscription no longer exists.",
        );
        return Ok(());
    };
    let groups = subscription_rule_groups_for_view(paths, config, app);
    let Some(group) = groups
        .iter()
        .find(|group| group.name == context.group_name)
        .or_else(|| groups.get(app.selected_subscription_rule_group))
    else {
        app.status = "No rule group selected".into();
        return Ok(());
    };
    let Some(proxy) = group.all.get(app.selected_proxy) else {
        app.status = "No proxy selected".into();
        return Ok(());
    };

    let subscription_name = subscription.name.clone();
    let group_name = group.name.clone();
    let proxy_name = proxy.clone();
    if let Some(subscription) = config.subscriptions.get_mut(context.subscription_index) {
        subscription
            .rule_selections
            .insert(group_name.clone(), proxy_name.clone());
    }
    app.mark_dirty();
    app.rule_group_selection = None;
    let status = format!(
        "{} -> {} pending save for {}",
        display_name(&group_name),
        display_name(&proxy_name),
        display_name(&subscription_name)
    );

    app.return_to_previous_or(Page::SubscriptionRuleGroups, status);
    Ok(())
}

fn draw(frame: &mut Frame, paths: &Paths, config: &AppConfig, app: &ConfigApp) {
    let area = frame.area();
    let separator_column = split_column(area);
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
    draw_footer(frame, app, layout[2], separator_column);

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
    if app.assistant_test.is_some() {
        draw_assistant_test(frame, app);
    }
    if app.runtime_command.is_some() {
        draw_runtime_command(frame, app);
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
                format!("[ {} ]", page.title_for(app.language))
            } else {
                format!(" {} ", page.title_for(app.language))
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
    if app.page == Page::Chat {
        draw_chat_page(frame, paths, config, app, area);
        return;
    }
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(66),
            Constraint::Length(1),
            Constraint::Percentage(34),
        ])
        .split(area);
    let selected_row = draw_settings(frame, paths, config, app, columns[0]);
    draw_vertical_separator(frame, columns[1]);
    draw_help(frame, config, app, selected_row.as_ref(), columns[2]);
}

fn draw_settings(
    frame: &mut Frame,
    paths: &Paths,
    config: &AppConfig,
    app: &ConfigApp,
    area: Rect,
) -> Option<SettingRow> {
    if app.page == Page::Main {
        draw_main_settings(frame, paths, config, app, area);
        return None;
    }
    if app.page == Page::Subscription {
        draw_subscription_settings(frame, config, app, area);
        return None;
    }
    if app.page == Page::SubscriptionRules {
        draw_compact_rows(
            frame,
            page_summary(app.page),
            subscription_rule_compact_rows(paths, config, app),
            app.selected_subscription_rule,
            area,
            Style::default().fg(Color::White),
        );
        return None;
    }
    if app.page == Page::ProxyConnections {
        draw_compact_rows(
            frame,
            page_summary(app.page),
            proxy_connection_rows(config, app),
            app.selected_proxy_connection,
            area,
            Style::default().fg(Color::White),
        );
        return None;
    }
    if app.page == Page::ProxyLogs {
        draw_compact_rows(
            frame,
            page_summary(app.page),
            proxy_log_rows(paths, config, app),
            app.selected_proxy_log,
            area,
            Style::default().fg(Color::Yellow),
        );
        return None;
    }

    let rows = setting_rows(paths, config, app);
    let selected = app.selected_index();
    let selected_row = rows.get(selected).cloned();
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
        lines.push(with_padding(setting_row_line(
            row,
            content_width,
            index == selected,
        )));
    }

    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
    selected_row
}

fn draw_compact_rows(
    frame: &mut Frame,
    summary: &str,
    rows: Vec<CompactRow>,
    selected: usize,
    area: Rect,
    base_style: Style,
) {
    let content_width = area.width.saturating_sub(2) as usize;
    let mut lines = vec![
        with_padding(Line::from(Span::styled(
            summary.to_string(),
            Style::default().fg(Color::Gray),
        ))),
        Line::from(Span::styled(
            horizontal_line(area.width),
            Style::default().fg(Color::DarkGray),
        )),
    ];

    let rows = if rows.is_empty() {
        vec![CompactRow {
            primary: "-  no data".into(),
            secondary: None,
        }]
    } else {
        rows
    };
    let visible = area.height.saturating_sub(2) as usize;
    let rows_per_item = 2;
    let visible_items = (visible / rows_per_item).max(1);
    let selected = selected.min(rows.len().saturating_sub(1));
    let start = visible_window_start(selected, rows.len(), visible_items);

    for (index, row) in rows.iter().enumerate().skip(start).take(visible_items) {
        let selected_row = index == selected;
        let style = if selected_row {
            selected_style()
        } else {
            base_style
        };
        lines.push(with_padding(Line::from(Span::styled(
            fit_width(&row.primary, content_width),
            style,
        ))));
        let secondary = row.secondary.as_deref().unwrap_or("");
        let secondary_style = if selected_row {
            selected_style()
        } else {
            Style::default().fg(base_style.fg.unwrap_or(Color::White))
        };
        lines.push(with_padding(Line::from(Span::styled(
            fit_width(secondary, content_width),
            secondary_style,
        ))));
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

    if let Some(notice) = runtime_offline_notice(app) {
        lines.push(with_padding(Line::from(Span::styled(
            notice,
            Style::default().fg(Color::Yellow),
        ))));
    }

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
            Style::default().fg(Color::White)
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

        let third = fit_width(
            &format!("  {}", proxy_runtime_line(config, app, index)),
            content_width,
        );
        lines.push(with_padding(Line::from(Span::styled(
            third,
            Style::default().fg(Color::DarkGray),
        ))));
    }

    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

fn draw_subscription_settings(frame: &mut Frame, config: &AppConfig, app: &ConfigApp, area: Rect) {
    let selected = app.selected_subscription;
    let content_width = area.width.saturating_sub(2) as usize;
    let mut lines = vec![
        with_padding(Line::from(Span::styled(
            page_summary(Page::Subscription),
            Style::default().fg(Color::Gray),
        ))),
        Line::from(Span::styled(
            horizontal_line(area.width),
            Style::default().fg(Color::DarkGray),
        )),
    ];

    let visible = area.height.saturating_sub(2) as usize;
    let rows_per_item = 2;
    let visible_items = (visible / rows_per_item).max(1);
    let start = visible_window_start(selected, subscription_menu_count(config), visible_items);

    for index in start..subscription_menu_count(config).min(start + visible_items) {
        let selected_row = index == selected;
        if let Some(subscription) = config.subscriptions.get(index) {
            let first = fit_width(
                &format!(
                    "> {}  updated={}  {}",
                    display_name(&subscription.name),
                    format_optional_timestamp(subscription.updated_at.as_deref(), "-"),
                    subscription_state_label(subscription)
                ),
                content_width,
            );
            let first_style = if selected_row {
                selected_style()
            } else {
                Style::default().fg(Color::White)
            };
            lines.push(with_padding(Line::from(Span::styled(first, first_style))));

            let second = fit_width(
                &format!(
                    "  proxies={}  {}",
                    subscription_proxy_count_summary(app, subscription),
                    subscription_usage_value(subscription)
                ),
                content_width,
            );
            lines.push(with_padding(Line::from(Span::styled(
                second,
                Style::default().fg(Color::DarkGray),
            ))));
        } else {
            let first = fit_width("  Add Subscription", content_width);
            let first_style = if selected_row {
                selected_style()
            } else {
                Style::default().fg(Color::Yellow)
            };
            lines.push(with_padding(Line::from(Span::styled(first, first_style))));
        }
    }

    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

fn draw_help(
    frame: &mut Frame,
    config: &AppConfig,
    app: &ConfigApp,
    selected_row: Option<&SettingRow>,
    area: Rect,
) {
    const HOTKEY_ROWS: u16 = 9;
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(6), Constraint::Length(HOTKEY_ROWS)])
        .split(area);
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
                Line::from(Span::styled(
                    row.name.clone(),
                    Style::default().fg(Color::Cyan),
                )),
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
                Line::from(format!(
                    "{}: {}",
                    if app.selected_main == 0 {
                        "Profile"
                    } else {
                        "Subscription"
                    },
                    row.subscription
                )),
                Line::from(format!("Config: {}", row.features)),
            ]);
            if let Some(udp) = row.udp {
                lines.push(Line::from(format!("UDP: {}", on_off_upper(udp))));
            }
        }
    } else if app.page == Page::Profile {
        if let Some(profile) = config.proxy_profiles.get(app.selected_profile) {
            lines.extend([
                Line::from(Span::styled(
                    display_name(&profile.name),
                    Style::default().fg(Color::Cyan),
                )),
                Line::from(""),
                Line::from(format!(
                    "Active: {}",
                    if profile.name == config.active_proxy_profile {
                        "yes"
                    } else {
                        "no"
                    }
                )),
                Line::from(format!("Mode: {}", profile.mode)),
                Line::from(format!(
                    "Subscription: {}",
                    profile
                        .subscription
                        .as_deref()
                        .map_or_else(|| "-".into(), display_name)
                )),
            ]);
        } else {
            lines.extend([
                Line::from(Span::styled(
                    "Add Profile",
                    Style::default().fg(Color::Cyan),
                )),
                Line::from(""),
                Line::from("Create a reusable Global Proxy profile."),
            ]);
        }
    } else if app.page == Page::Subscription {
        if let Some(subscription) = config.subscriptions.get(app.selected_subscription) {
            lines.extend([
                Line::from(Span::styled(
                    display_name(&subscription.name),
                    Style::default().fg(Color::Cyan),
                )),
                Line::from(""),
                Line::from(format!(
                    "Refresh: {}",
                    subscription_refresh_label(subscription.refresh)
                )),
                Line::from(format!(
                    "Updated: {}",
                    format_optional_timestamp(subscription.updated_at.as_deref(), "never")
                )),
                Line::from(format!("State: {}", subscription_state_label(subscription))),
            ]);
            if let Some(error) = subscription.last_error.as_deref() {
                lines.push(Line::from(format!("Last error: {error}")));
            }
        } else {
            lines.extend([
                Line::from(Span::styled(
                    "Add Subscription",
                    Style::default().fg(Color::Cyan),
                )),
                Line::from(""),
                Line::from("Refresh: weekly default"),
                Line::from("Updated: never"),
            ]);
        }
    } else if let Some(row) = selected_row {
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
        Line::from("PgUp/PgDn page"),
        Line::from("Enter open/edit page"),
        Line::from("O on/off trigger"),
        Line::from("Esc back one page"),
        Line::from("←→/Tab switch section"),
        Line::from("F9 defaults"),
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
        Page::Profile => proxy_profile_rows(config),
        Page::ProfileConfig => proxy_profile_config_rows(config, app),
        Page::Subscription => subscription_rows(config),
        Page::SubscriptionDetail => subscription_detail_rows(paths, config, app),
        Page::SubscriptionRuleGroups => subscription_rule_group_rows(paths, config, app),
        Page::SubscriptionProxies => subscription_proxy_rows(paths, config, app),
        Page::SubscriptionRules => subscription_rule_rows(paths, config, app),
        Page::AddSubscription => add_subscription_rows(app),
        Page::Runtime => runtime_rows(paths, config, app),
        Page::Chat => chat_rows(config, app),
        Page::Exit => exit_rows(app.language),
        Page::ProxyConfig => proxy_config_rows(config, app),
        Page::ProxyConnections => compact_setting_rows(proxy_connection_rows(config, app)),
        Page::ProxyLogs => compact_setting_rows(proxy_log_rows(paths, config, app)),
        Page::ProxyGroups => proxy_group_rows(paths, config, app),
        Page::Mode => mode_rows(config, app),
        Page::Dns => dns_rows(config, app.language),
    }
}

fn proxy_profile_rows(config: &AppConfig) -> Vec<SettingRow> {
    let mut rows = config
        .proxy_profiles
        .iter()
        .map(|profile| SettingRow {
            label: display_name(&profile.name),
            value: if profile.name == config.active_proxy_profile {
                "active".into()
            } else {
                profile.mode.clone()
            },
            help: proxy_profile_summary(profile),
            kind: RowKind::Submenu(Page::ProfileConfig),
        })
        .collect::<Vec<_>>();

    rows.push(SettingRow {
        label: "Add Profile".into(),
        value: "new".into(),
        help: "Create another reusable Global Proxy profile.".into(),
        kind: RowKind::Action(ActionKind::AddPortProxy),
    });
    rows
}

fn proxy_profile_config_rows(config: &AppConfig, app: &ConfigApp) -> Vec<SettingRow> {
    ProfileConfigField::ALL
        .iter()
        .map(|field| SettingRow {
            label: field.label().into(),
            value: proxy_profile_field_value(config, app, *field),
            help: proxy_profile_field_help(*field).into(),
            kind: match field {
                ProfileConfigField::Name => RowKind::Input(InputMode::ProxyProfileName),
                ProfileConfigField::Active => RowKind::Info,
                ProfileConfigField::Subscription => RowKind::Choice(ChoiceAction::Subscription),
                ProfileConfigField::Mode => RowKind::Choice(ChoiceAction::Mode),
                ProfileConfigField::ProxyGroups => RowKind::Submenu(Page::ProxyGroups),
                ProfileConfigField::Activate => RowKind::Action(ActionKind::SelectProxy),
                ProfileConfigField::Delete => RowKind::Action(ActionKind::DeletePortProxy),
            },
        })
        .collect()
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
    let summary = subscription_profile_summary_for_view(app, subscription);
    vec![
        SettingRow {
            label: "Overview".into(),
            value: subscription_usage_value(subscription),
            help: format!(
                "URL: {} | profile: {} | updated: {}",
                subscription.url,
                profile.display(),
                format_optional_timestamp(subscription.updated_at.as_deref(), "-")
            ),
            kind: RowKind::Info,
        },
        SettingRow {
            label: "URL".into(),
            value: subscription.url.clone(),
            help: "Edit this subscription URL directly.".into(),
            kind: RowKind::Input(InputMode::SubscriptionUrl),
        },
        SettingRow {
            label: "Refresh".into(),
            value: subscription_refresh_label(subscription.refresh).into(),
            help: "Automatic update interval for this subscription.".into(),
            kind: RowKind::Choice(ChoiceAction::SubscriptionRefresh),
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
            value: if summary.loading {
                "loading".into()
            } else {
                format!("{} nodes", summary.proxies)
            },
            help: "Inspect proxy nodes from the local subscription profile. Delay checks require this subscription to be selected in Main.".into(),
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
            value: format_optional_timestamp(subscription.updated_at.as_deref(), "never"),
            help: "Refresh from URL. Failure records last_error and keeps the existing profile file.".into(),
            kind: RowKind::Action(ActionKind::UpdateSubscription),
        },
        SettingRow {
            label: "Delete".into(),
            value: "confirm".into(),
            help: "Delete this subscription and its local profile file after confirmation.".into(),
            kind: RowKind::Action(ActionKind::DeleteSubscription),
        },
    ]
}

fn subscription_rule_group_rows(
    paths: &Paths,
    config: &AppConfig,
    app: &ConfigApp,
) -> Vec<SettingRow> {
    let Some(subscription) = selected_subscription(config, app) else {
        return empty_rows("No Subscription", "Go back and add a subscription first.");
    };

    let groups = subscription_rule_groups_for_view(paths, config, app);
    if groups.is_empty() && subscription.rule_selections.is_empty() {
        if cached_subscription_profile(app, subscription).is_none()
            && app.subscription_profile_refresh.is_some()
        {
            return empty_rows(
                "Profile Loading",
                "Subscription profile cache is refreshing.",
            );
        }
        return empty_rows(
            "No Groups",
            "No proxy-groups were found in the local subscription profile.",
        );
    }

    groups
        .iter()
        .map(|group| {
            let saved = subscription
                .rule_selections
                .get(&group.name)
                .map_or_else(|| display_name(&group.now), |value| display_name(value));
            SettingRow {
                label: display_name(&group.name),
                value: format!("{} current {}", group.kind, saved),
                help: if group.all.is_empty() {
                    "Saved selection exists, but this group has no local proxy list.".into()
                } else {
                    "Rules target this group name. Enter chooses this group's selected outbound proxy."
                        .into()
                },
                kind: if group.all.is_empty() {
                    RowKind::Info
                } else {
                    RowKind::Submenu(Page::ProxyGroups)
                },
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
            help: "Saved selected outbound for this proxy group.".into(),
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
        if cached_subscription_profile(app, subscription).is_none()
            && app.subscription_profile_refresh.is_some()
        {
            return empty_rows(
                "Profile Loading",
                "Subscription profile cache is refreshing.",
            );
        }
        return empty_rows(
            "No Proxies",
            "No proxy nodes found in the local subscription profile.",
        );
    }

    let mut rows = proxies
        .into_iter()
        .map(|proxy| SettingRow {
            value: proxy_check_row_value(
                app.proxy_delays
                    .get(&subscription_proxy_delay_key(subscription, &proxy))
                    .map(String::as_str),
                subscription_proxy_is_selected(config, app, subscription, &proxy),
            ),
            label: if subscription_proxy_is_selected(config, app, subscription, &proxy) {
                format!("* {}", display_name(&proxy))
            } else {
                display_name(&proxy)
            },
            help: subscription_proxy_help(config, app, subscription, &proxy),
            kind: RowKind::Action(ActionKind::TestProxyDelay),
        })
        .collect::<Vec<_>>();

    rows.push(SettingRow {
        label: "Check All".into(),
        value: format!("{} nodes", rows.len()),
        help: "Run delay checks for every proxy in this subscription.".into(),
        kind: RowKind::Action(ActionKind::TestAllProxyDelays),
    });
    rows
}

fn proxy_check_row_value(delay: Option<&str>, selected: bool) -> String {
    match (delay, selected) {
        (Some(delay), true) => format!("{delay} selected"),
        (Some(delay), false) => delay.to_string(),
        (None, true) => "selected".into(),
        (None, false) => "Check".into(),
    }
}

fn subscription_proxy_help(
    config: &AppConfig,
    app: &ConfigApp,
    subscription: &Subscription,
    proxy: &str,
) -> String {
    let selected_by = subscription_proxy_selection_sources(config, app, subscription, proxy);
    let selected = if selected_by.is_empty() {
        String::new()
    } else {
        format!(" Selected by: {}.", selected_by.join(", "))
    };
    let action = "Runs mihomo provider healthcheck through the hidden subscription check provider.";
    format!("{action}{selected}")
}

fn subscription_proxy_is_selected(
    config: &AppConfig,
    app: &ConfigApp,
    subscription: &Subscription,
    proxy: &str,
) -> bool {
    !subscription_proxy_selection_sources(config, app, subscription, proxy).is_empty()
}

fn subscription_proxy_selection_sources(
    config: &AppConfig,
    app: &ConfigApp,
    subscription: &Subscription,
    proxy: &str,
) -> Vec<String> {
    let mut sources = Vec::new();
    for (group, selected_proxy) in &subscription.rule_selections {
        if selected_proxy == proxy {
            push_unique(&mut sources, display_name(group));
        }
    }

    if is_selected_subscription_active(config, subscription) {
        if config.runtime_mode.eq_ignore_ascii_case("global")
            && config
                .proxy_selections
                .get("GLOBAL")
                .is_some_and(|selected_proxy| selected_proxy == proxy)
        {
            push_unique(&mut sources, "GLOBAL".into());
        }
        for group in &app.runtime.groups {
            if group.now == proxy {
                push_unique(&mut sources, display_name(&group.name));
            }
        }
    }

    sources
}

fn push_unique(values: &mut Vec<String>, value: String) {
    if !values.iter().any(|existing| existing == &value) {
        values.push(value);
    }
}

fn subscription_rule_rows(_paths: &Paths, config: &AppConfig, app: &ConfigApp) -> Vec<SettingRow> {
    let Some(subscription) = selected_subscription(config, app) else {
        return empty_rows("No Subscription", "Go back and add a subscription first.");
    };
    let summary = subscription_profile_summary_for_view(app, subscription);
    if summary.loading {
        return empty_rows(
            "Profile Loading",
            "Subscription profile cache is refreshing.",
        );
    }
    if !summary.exists {
        return empty_rows(
            "Profile Missing",
            "Update this subscription before viewing its rules.",
        );
    }
    if let Some(err) = summary.error {
        return empty_rows("Profile Error", &err);
    }

    let mut rows = cached_subscription_profile(app, subscription)
        .map(|profile| profile.rule_rows.clone())
        .unwrap_or_default();
    if rows.is_empty() {
        rows.push(SettingRow {
            label: "No Rules".into(),
            value: "empty".into(),
            help: "The local subscription profile has no rules or rule-providers.".into(),
            kind: RowKind::Info,
        });
    }
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
                ProxyConfigField::ServiceStatus | ProxyConfigField::TrafficStatus => {
                    RowKind::Status
                }
                ProxyConfigField::Connections => RowKind::StatusSubmenu(Page::ProxyConnections),
                ProxyConfigField::Enabled => {
                    if app.selected_main == 0 {
                        RowKind::Status
                    } else {
                        RowKind::Toggle(ToggleAction::PortProxy)
                    }
                }
                ProxyConfigField::Profile => RowKind::Choice(ChoiceAction::Profile),
                ProxyConfigField::Subscription => RowKind::Choice(ChoiceAction::Subscription),
                ProxyConfigField::Mode => RowKind::Choice(ChoiceAction::Mode),
                ProxyConfigField::ProxyGroups => RowKind::Submenu(Page::ProxyGroups),
                ProxyConfigField::LocalPort => match main_proxy_kind(config, app.selected_main) {
                    MainProxyKind::System => RowKind::Input(InputMode::MixedPort),
                    MainProxyKind::Service => RowKind::Input(InputMode::ServicePort),
                    MainProxyKind::AddPortProxy => RowKind::Action(ActionKind::AddPortProxy),
                },
                ProxyConfigField::Delete => RowKind::Action(ActionKind::DeletePortProxy),
                ProxyConfigField::OsProxy => RowKind::Toggle(ToggleAction::SystemProxy),
                ProxyConfigField::Pac => RowKind::Info,
                ProxyConfigField::Tun => RowKind::Toggle(ToggleAction::Tun),
                ProxyConfigField::Logs => RowKind::Action(ActionKind::Logs),
            };
            SettingRow {
                label: field.label().into(),
                value: proxy_config_field_value(config, app, *field),
                help: proxy_config_field_help(config, app, *field).into(),
                kind,
            }
        })
        .collect()
}

fn proxy_group_rows(paths: &Paths, config: &AppConfig, app: &ConfigApp) -> Vec<SettingRow> {
    if app.rule_group_selection.is_some() {
        return subscription_rule_proxy_rows(paths, config, app);
    }

    if proxy_groups_page_is_global_proxy_list(config, app) {
        let proxies = route_proxy_names(paths, config, app);
        if proxies.is_empty() {
            return empty_rows(
                "No Proxies",
                "Update the selected subscription or start runtime before choosing a proxy.",
            );
        }
        let saved = selected_global_proxy(config, app);
        let subscription_name = proxy_subscription_name(config, app);
        return proxies
            .into_iter()
            .map(|proxy| SettingRow {
                label: display_name(&proxy),
                value: proxy_check_row_value(
                    subscription_name.and_then(|name| {
                        app.proxy_delays
                            .get(&subscription_proxy_delay_key_for_name(name, &proxy))
                            .map(String::as_str)
                    }),
                    saved.as_deref() == Some(proxy.as_str()),
                ),
                help: "Enter saves this proxy for global mode.".into(),
                kind: RowKind::Action(ActionKind::SelectProxy),
            })
            .collect();
    }

    match app.proxy_pane {
        ProxyPane::Groups => {
            let runtime = selected_mihomo_runtime(config, app);
            if runtime.groups.is_empty() {
                return vec![SettingRow {
                    label: "No Groups".into(),
                    value: "offline".into(),
                    help: "Start the daemon or refresh runtime before choosing a proxy.".into(),
                    kind: RowKind::Info,
                }];
            }
            runtime
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
            let Some(group) = app.current_group(config) else {
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
                    value: proxy_check_row_value(
                        proxy_subscription_name(config, app).and_then(|name| {
                            app.proxy_delays
                                .get(&subscription_proxy_delay_key_for_name(name, proxy))
                                .map(String::as_str)
                        }),
                        proxy == &group.now,
                    ),
                    help: format!("Enter saves this proxy for {}.", display_name(&group.name)),
                    kind: RowKind::Action(ActionKind::SelectProxy),
                })
                .collect()
        }
    }
}

fn subscription_rule_proxy_rows(
    paths: &Paths,
    config: &AppConfig,
    app: &ConfigApp,
) -> Vec<SettingRow> {
    let Some(context) = app.rule_group_selection.as_ref() else {
        return empty_rows("No Rule Group", "Go back and choose a rule group first.");
    };
    let Some(group) = selected_rule_group_for_proxy_selection(paths, config, app) else {
        return empty_rows("No Rule Group", "The selected rule group was not found.");
    };
    if group.all.is_empty() {
        return empty_rows(
            "No Proxies",
            "This rule group has no concrete proxy nodes in the local profile.",
        );
    }
    let saved = config
        .subscriptions
        .get(context.subscription_index)
        .and_then(|subscription| subscription.rule_selections.get(&context.group_name));
    let subscription_name = config
        .subscriptions
        .get(context.subscription_index)
        .map(|subscription| subscription.name.as_str());
    group
        .all
        .iter()
        .map(|proxy| SettingRow {
            label: display_name(proxy),
            value: proxy_check_row_value(
                subscription_name.and_then(|name| {
                    app.proxy_delays
                        .get(&subscription_proxy_delay_key_for_name(name, proxy))
                        .map(String::as_str)
                }),
                saved == Some(proxy),
            ),
            help: format!(
                "Enter sets this proxy as the selected outbound for group {}.",
                display_name(&context.group_name)
            ),
            kind: RowKind::Action(ActionKind::SelectProxy),
        })
        .collect()
}

fn runtime_rows(paths: &Paths, config: &AppConfig, app: &ConfigApp) -> Vec<SettingRow> {
    RuntimeItem::ALL
        .iter()
        .map(|item| {
            let kind = match item {
                RuntimeItem::Service => RowKind::Action(ActionKind::Service),
                RuntimeItem::Autostart => RowKind::Toggle(ToggleAction::Autostart),
                RuntimeItem::Logs => RowKind::Action(ActionKind::Logs),
                RuntimeItem::CorePath => RowKind::Choice(ChoiceAction::CoreSource),
                RuntimeItem::CoreUpdate => RowKind::Action(ActionKind::UpdateCore),
                RuntimeItem::Controller => RowKind::Input(InputMode::Controller),
                RuntimeItem::Refresh => RowKind::Action(ActionKind::RefreshRuntime),
                RuntimeItem::LlmSection => RowKind::SoftSection,
                RuntimeItem::LlmProvider => RowKind::Choice(ChoiceAction::LlmProvider),
                RuntimeItem::LlmBaseUrl => RowKind::Input(InputMode::LlmBaseUrl),
                RuntimeItem::LlmModel => RowKind::Choice(ChoiceAction::LlmModel),
                RuntimeItem::LlmApiKey => RowKind::Input(InputMode::LlmApiKey),
                RuntimeItem::LlmProvidersUpdate => RowKind::Action(ActionKind::UpdateLlmProviders),
                RuntimeItem::TestAssistant => RowKind::Action(ActionKind::TestAssistant),
            };
            SettingRow {
                label: item.label_for(app.language).into(),
                value: runtime_item_value(paths, config, app, *item),
                help: runtime_item_help(*item, app.language).into(),
                kind,
            }
        })
        .collect()
}

fn dns_rows(config: &AppConfig, language: Language) -> Vec<SettingRow> {
    DnsItem::ALL
        .iter()
        .map(|item| {
            let kind = match item {
                DnsItem::Enabled => RowKind::Toggle(ToggleAction::Dns),
                DnsItem::Listen => RowKind::Input(InputMode::DnsListen),
                DnsItem::LanDomains => RowKind::Input(InputMode::DnsLanDomains),
                DnsItem::LanNameserver => RowKind::Input(InputMode::DnsLanNameserver),
                DnsItem::NameserverPolicy => RowKind::Input(InputMode::DnsNameserverPolicy),
                DnsItem::DirectNameserver => RowKind::Input(InputMode::DnsDirectNameserver),
                DnsItem::DirectFollowPolicy => RowKind::Toggle(ToggleAction::DnsDirectFollowPolicy),
                DnsItem::Nameserver => RowKind::Input(InputMode::DnsNameserver),
                DnsItem::Fallback => RowKind::Input(InputMode::DnsFallback),
                DnsItem::FakeIpFilter => RowKind::Input(InputMode::DnsFakeIpFilter),
            };
            SettingRow {
                label: item.label_for(language).into(),
                value: dns_item_value(config, *item),
                help: dns_item_help(*item, language).into(),
                kind,
            }
        })
        .collect()
}

fn chat_rows(config: &AppConfig, app: &ConfigApp) -> Vec<SettingRow> {
    vec![
        SettingRow {
            label: "Assistant".into(),
            value: chat_session_status(app).into(),
            help: format!(
                "{} {} {}",
                text(
                    app.language,
                    "Native chat assistant using",
                    "内置 chat assistant 使用"
                ),
                config.llm.model,
                text(
                    app.language,
                    "with bundled clashtui/mihomo knowledge.",
                    "和内置 clashtui/mihomo knowledge。"
                )
            ),
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
                "LLM applies structured patches to draft; press F10 to save/restart. Runtime: {}",
                app.runtime.error.as_deref().unwrap_or("online")
            ),
            kind: RowKind::Info,
        },
    ]
}

fn chat_session_status(app: &ConfigApp) -> &'static str {
    if app.chat.task.is_some() {
        "running"
    } else if app.chat.entries.is_empty() {
        "idle"
    } else {
        "done"
    }
}

fn exit_rows(language: Language) -> Vec<SettingRow> {
    ExitItem::ALL
        .iter()
        .map(|item| SettingRow {
            label: item.label_for(language).into(),
            value: item.value().into(),
            help: item.help_for(language).into(),
            kind: match item {
                ExitItem::RuntimeSection | ExitItem::SaveSection | ExitItem::ExitSection => {
                    RowKind::Section
                }
                ExitItem::StartRuntime => RowKind::Action(ActionKind::StartRuntime),
                ExitItem::StopRuntime => RowKind::Action(ActionKind::StopRuntime),
                ExitItem::ReloadRuntime => RowKind::Action(ActionKind::ReloadRuntime),
                ExitItem::RestartRuntime => RowKind::Action(ActionKind::RestartRuntime),
                ExitItem::SaveConfig => RowKind::Action(ActionKind::SaveConfig),
                ExitItem::SaveRestart => RowKind::Action(ActionKind::SaveRestart),
                ExitItem::SaveRestartExit => RowKind::Action(ActionKind::SaveRestartExit),
                ExitItem::ExitWithoutSaving => RowKind::Action(ActionKind::ExitWithoutSaving),
                ExitItem::LoadDefaults => RowKind::Action(ActionKind::LoadDefaults),
                ExitItem::Exit => RowKind::Action(ActionKind::Exit),
            },
        })
        .collect()
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
        Page::Profile => "proxy profiles / activate / reuse",
        Page::ProfileConfig => "profile settings / subscription / mode",
        Page::Subscription => "profiles / refresh / usage",
        Page::SubscriptionDetail => "profile detail / maintenance",
        Page::SubscriptionRuleGroups => "rule-mode group selections",
        Page::SubscriptionProxies => "proxy nodes / delay checks",
        Page::SubscriptionRules => "rules / providers / raw profile",
        Page::AddSubscription => "new profile / URL / refresh",
        Page::Runtime => "service / controller / logs",
        Page::Chat => "assistant / structured patch",
        Page::Exit => "runtime / save / close",
        Page::ProxyConfig => "proxy settings / listener / TUN",
        Page::ProxyConnections => "active connections / rule / chain",
        Page::ProxyLogs => "runtime log tail",
        Page::ProxyGroups => "proxy group / server selection",
        Page::Mode => "runtime mode",
        Page::Dns => "resolve strategy / nameservers",
    }
}

fn setting_row_line(row: &SettingRow, content_width: usize, selected: bool) -> Line<'static> {
    let submenu = matches!(row.kind, RowKind::Submenu(_) | RowKind::StatusSubmenu(_));
    let prefix = if submenu { "> " } else { "  " };
    let label = fit_width(
        &format!("{prefix}{:<LABEL_WIDTH$} ", row.label),
        content_width,
    );
    let value_width = content_width.saturating_sub(label.chars().count());
    if value_width == 0 {
        return Line::from(Span::styled(label, row_style(row, selected)));
    }

    Line::from(vec![
        Span::styled(label, row_style(row, selected)),
        Span::styled(
            fit_width(&row.value, value_width),
            row_value_style(row, selected),
        ),
    ])
}

fn row_style(row: &SettingRow, selected: bool) -> Style {
    if matches!(row.kind, RowKind::Section) {
        return Style::default().fg(Color::DarkGray);
    }
    if matches!(row.kind, RowKind::SoftSection) {
        return Style::default().fg(Color::Gray).add_modifier(Modifier::DIM);
    }
    if selected {
        return selected_style();
    }
    if row_is_function_action(row) {
        Style::default().fg(Color::Yellow)
    } else if matches!(
        row.kind,
        RowKind::Info | RowKind::Status | RowKind::StatusSubmenu(_)
    ) {
        Style::default().fg(Color::Gray)
    } else {
        Style::default().fg(Color::White)
    }
}

fn row_value_style(row: &SettingRow, selected: bool) -> Style {
    if let Some(style) = proxy_delay_value_style(row).or_else(|| semantic_value_style(row)) {
        if selected {
            return style.bg(Color::White).add_modifier(Modifier::BOLD);
        }
        return style;
    }

    row_style(row, selected)
}

fn semantic_value_style(row: &SettingRow) -> Option<Style> {
    let value = row.value.trim().to_ascii_lowercase();
    if value.is_empty() {
        return None;
    }

    if matches!(
        value.as_str(),
        "fail" | "failed" | "error" | "parse error" | "unavailable" | "offline"
    ) || value.ends_with(" error")
        || value.contains(" failed")
    {
        return Some(Style::default().fg(Color::Red));
    }

    if matches!(
        value.as_str(),
        "slow" | "pending" | "checking" | "unknown" | "missing"
    ) {
        return Some(Style::default().fg(Color::Yellow));
    }

    if matches!(value.as_str(), "ok" | "online" | "running") {
        return Some(Style::default().fg(Color::Green));
    }

    if matches!(value.as_str(), "selected" | "current") || value.contains(" selected") {
        return Some(Style::default().fg(Color::Cyan));
    }

    None
}

fn row_is_function_action(row: &SettingRow) -> bool {
    matches!(
        row.kind,
        RowKind::Action(
            ActionKind::AddPortProxy
                | ActionKind::AddSubscription
                | ActionKind::SaveSubscription
                | ActionKind::UpdateSubscription
                | ActionKind::DeleteSubscription
                | ActionKind::DeletePortProxy
                | ActionKind::EditSubscription
                | ActionKind::TestAllProxyDelays
                | ActionKind::Service
                | ActionKind::Logs
                | ActionKind::RefreshRuntime
                | ActionKind::UpdateLlmProviders
                | ActionKind::TestAssistant
                | ActionKind::UpdateCore
                | ActionKind::StartRuntime
                | ActionKind::StopRuntime
                | ActionKind::ReloadRuntime
                | ActionKind::RestartRuntime
                | ActionKind::LoadDefaults
                | ActionKind::SaveConfig
                | ActionKind::SaveRestart
                | ActionKind::SaveRestartExit
                | ActionKind::ExitWithoutSaving
                | ActionKind::Exit
        )
    )
}

fn proxy_delay_value_style(row: &SettingRow) -> Option<Style> {
    if !matches!(
        row.kind,
        RowKind::Action(ActionKind::TestProxyDelay | ActionKind::SelectProxy)
    ) {
        return None;
    }
    let value = row.value.trim().to_ascii_lowercase();
    if value
        .split_whitespace()
        .any(|part| matches!(part, "fail" | "failed" | "timeout" | "error"))
    {
        return Some(Style::default().fg(Color::Red));
    }
    let delay = value.split_whitespace().find_map(|part| {
        part.strip_suffix("ms")
            .and_then(|delay| delay.trim().parse::<u64>().ok())
    })?;
    if delay <= 800 {
        Some(Style::default().fg(Color::Green))
    } else {
        Some(Style::default().fg(Color::Yellow))
    }
}

fn required_value(value: &str) -> String {
    if value.trim().is_empty() {
        "(required)".into()
    } else {
        value.to_string()
    }
}

const fn text(language: Language, en: &'static str, zh_cn: &'static str) -> &'static str {
    if language.is_zh_cn() { zh_cn } else { en }
}

const fn runtime_item_help(item: RuntimeItem, language: Language) -> &'static str {
    if language.is_zh_cn() {
        return match item {
            RuntimeItem::Service => "安装或检查 TUN 使用的 privileged Service。",
            RuntimeItem::Autostart => "从 config 切换登录自启动。",
            RuntimeItem::Logs => "打开 Runtime logs；此功能尚未完成。",
            RuntimeItem::CorePath => "选择 auto、托管 Mihomo release、托管 alpha 或自定义路径。",
            RuntimeItem::CoreUpdate => "下载当前选择的托管 Mihomo core；重启后生效。",
            RuntimeItem::Controller => "clashtui 使用的 mihomo external controller URL。",
            RuntimeItem::Refresh => "从 mihomo 刷新 Runtime 信息。",
            RuntimeItem::LlmSection => "LLM assistant endpoint、model、key 和 provider catalog。",
            RuntimeItem::LlmProvider => "选择 OpenAI-compatible assistant endpoint preset。",
            RuntimeItem::LlmBaseUrl => "assistant chat/completions endpoint 的 Base URL。",
            RuntimeItem::LlmModel => "发送给 OpenAI-compatible endpoint 的 Model 名称。",
            RuntimeItem::LlmApiKey => "粘贴 API key；会保存到 llm-providers.yaml。",
            RuntimeItem::LlmProvidersUpdate => {
                "手动合并内置 provider 更新，并保留本地 API key 和 models。"
            }
            RuntimeItem::TestAssistant => "向当前 LLM 发送 hello，并显示 response 或错误。",
        };
    }
    match item {
        RuntimeItem::Service => "Install or inspect the privileged service used by TUN.",
        RuntimeItem::Autostart => "Toggle login autostart from config.",
        RuntimeItem::Logs => "Open runtime logs. Implementation is pending.",
        RuntimeItem::CorePath => {
            "Choose auto, managed Mihomo release, managed alpha, or custom path."
        }
        RuntimeItem::CoreUpdate => {
            "Download the latest selected managed Mihomo core; restart to use it."
        }
        RuntimeItem::Controller => "Mihomo external controller URL used by clashtui.",
        RuntimeItem::Refresh => "Refresh runtime information from mihomo.",
        RuntimeItem::LlmSection => "LLM assistant endpoint, model, key, and provider catalog.",
        RuntimeItem::LlmProvider => "Choose the OpenAI-compatible assistant endpoint preset.",
        RuntimeItem::LlmBaseUrl => "Base URL for the assistant chat/completions endpoint.",
        RuntimeItem::LlmModel => "Model name sent to the configured OpenAI-compatible endpoint.",
        RuntimeItem::LlmApiKey => "Paste an API key. It is saved in llm-providers.yaml.",
        RuntimeItem::LlmProvidersUpdate => {
            "Manually merge bundled provider updates while preserving local API keys and models."
        }
        RuntimeItem::TestAssistant => "Send hello to the configured LLM and show the response.",
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
    let core_path = core::resolve_core_path(paths, config)
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
            let route_label = if main_proxy_kind(config, index) == MainProxyKind::System {
                "profile"
            } else {
                "sub"
            };
            ListItem::new(vec![
                Line::from(Span::styled(
                    format!("{:<18} {:<7} {}", row.name, row.kind, row.listen),
                    style,
                )),
                Line::from(Span::styled(
                    format!(
                        "     mode={:<8} {}={} {}",
                        row.mode, route_label, row.subscription, feature_summary
                    ),
                    Style::default().fg(Color::Gray),
                )),
                Line::from(Span::styled(
                    format!("     {}", proxy_runtime_line(config, app, index)),
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
            Line::from("Enter: choose profile"),
            Line::from("c: proxy config"),
        ]),
        MainProxyKind::AddPortProxy => lines.extend([
            Line::from("Action: add port proxy"),
            Line::from("Enter: create and configure"),
        ]),
        _ => lines.extend([
            Line::from(format!("State: {}", on_off_upper(row.enabled))),
            Line::from(format!("Listener: {}={}", row.listener, row.listen)),
            Line::from("Enter: proxy config"),
        ]),
    }
    lines.extend([
        Line::from("F10 save & restart"),
        Line::from("Esc at root opens Exit"),
    ]);
    frame.render_widget(bios_panel("Selected Proxy Config", lines), area);
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
    draw_groups(frame, config, app, columns[0]);
    draw_proxy_options(frame, config, app, columns[1]);
    draw_proxy_help(frame, config, app, columns[2]);
}

fn draw_groups(frame: &mut Frame, config: &AppConfig, app: &ConfigApp, area: Rect) {
    let runtime = selected_mihomo_runtime(config, app);
    let items = if runtime.groups.is_empty() {
        vec![ListItem::new("No groups. Start daemon, then refresh.")]
    } else {
        runtime
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
    if !runtime.groups.is_empty() {
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

fn draw_proxy_options(frame: &mut Frame, config: &AppConfig, app: &ConfigApp, area: Rect) {
    let Some(group) = app.current_group(config) else {
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

    if let Some(group) = app.current_group(config) {
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
        Line::from(proxy_config_field_help(config, app, field)),
        Line::from(""),
        Line::from(format!("Proxy: {}", row.name)),
        Line::from(format!("Kind: {}", row.kind)),
        Line::from(format!("Listen: {}", row.listen)),
        Line::from(format!("Mode: {}", row.mode)),
        Line::from(format!(
            "{}: {}",
            if app.selected_main == 0 {
                "Profile"
            } else {
                "Subscription"
            },
            row.subscription
        )),
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

fn draw_runtime_page(
    frame: &mut Frame,
    paths: &Paths,
    config: &AppConfig,
    app: &ConfigApp,
    area: Rect,
) {
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
        .split(area);
    draw_runtime_menu(frame, paths, config, app, columns[0]);
    draw_runtime_help(frame, config, app, columns[1]);
}

fn draw_runtime_menu(
    frame: &mut Frame,
    paths: &Paths,
    config: &AppConfig,
    app: &ConfigApp,
    area: Rect,
) {
    let items = RuntimeItem::ALL
        .iter()
        .map(|item| {
            ListItem::new(Line::from(vec![
                Span::raw(format!("{:<18}", item.label_for(app.language))),
                Span::styled(
                    runtime_item_value(paths, config, app, *item),
                    Style::default().fg(Color::Gray),
                ),
            ]))
        })
        .collect::<Vec<_>>();

    let mut state = ListState::default();
    state.select(Some(app.selected_runtime));
    let list = List::new(items)
        .block(focused_block(
            text(app.language, "Runtime Menu", "Runtime 菜单"),
            true,
        ))
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
        Line::from(text(
            app.language,
            "Runtime contains non-proxy operational settings.",
            "Runtime 包含非 proxy 的运行设置。",
        )),
        Line::from(text(
            app.language,
            "Proxy mode and server selection live on each Proxy config page.",
            "Proxy mode 和 server selection 在各 Proxy config 页面中设置。",
        )),
        Line::from(""),
        Line::from(format!("Core: {}", core_source_value(config))),
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
        Line::from(text(
            app.language,
            "Service and TUN permission installers are planned.",
            "Service 和 TUN 权限安装器仍在规划中。",
        )),
        Line::from(text(
            app.language,
            "Enter opens editable items where available.",
            "可编辑项目可按 Enter 打开。",
        )),
    ];
    frame.render_widget(
        bios_panel(text(app.language, "Runtime Help", "Runtime 帮助"), lines),
        area,
    );
}

fn draw_chat_page(
    frame: &mut Frame,
    paths: &Paths,
    config: &AppConfig,
    app: &ConfigApp,
    area: Rect,
) {
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(66), Constraint::Percentage(34)])
        .split(area);
    let left = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(8), Constraint::Length(5)])
        .split(columns[0]);

    let mut transcript = vec![
        Line::from(Span::styled(
            "Chat",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
    ];
    let transcript_width = left[0].width.saturating_sub(4) as usize;
    let visible = left[0].height.saturating_sub(4) as usize;
    let lines = chat_transcript_lines(app, visible, transcript_width);
    if lines.is_empty() {
        transcript.extend([
            Line::from(text(
                app.language,
                "Ask about proxy, DNS, TUN, subscriptions, runtime logs, or config changes.",
                "可以询问 proxy、DNS、TUN、订阅、Runtime logs 或 config 修改。",
            )),
            Line::from(""),
            Line::from(text(app.language, "Examples:", "示例:")),
            Line::from(text(
                app.language,
                "  Explain why TUN is unavailable",
                "  解释为什么 TUN 不可用",
            )),
            Line::from(text(
                app.language,
                "  Add a SOCKS5 Port Proxy on 7081 using HK-01",
                "  添加一个使用 HK-01 的 SOCKS5 Port Proxy，端口 7081",
            )),
        ]);
    } else {
        transcript.extend(lines);
    }
    frame.render_widget(bios_panel_preserve_indent("Assistant", transcript), left[0]);

    let input_width = left[1].width.saturating_sub(4) as usize;
    let mut input_lines = chat_input_lines(app.chat.input.as_str(), input_width, 2);
    input_lines.push(Line::from(Span::styled(
        fit_width(
            text(
                app.language,
                "Enter send | Ctrl+J newline | Esc cancel/back | F10 save & restart",
                "Enter 发送 | Ctrl+J 换行 | Esc 取消/返回 | F10 保存并重启",
            ),
            input_width,
        ),
        Style::default().fg(Color::Gray),
    )));
    frame.render_widget(panel("Input", input_lines), left[1]);

    let inspector = vec![
        Line::from(Span::styled(
            "Agent",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(format!(
            "Status: {}",
            if app.chat.task.is_some() {
                text(app.language, "running", "运行中")
            } else {
                text(app.language, "idle", "空闲")
            }
        )),
        Line::from(format!(
            "Provider: {}",
            llm_provider_label(&config.llm.provider)
        )),
        Line::from(format!("Model: {}", config.llm.model)),
        Line::from(format!("Key: {}", agent::api_key_status(paths, config))),
        Line::from(format!("Base: {}", config.llm.base_url)),
        Line::from(fit_width(
            &llm_provider_note(&config.llm.provider),
            columns[1].width.saturating_sub(4) as usize,
        )),
        Line::from(""),
        Line::from(Span::styled(
            if app.chat.usage.estimated {
                "Usage (mixed/est.)"
            } else {
                "Usage"
            },
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(format!(
            "Tokens: up {} / down {}",
            format_count(app.chat.usage.prompt_tokens),
            format_count(app.chat.usage.completion_tokens)
        )),
        Line::from(format!(
            "Context: {} tok est / {} chars",
            format_count(app.chat.usage.context_tokens),
            format_count(app.chat.usage.context_chars)
        )),
        Line::from(format!(
            "Msgs: {} Turns: {} Tools: {}",
            app.chat.usage.context_messages, app.chat.usage.turns, app.chat.usage.tool_calls
        )),
        Line::from(""),
        Line::from(format!(
            "Runtime: {}",
            app.runtime.error.as_deref().unwrap_or("online")
        )),
        Line::from(format!(
            "Mixed: {}:{}",
            config.proxy_host, config.mixed_port
        )),
        Line::from(format!("TUN: {}", on_off(config.tun.enable))),
        Line::from(format!(
            "DNS: {} {}",
            on_off(config.dns.enable),
            config.dns.listen
        )),
    ];
    frame.render_widget(
        bios_panel(text(app.language, "Inspector", "检查器"), inspector),
        columns[1],
    );
}

fn chat_transcript_lines(app: &ConfigApp, visible: usize, width: usize) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for entry in &app.chat.entries {
        let (label, style) = match entry.kind {
            ChatEntryKind::User => ("user", Style::default().fg(Color::Cyan)),
            ChatEntryKind::Assistant => ("assistant", Style::default().fg(Color::White)),
            ChatEntryKind::Tool => ("tool", Style::default().fg(Color::Gray)),
            ChatEntryKind::Patch => ("patch", Style::default().fg(Color::Yellow)),
            ChatEntryKind::Error => ("error", Style::default().fg(Color::Red)),
        };
        if entry.kind != ChatEntryKind::Patch {
            lines.push(Line::from(Span::styled(
                label,
                style.add_modifier(Modifier::BOLD),
            )));
        }
        if entry.content.is_empty() {
            lines.push(Line::from(Span::styled("  ...", style)));
        } else if entry.kind == ChatEntryKind::Patch {
            lines.extend(wrap_patch_entry_lines(&entry.content, width));
        } else {
            lines.extend(wrap_prefixed_lines(&entry.content, "  ", width, style));
        }
        lines.push(Line::from(""));
    }
    let end = lines.len().saturating_sub(app.chat.scroll);
    let start = end.saturating_sub(visible);
    lines
        .into_iter()
        .skip(start)
        .take(end.saturating_sub(start))
        .collect()
}

fn wrap_patch_entry_lines(value: &str, width: usize) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for raw in value.split('\n') {
        let (prefix, content) = patch_diff_line_parts(raw);
        lines.extend(wrap_prefixed_lines(
            content,
            prefix,
            width,
            patch_entry_line_style(raw),
        ));
    }
    lines
}

fn patch_diff_line_parts(value: &str) -> (&'static str, &str) {
    if let Some(content) = value.strip_prefix('+') {
        (" +", content)
    } else if let Some(content) = value.strip_prefix('-') {
        (" -", content)
    } else if let Some(content) = value.strip_prefix(' ') {
        ("  ", content)
    } else {
        ("", value)
    }
}

fn patch_entry_line_style(value: &str) -> Style {
    if value.starts_with('+') {
        Style::default().fg(Color::White).bg(PATCH_DIFF_ADD_BG)
    } else if value.starts_with('-') {
        Style::default().fg(Color::White).bg(PATCH_DIFF_REMOVE_BG)
    } else {
        Style::default().fg(Color::White)
    }
}

fn wrap_prefixed_lines(
    value: &str,
    prefix: &'static str,
    width: usize,
    style: Style,
) -> Vec<Line<'static>> {
    let prefix_width = prefix.chars().count();
    let content_width = width.saturating_sub(prefix_width).max(1);
    let mut lines = Vec::new();
    for raw in value.split('\n') {
        if raw.is_empty() {
            lines.push(Line::from(Span::styled(prefix.to_string(), style)));
            continue;
        }

        let mut current = String::new();
        for ch in raw.chars() {
            if current.chars().count() >= content_width {
                lines.push(Line::from(Span::styled(
                    format!("{prefix}{current}"),
                    style,
                )));
                current.clear();
            }
            current.push(ch);
        }
        lines.push(Line::from(Span::styled(
            format!("{prefix}{current}"),
            style,
        )));
    }
    lines
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
                Span::raw(format!("{:<24}", item.label_for(app.language))),
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
        .block(focused_block(
            text(app.language, "DNS Menu", "DNS 菜单"),
            true,
        ))
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
            item.label_for(app.language),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(dns_item_help(item, app.language)),
        Line::from(""),
        Line::from(text(
            app.language,
            "List values are comma separated.",
            "列表值使用逗号分隔。",
        )),
        Line::from("Policy format: +.example.com=1.1.1.1, 8.8.8.8"),
        Line::from(text(
            app.language,
            "Use system for the OS resolver, or IP/DoH/DoT servers.",
            "可使用 system 表示 OS resolver，也可填写 IP/DoH/DoT servers。",
        )),
        Line::from(""),
        Line::from(if app.language.is_zh_cn() {
            format!("生效 policy 条目: {}", policy.len())
        } else {
            format!("Effective policy entries: {}", policy.len())
        }),
        Line::from(format!(
            "{}: {}",
            text(app.language, "Configured policy", "已配置 policy"),
            compact_nameserver_policy(&config.dns.nameserver_policy)
        )),
        Line::from(format!(
            "{}: {}",
            text(app.language, "LAN domains", "LAN domains"),
            compact_list(&config.dns.lan_domains)
        )),
        Line::from(format!(
            "LAN DNS: {}",
            compact_list(&config.dns.lan_nameserver)
        )),
        Line::from(format!(
            "{}: {}",
            text(
                app.language,
                "Effective fake-IP filter",
                "生效 fake-IP filter"
            ),
            dns::effective_fake_ip_filter(&config.dns).len()
        )),
        Line::from(""),
        Line::from(text(
            app.language,
            "Typical LAN DNS: system, 192.168.0.1",
            "常见 LAN DNS: system, 192.168.0.1",
        )),
    ];
    frame.render_widget(
        panel(text(app.language, "DNS Help", "DNS 帮助"), lines),
        area,
    );
}

fn draw_footer(frame: &mut Frame, app: &ConfigApp, area: Rect, separator_column: u16) {
    frame.render_widget(Clear, area);
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

fn draw_input(frame: &mut Frame, app: &ConfigApp) {
    let (width, rows) = input_dialog_spec(app.input_mode);
    let area = fixed_rect(width, input_dialog_height(rows), frame.area());
    frame.render_widget(Clear, area);
    let display_input = if app.input_mode == InputMode::LlmApiKey {
        "*".repeat(app.input.chars().count())
    } else {
        app.input.clone()
    };
    let mut body = vec![
        popup_title_line(
            app.input_mode.title_for(app.language),
            area.width.saturating_sub(2),
        ),
        Line::from(""),
    ];
    body.extend(input_box_lines(
        &display_input,
        display_input.len(),
        area.width.saturating_sub(6) as usize,
        rows,
    ));
    body.extend([
        Line::from(""),
        input_action_line(app),
        Line::from(Span::styled(
            input_help_text(app),
            Style::default().fg(Color::Gray),
        ))
        .alignment(Alignment::Center),
    ]);

    frame.render_widget(dialog_panel(body), area);
}

fn input_action_line(app: &ConfigApp) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            text(app.language, " Save ", " 保存 "),
            button_style(app.input_focus == InputFocus::Save),
        ),
        Span::raw("  "),
        Span::styled(
            text(app.language, " Cancel ", " 取消 "),
            button_style(app.input_focus == InputFocus::Cancel),
        ),
    ])
    .alignment(Alignment::Center)
}

fn input_help_text(app: &ConfigApp) -> &'static str {
    if app.input_focus != InputFocus::Editor {
        return text(
            app.language,
            "Enter/Space activate | Tab switch | Esc cancel",
            "Enter/Space 执行 | Tab 切换 | Esc 取消",
        );
    }
    if is_number_input(app.input_mode) {
        text(
            app.language,
            "Edit value | Up/Down change | Tab buttons | Ctrl+S save | Esc cancel",
            "编辑数值 | Up/Down 调整 | Tab 按钮 | Ctrl+S 保存 | Esc 取消",
        )
    } else if is_multiline_input(app.input_mode) {
        text(
            app.language,
            "Enter newline | Tab buttons | Ctrl+S save | Esc cancel",
            "Enter 换行 | Tab 按钮 | Ctrl+S 保存 | Esc 取消",
        )
    } else {
        text(
            app.language,
            "Edit value | Tab buttons | Ctrl+S save | Esc cancel",
            "编辑内容 | Tab 按钮 | Ctrl+S 保存 | Esc 取消",
        )
    }
}

const fn input_dialog_spec(input_mode: InputMode) -> (u16, usize) {
    match input_mode {
        InputMode::SubscriptionUrl | InputMode::LlmBaseUrl => (URL_INPUT_WIDTH, URL_INPUT_ROWS),
        InputMode::LlmApiKey => (URL_INPUT_WIDTH, DEFAULT_INPUT_ROWS),
        InputMode::DnsLanDomains
        | InputMode::DnsLanNameserver
        | InputMode::DnsNameserverPolicy
        | InputMode::DnsDirectNameserver
        | InputMode::DnsNameserver
        | InputMode::DnsFallback
        | InputMode::DnsFakeIpFilter => (DNS_TEXT_INPUT_WIDTH, DNS_TEXT_INPUT_ROWS),
        _ => (DEFAULT_INPUT_WIDTH, DEFAULT_INPUT_ROWS),
    }
}

fn input_dialog_height(rows: usize) -> u16 {
    rows.saturating_add(9).try_into().unwrap_or(13)
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
    let is_llm_provider = dropdown == Dropdown::LlmProvider;
    let is_llm_catalog_dropdown = matches!(dropdown, Dropdown::LlmProvider | Dropdown::LlmModel);
    let height = if is_llm_catalog_dropdown {
        (options.len() as u16 + 6).clamp(8, 18)
    } else {
        (options.len() as u16 + 4).clamp(6, 12)
    };
    let width = if is_llm_catalog_dropdown { 72 } else { 46 };
    let area = fixed_rect(width, height, frame.area());
    frame.render_widget(Clear, area);

    let selected = app.selected_dropdown.min(options.len().saturating_sub(1));
    let max_visible = area
        .height
        .saturating_sub(if is_llm_catalog_dropdown { 6 } else { 4 })
        .max(1) as usize;
    let start = dropdown_window_start(selected, options.len(), max_visible);
    let end = (start + max_visible).min(options.len());
    let mut lines = vec![
        popup_title_line(dropdown_title(dropdown), area.width.saturating_sub(2)),
        Line::from(""),
    ];
    if start > 0 {
        lines.push(Line::from(Span::styled(
            "...",
            Style::default().fg(Color::Gray),
        )));
    }
    for (index, option) in options.iter().enumerate().skip(start).take(max_visible) {
        let style = if index == selected {
            selected_style()
        } else if dropdown == Dropdown::ProxyProfile && proxy_profile_dropdown_active(config, index)
        {
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
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
    if end < options.len() {
        lines.push(Line::from(Span::styled(
            "...",
            Style::default().fg(Color::Gray),
        )));
    }
    if is_llm_provider {
        lines.push(Line::from(""));
        let note = llm_provider_note_by_index(selected);
        lines.push(Line::from(Span::styled(
            fit_width(&note, area.width.saturating_sub(4) as usize),
            Style::default().fg(Color::Gray),
        )));
    } else if dropdown == Dropdown::LlmModel {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "Edit LLM Model manually to use a custom model id.",
            Style::default().fg(Color::Gray),
        )));
    }
    frame.render_widget(dialog_panel(lines), area);
}

fn dropdown_window_start(selected: usize, count: usize, max_visible: usize) -> usize {
    if count <= max_visible {
        return 0;
    }
    let half = max_visible / 2;
    let mut start = selected.saturating_sub(half);
    if start + max_visible > count {
        start = count - max_visible;
    }
    start
}

fn dropdown_title(dropdown: Dropdown) -> &'static str {
    match dropdown {
        Dropdown::ProxyProfile => "Profile",
        Dropdown::ProxySubscription => "Subscription",
        Dropdown::Mode => "Mode",
        Dropdown::SubscriptionRefresh => "Refresh",
        Dropdown::CoreSource => "Mihomo Core",
        Dropdown::LlmProvider => "LLM Provider",
        Dropdown::LlmModel => "LLM Model",
    }
}

fn dropdown_options(dropdown: Dropdown, config: &AppConfig) -> Vec<String> {
    match dropdown {
        Dropdown::ProxyProfile => {
            if config.proxy_profiles.is_empty() {
                vec!["No profiles configured".into()]
            } else {
                config
                    .proxy_profiles
                    .iter()
                    .map(|profile| display_name(&profile.name))
                    .collect()
            }
        }
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
        Dropdown::CoreSource => core::CoreSource::ALL
            .iter()
            .map(|source| source.label().to_string())
            .collect(),
        Dropdown::LlmProvider => llm_providers::presets()
            .iter()
            .map(|preset| preset.label.clone())
            .collect(),
        Dropdown::LlmModel => llm_model_dropdown_options(config),
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

fn proxy_profile_dropdown_active(config: &AppConfig, index: usize) -> bool {
    config
        .proxy_profiles
        .get(index)
        .is_some_and(|profile| profile.name == config.active_proxy_profile)
}

fn draw_confirm(frame: &mut Frame, app: &ConfigApp) {
    let Some(action) = app.confirm else {
        return;
    };

    let area = fixed_rect(42, 6, frame.area());
    frame.render_widget(Clear, area);
    let lines = vec![
        popup_title_line(action.title_for(app.language), area.width.saturating_sub(2)),
        Line::from(""),
        Line::from(vec![
            Span::raw("          "),
            Span::styled(
                text(app.language, " No ", " 否 "),
                button_style(!app.confirm_yes),
            ),
            Span::raw("              "),
            Span::styled(
                text(app.language, " Yes ", " 是 "),
                button_style(app.confirm_yes),
            ),
        ]),
    ];
    frame.render_widget(dialog_panel(lines), area);
}

fn draw_alert(frame: &mut Frame, app: &ConfigApp) {
    let Some(alert) = &app.alert else {
        return;
    };

    let width = frame.area().width.saturating_sub(8).clamp(46, 88);
    let message_width = width.saturating_sub(6) as usize;
    let mut message_lines = wrap_popup_message(&alert.message, message_width, 12);
    let height = (message_lines.len() as u16 + 5).clamp(7, 18);
    let area = fixed_rect(width, height, frame.area());
    frame.render_widget(Clear, area);
    let mut lines = vec![
        popup_title_line(&alert.title, area.width.saturating_sub(2)),
        Line::from(""),
    ];
    lines.append(&mut message_lines);
    lines.extend([
        Line::from(""),
        Line::from(Span::styled(" OK ", button_style(true))).alignment(Alignment::Center),
    ]);
    frame.render_widget(dialog_panel(lines), area);
}

fn wrap_popup_message(message: &str, width: usize, max_lines: usize) -> Vec<Line<'static>> {
    let width = width.max(12);
    let mut lines = Vec::new();
    for raw_line in message.lines() {
        if raw_line.is_empty() {
            lines.push(String::new());
            continue;
        }
        let mut current = String::new();
        for ch in raw_line.chars() {
            if current.chars().count() >= width {
                lines.push(current);
                current = String::new();
            }
            current.push(ch);
        }
        lines.push(current);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    if lines.len() > max_lines {
        let tail = lines[lines.len().saturating_sub(max_lines - 1)..].to_vec();
        lines = std::iter::once("...".into()).chain(tail).collect();
    }
    lines
        .into_iter()
        .map(|line| Line::from(Span::raw(format!("  {line}"))))
        .collect()
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

fn draw_assistant_test(frame: &mut Frame, app: &ConfigApp) {
    let Some(task) = &app.assistant_test else {
        return;
    };

    let width = frame.area().width.saturating_sub(8).clamp(52, 88);
    let message_width = width.saturating_sub(6) as usize;
    let area = fixed_rect(width, 13, frame.area());
    let body_limit = area.height.saturating_sub(7).max(1) as usize;
    frame.render_widget(Clear, area);

    let mut lines = vec![
        popup_title_line(
            text(app.language, "Test Assistant", "Test Assistant"),
            area.width.saturating_sub(2),
        ),
        Line::from(""),
    ];
    if let Some(error) = &task.error {
        lines.push(Line::from(Span::styled(
            "Error",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        )));
        let body = limit_popup_lines(
            wrap_prefixed_lines(error, "  ", message_width, Style::default().fg(Color::Red)),
            body_limit,
        );
        lines.extend(body);
    } else if task.response.is_empty() {
        let spinner = runtime_command_spinner(task.started_at.elapsed());
        lines.push(
            Line::from(format!(
                "{spinner} {}",
                text(app.language, "sending hello...", "发送 hello...")
            ))
            .alignment(Alignment::Center),
        );
    } else {
        lines.push(Line::from(Span::styled(
            "Response",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )));
        let body = limit_popup_lines(
            wrap_prefixed_lines(
                &task.response,
                "  ",
                message_width,
                Style::default().fg(Color::White),
            ),
            body_limit,
        );
        lines.extend(body);
    }

    let action = if task.finished {
        text(
            app.language,
            "Enter OK | Esc close",
            "Enter 确定 | Esc 关闭",
        )
    } else {
        text(app.language, "Esc cancel", "Esc 取消")
    };
    lines.extend([
        Line::from(""),
        Line::from(Span::styled(action, Style::default().fg(Color::Gray)))
            .alignment(Alignment::Center),
    ]);
    frame.render_widget(dialog_panel(lines), area);
}

fn limit_popup_lines(mut lines: Vec<Line<'static>>, max_lines: usize) -> Vec<Line<'static>> {
    if lines.len() <= max_lines {
        return lines;
    }
    let tail = lines.split_off(lines.len().saturating_sub(max_lines.saturating_sub(1)));
    std::iter::once(Line::from(Span::styled(
        "  ...",
        Style::default().fg(Color::DarkGray),
    )))
    .chain(tail)
    .collect()
}

fn draw_runtime_command(frame: &mut Frame, app: &ConfigApp) {
    let Some(task) = &app.runtime_command else {
        return;
    };

    let area = fixed_rect(46, 7, frame.area());
    frame.render_widget(Clear, area);
    let elapsed = task.started_at.elapsed().as_secs();
    let spinner = runtime_command_spinner(task.started_at.elapsed());
    let lines = vec![
        popup_title_line(task.command.title(), area.width.saturating_sub(2)),
        Line::from(""),
        Line::from(format!("{spinner} {}", task.command.message())).alignment(Alignment::Center),
        Line::from(format!("Elapsed: {elapsed}s")).alignment(Alignment::Center),
    ];
    frame.render_widget(dialog_panel(lines), area);
}

fn runtime_command_spinner(elapsed: Duration) -> &'static str {
    match (elapsed.as_millis() / 200) % 4 {
        0 => "|",
        1 => "/",
        2 => "-",
        _ => "\\",
    }
}

fn confirm_choice_style(selected: bool) -> Style {
    button_style(selected)
}

fn form_choice_style(selected: bool) -> Style {
    if selected {
        selected_style()
    } else {
        Style::default().fg(Color::White)
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

fn input_box_lines(value: &str, cursor: usize, width: usize, rows: usize) -> Vec<Line<'static>> {
    let rows = rows.max(1);
    let inner_width = width.saturating_sub(4).max(8);
    let wrapped = input_editor_lines(value, cursor, inner_width, rows);
    let mut lines = vec![Line::from(format!("  ┌{}┐", "─".repeat(inner_width)))];
    for line in wrapped {
        let line_width = line_width(&line);
        let padding = inner_width.saturating_sub(line_width);
        let mut spans = vec![Span::raw("  │")];
        spans.extend(line.spans);
        spans.push(Span::raw(" ".repeat(padding)));
        spans.push(Span::raw("│"));
        lines.push(Line::from(spans));
    }
    lines.push(Line::from(format!("  └{}┘", "─".repeat(inner_width))));
    lines
}

fn chat_input_lines(value: &str, width: usize, rows: usize) -> Vec<Line<'static>> {
    let rows = rows.max(1);
    let width = width.max(1);
    let prompt = if width > 1 { "> " } else { ">" };
    let body_width = width.saturating_sub(prompt.chars().count()).max(1);
    let body_lines = input_editor_lines(value, value.len(), body_width, rows);

    body_lines
        .into_iter()
        .enumerate()
        .map(|(index, mut line)| {
            if index == 0 {
                line.spans.insert(
                    0,
                    Span::styled(
                        prompt.to_string(),
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                    ),
                );
                line
            } else {
                line.spans.insert(0, Span::raw("  "));
                line
            }
        })
        .collect()
}

fn input_editor_lines(value: &str, cursor: usize, width: usize, rows: usize) -> Vec<Line<'static>> {
    let rows = rows.max(1);
    let width = width.max(1);
    let cursor = normalize_cursor(value, cursor);
    let segments = wrapped_input_segments(value, width);
    let cursor_line_index = segments
        .iter()
        .position(|segment| segment.contains_cursor(cursor))
        .unwrap_or_else(|| segments.len().saturating_sub(1));
    let start = if cursor_line_index >= rows {
        cursor_line_index + 1 - rows
    } else {
        0
    };
    let mut visible = segments
        .iter()
        .skip(start)
        .take(rows)
        .enumerate()
        .map(|(visible_index, segment)| {
            segment.to_line(
                value,
                cursor,
                width,
                start > 0 && visible_index == 0,
                segment.contains_cursor(cursor),
            )
        })
        .collect::<Vec<_>>();

    while visible.len() < rows {
        let show_cursor = value.is_empty() && cursor == 0 && visible.is_empty();
        visible.push(empty_input_line(show_cursor));
    }
    visible
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct InputSegment {
    start: usize,
    end: usize,
    line_end: usize,
}

impl InputSegment {
    const fn contains_cursor(self, cursor: usize) -> bool {
        if self.start == self.end {
            return cursor == self.start;
        }
        if cursor < self.start || cursor > self.end {
            return false;
        }
        cursor < self.end || self.end == self.line_end
    }

    fn to_line(
        self,
        value: &str,
        cursor: usize,
        width: usize,
        truncated: bool,
        selected: bool,
    ) -> Line<'static> {
        let mut spans = Vec::new();
        let mut remaining_width = width;
        if truncated {
            spans.push(Span::styled("…", Style::default().fg(Color::DarkGray)));
            remaining_width = remaining_width.saturating_sub(1);
        }

        let mut rendered_cursor = false;
        for (offset, ch) in value[self.start..self.end].char_indices() {
            if remaining_width == 0 {
                break;
            }
            let index = self.start + offset;
            if selected && index == cursor {
                spans.push(Span::styled(ch.to_string(), selected_style()));
                rendered_cursor = true;
            } else {
                spans.push(Span::raw(ch.to_string()));
            }
            remaining_width = remaining_width.saturating_sub(1);
        }

        if selected && !rendered_cursor && cursor == self.end {
            if remaining_width > 0 {
                spans.push(cursor_span());
            } else if let Some(last) = spans.last_mut() {
                last.style = selected_style();
            }
        }

        Line::from(spans)
    }
}

fn empty_input_line(show_cursor: bool) -> Line<'static> {
    if show_cursor {
        Line::from(cursor_span())
    } else {
        Line::from("")
    }
}

fn cursor_span() -> Span<'static> {
    Span::styled(" ", selected_style())
}

fn line_width(line: &Line<'_>) -> usize {
    line.spans
        .iter()
        .map(|span| span.content.chars().count())
        .sum()
}

fn wrapped_input_segments(value: &str, width: usize) -> Vec<InputSegment> {
    let width = width.max(1);
    if value.is_empty() {
        return vec![InputSegment {
            start: 0,
            end: 0,
            line_end: 0,
        }];
    }

    let mut segments = Vec::new();
    let mut line_start = 0;
    for line in value.split('\n') {
        let line_end = line_start + line.len();
        push_wrapped_line_segments(&mut segments, line, line_start, line_end, width);
        line_start = line_end.saturating_add(1);
    }

    if segments.is_empty() {
        segments.push(InputSegment {
            start: value.len(),
            end: value.len(),
            line_end: value.len(),
        });
    }
    segments
}

fn push_wrapped_line_segments(
    segments: &mut Vec<InputSegment>,
    content: &str,
    line_start: usize,
    line_end: usize,
    width: usize,
) {
    if content.is_empty() {
        segments.push(InputSegment {
            start: line_start,
            end: line_start,
            line_end,
        });
        return;
    }

    let mut start = line_start;
    let mut count = 0;
    for (offset, _) in content.char_indices() {
        if count == width {
            segments.push(InputSegment {
                start,
                end: line_start + offset,
                line_end,
            });
            start = line_start + offset;
            count = 0;
        }
        count += 1;
    }
    segments.push(InputSegment {
        start,
        end: line_end,
        line_end,
    });
}

fn wrapped_input_lines(value: &str, width: usize, rows: usize) -> Vec<String> {
    let rows = rows.max(1);
    if value.is_empty() {
        return std::iter::repeat_n(String::new(), rows).collect();
    }

    let mut lines = Vec::new();
    for segment in value.split('\n') {
        let mut current = String::new();
        for ch in segment.chars() {
            if current.chars().count() >= width {
                lines.push(current);
                current = String::new();
            }
            current.push(ch);
        }
        lines.push(current);
    }

    if lines.len() > rows {
        lines = lines[lines.len() - rows..].to_vec();
        if let Some(first) = lines.first_mut()
            && width > 1
        {
            let suffix = first
                .chars()
                .take(width.saturating_sub(1))
                .collect::<String>();
            *first = format!("…{suffix}");
        }
    }

    while lines.len() < rows {
        lines.push(String::new());
    }
    lines
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

fn status_bar_line(app: &ConfigApp, width: u16) -> Line<'static> {
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

fn breadcrumb(app: &ConfigApp) -> String {
    app.history
        .iter()
        .map(|location| location.page.title_for(app.language))
        .chain(std::iter::once(app.page.title_for(app.language)))
        .collect::<Vec<_>>()
        .join(" / ")
}

fn config_state(app: &ConfigApp) -> &'static str {
    if app.dirty
        || !app.subscription_form.name.trim().is_empty()
        || !app.subscription_form.url.trim().is_empty()
    {
        "cfg draft"
    } else {
        "cfg saved"
    }
}

fn service_state(app: &ConfigApp) -> &'static str {
    if !app.runtime_checked {
        "svc checking"
    } else if app.runtime.error.is_none() {
        "svc running"
    } else {
        "svc offline"
    }
}

fn runtime_offline_notice(app: &ConfigApp) -> Option<&'static str> {
    (app.runtime_checked && app.runtime.error.is_some()).then_some(
        "Runtime offline: static config is editable; live proxy/rule changes need `clashtui start`.",
    )
}

fn split_column(area: Rect) -> u16 {
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(66),
            Constraint::Length(1),
            Constraint::Percentage(34),
        ])
        .split(area);
    columns[1].x.saturating_sub(area.x)
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

enum ProxyProfileAction {
    Open(usize),
    Add,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum MainProxyKind {
    System,
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
    udp: Option<bool>,
    enabled: bool,
    action: bool,
}

fn subscription_menu_count(config: &AppConfig) -> usize {
    config.subscriptions.len() + 1
}

fn subscription_dropdown_count(config: &AppConfig) -> usize {
    config.subscriptions.len().max(1)
}

const SUBSCRIPTION_DETAIL_COUNT: usize = 8;
const fn subscription_detail_count() -> usize {
    SUBSCRIPTION_DETAIL_COUNT
}

fn subscription_rule_group_count(paths: &Paths, config: &AppConfig, app: &ConfigApp) -> usize {
    subscription_rule_groups_for_view(paths, config, app)
        .len()
        .max(1)
}

fn subscription_proxy_count(paths: &Paths, config: &AppConfig, app: &ConfigApp) -> usize {
    let count = subscription_proxy_names_for_view(paths, config, app).len();
    if count > 0 { count + 1 } else { 1 }
}

fn subscription_rule_count(paths: &Paths, config: &AppConfig, app: &ConfigApp) -> usize {
    subscription_rule_rows(paths, config, app).len().max(1)
}

fn subscription_rule_proxy_count(paths: &Paths, config: &AppConfig, app: &ConfigApp) -> usize {
    selected_rule_group_for_proxy_selection(paths, config, app)
        .map_or(1, |group| group.all.len().max(1))
}

fn cached_subscription_profile<'a>(
    app: &'a ConfigApp,
    subscription: &Subscription,
) -> Option<&'a CachedSubscriptionProfile> {
    app.subscription_profiles.profiles.get(&subscription.name)
}

fn subscription_profile_summary_for_view(
    app: &ConfigApp,
    subscription: &Subscription,
) -> SubscriptionProfileSummary {
    cached_subscription_profile(app, subscription)
        .map(|profile| profile.summary.clone())
        .unwrap_or_else(|| {
            if app.subscription_profiles.loaded && app.subscription_profile_refresh.is_none() {
                SubscriptionProfileSummary::default()
            } else {
                SubscriptionProfileSummary {
                    loading: true,
                    ..SubscriptionProfileSummary::default()
                }
            }
        })
}

fn selected_subscription_action(config: &AppConfig, app: &ConfigApp) -> SubscriptionAction {
    let index = app.selected_subscription;
    if index < config.subscriptions.len() {
        return SubscriptionAction::Open(index);
    }

    SubscriptionAction::Add
}

fn selected_proxy_profile_action(config: &AppConfig, app: &ConfigApp) -> ProxyProfileAction {
    let index = app.selected_profile;
    if index < config.proxy_profiles.len() {
        return ProxyProfileAction::Open(index);
    }

    ProxyProfileAction::Add
}

fn selected_proxy_profile<'a>(config: &'a AppConfig, app: &ConfigApp) -> Option<&'a ProxyProfile> {
    config.proxy_profiles.get(app.selected_profile)
}

fn selected_proxy_profile_mut<'a>(
    config: &'a mut AppConfig,
    app: &ConfigApp,
) -> Option<&'a mut ProxyProfile> {
    config.proxy_profiles.get_mut(app.selected_profile)
}

fn subscription_by_name(config: &AppConfig, name: &str) -> Option<Subscription> {
    config
        .subscriptions
        .iter()
        .find(|subscription| subscription.name == name)
        .cloned()
}

fn selected_profile_delay_target(
    config: &AppConfig,
    app: &ConfigApp,
) -> Option<(Subscription, String)> {
    let profile = selected_proxy_profile(config, app)?;
    let subscription = profile
        .subscription
        .as_deref()
        .and_then(|name| subscription_by_name(config, name))?;
    let proxy = profile
        .proxy
        .clone()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| profile.rule_selections.values().next().cloned())?;
    Some((subscription, proxy))
}

fn selected_proxy_config_delay_target(
    config: &AppConfig,
    app: &ConfigApp,
) -> Option<(Subscription, String)> {
    let subscription =
        proxy_subscription_name(config, app).and_then(|name| subscription_by_name(config, name))?;
    let proxy = selected_global_proxy(config, app)?;
    Some((subscription, proxy))
}

fn selected_proxy_group_delay_target(
    paths: &Paths,
    config: &AppConfig,
    app: &ConfigApp,
) -> Option<(Subscription, String)> {
    if let Some(context) = app.rule_group_selection.as_ref() {
        let subscription = config
            .subscriptions
            .get(context.subscription_index)?
            .clone();
        let group = selected_rule_group_for_proxy_selection(paths, config, app)?;
        let proxy = group.all.get(app.selected_proxy)?.clone();
        return Some((subscription, proxy));
    }

    let subscription =
        proxy_subscription_name(config, app).and_then(|name| subscription_by_name(config, name))?;
    if proxy_groups_page_is_global_proxy_list(config, app) {
        let proxy = route_proxy_names(paths, config, app)
            .get(app.selected_proxy)
            .cloned()?;
        return Some((subscription, proxy));
    }

    match app.proxy_pane {
        ProxyPane::Groups => {
            let group = selected_mihomo_runtime(config, app)
                .groups
                .get(app.selected_group)?;
            Some((subscription, group.now.clone()))
        }
        ProxyPane::Proxies => {
            let group = app.current_group(config)?;
            let proxy = group.all.get(app.selected_proxy)?.clone();
            Some((subscription, proxy))
        }
    }
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
    if proxy_context_is_profile(app)
        && let Some(profile) = selected_proxy_profile(config, app)
    {
        return profile.mode.as_str();
    }
    service_for_main_proxy(config, app)
        .map(|service| service.mode.as_str())
        .unwrap_or(config.runtime_mode.as_str())
}

fn proxy_subscription_name<'a>(config: &'a AppConfig, app: &ConfigApp) -> Option<&'a str> {
    if proxy_context_is_profile(app) {
        return selected_proxy_profile(config, app)
            .and_then(|profile| profile.subscription.as_deref());
    }
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

fn proxy_profile_subscription_index(config: &AppConfig, app: &ConfigApp) -> Option<usize> {
    let name = selected_proxy_profile(config, app)?.subscription.as_ref()?;
    config
        .subscriptions
        .iter()
        .position(|subscription| &subscription.name == name)
}

fn mode_index(mode: &str) -> usize {
    ModeItem::ALL
        .iter()
        .position(|item| item.value().eq_ignore_ascii_case(mode))
        .unwrap_or_default()
}

fn proxy_context_is_profile(app: &ConfigApp) -> bool {
    app.page == Page::ProfileConfig
        || app
            .history
            .last()
            .is_some_and(|location| location.page == Page::ProfileConfig)
}

fn mode_selection_targets_profile(app: &ConfigApp) -> bool {
    proxy_context_is_profile(app)
}

fn uses_subscription_rule_selections(config: &AppConfig) -> bool {
    config.runtime_mode.eq_ignore_ascii_case("rule")
}

fn desired_proxy_selection<'a>(
    config: &'a AppConfig,
    app: &ConfigApp,
    group: &str,
) -> Option<&'a str> {
    if proxy_context_is_profile(app)
        && let Some(profile) = selected_proxy_profile(config, app)
    {
        if profile.mode.eq_ignore_ascii_case("rule")
            && let Some(selection) = profile.rule_selections.get(group)
        {
            return Some(selection.as_str());
        }
        if profile.mode.eq_ignore_ascii_case("global") {
            return profile.proxy.as_deref();
        }
        return None;
    }

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
    if proxy_context_is_profile(app) {
        let selected_index = app.selected_profile;
        if let Some(profile) = selected_proxy_profile_mut(config, app) {
            let profile_name = profile.name.clone();
            if mode.eq_ignore_ascii_case("rule") {
                profile
                    .rule_selections
                    .insert(group.to_string(), proxy.to_string());
                let _ = profile;
                sync_active_profile_if_selected(config, selected_index);
                return format!("{} rule", display_name(&profile_name));
            }

            if mode.eq_ignore_ascii_case("global") {
                profile.proxy = Some(proxy.to_string());
                let _ = profile;
                sync_active_profile_if_selected(config, selected_index);
                return format!("{} global", display_name(&profile_name));
            }

            profile.proxy = Some("DIRECT".into());
            let _ = profile;
            sync_active_profile_if_selected(config, selected_index);
            return format!("{} direct", display_name(&profile_name));
        }
    }

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

fn active_proxy_profile_index(config: &AppConfig) -> Option<usize> {
    config
        .proxy_profiles
        .iter()
        .position(|profile| profile.name == config.active_proxy_profile)
}

fn proxy_profile_menu_count(config: &AppConfig) -> usize {
    config.proxy_profiles.len() + 1
}

fn is_default_proxy_profile(config: &AppConfig, index: usize) -> bool {
    index == 0
        || config
            .proxy_profiles
            .get(index)
            .is_some_and(|profile| profile.name == "default")
}

fn proxy_profile_summary(profile: &ProxyProfile) -> String {
    format!(
        "{} {} proxy={}",
        profile.mode,
        profile.subscription.as_deref().map_or_else(
            || "sub=-".into(),
            |subscription| { format!("sub={}", display_name(subscription)) }
        ),
        profile
            .proxy
            .as_deref()
            .map_or_else(|| "-".into(), display_name)
    )
}

fn main_proxy_count(config: &AppConfig) -> usize {
    1 + config.proxy_ports.services.len() + 1
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
    let profile = config.active_proxy_profile.clone();
    match main_proxy_kind(config, index) {
        MainProxyKind::System => MainProxyRow {
            name: "Global Proxy".into(),
            kind: "Global".into(),
            listener: "MIX".into(),
            port: config.mixed_port.to_string(),
            listen: format!("{}:{}", config.proxy_host, config.mixed_port),
            mode: config.runtime_mode.clone(),
            subscription: profile,
            features: format!(
                "SUB={} SYS={} TUN={} DNS={}",
                subscription,
                on_off_upper(config.system_proxy.enabled),
                on_off_upper(config.tun.enable),
                on_off_upper(config.dns.enable)
            ),
            udp: None,
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
                        if service.mode.eq_ignore_ascii_case("global") {
                            service
                                .proxy
                                .as_deref()
                                .map_or("proxy=-", |proxy| proxy)
                                .to_string()
                        } else if service.mode.eq_ignore_ascii_case("rule") {
                            format!("groups={}", service.rule_selections.len())
                        } else {
                            "DIRECT".into()
                        }
                    })
                    .unwrap_or_else(|| "custom".into()),
                udp: service.map(|service| service.udp),
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
            udp: None,
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
    MainProxyKind::Service
}

fn main_proxy_runtime_key(config: &AppConfig, index: usize) -> String {
    match main_proxy_kind(config, index) {
        MainProxyKind::System => "global".into(),
        MainProxyKind::Service => service_index_for_main_proxy(config, index)
            .map(service_runtime_key)
            .unwrap_or_else(|| "service:unknown".into()),
        MainProxyKind::AddPortProxy => "add".into(),
    }
}

fn service_runtime_key(index: usize) -> String {
    format!("service:{index}")
}

fn service_index_for_main_proxy(config: &AppConfig, index: usize) -> Option<usize> {
    let service_index = index.checked_sub(1)?;
    (service_index < config.proxy_ports.services.len()).then_some(service_index)
}

fn main_proxy_ip_proxy_url(config: &AppConfig, index: usize) -> Option<String> {
    match main_proxy_kind(config, index) {
        MainProxyKind::System => proxy_url("http", &config.proxy_host, config.mixed_port),
        MainProxyKind::Service => {
            let service = service_index_for_main_proxy(config, index)
                .and_then(|index| config.proxy_ports.services.get(index))?;
            if !service.enabled || service.port == 0 {
                return None;
            }
            let scheme = match service.kind.trim().to_ascii_lowercase().as_str() {
                "socks" | "socks5" => "socks5h",
                _ => "http",
            };
            proxy_url(scheme, &service.listen, service.port)
        }
        MainProxyKind::AddPortProxy => None,
    }
}

fn proxy_url(scheme: &str, host: &str, port: u16) -> Option<String> {
    if port == 0 {
        return None;
    }
    let host = local_proxy_host(host);
    Some(format!("{scheme}://{host}:{port}"))
}

fn local_proxy_host(host: &str) -> &str {
    match host.trim() {
        "" | "*" | "0.0.0.0" | "::" | "[::]" => "127.0.0.1",
        host => host,
    }
}

fn main_proxy_index_for_service(config: &AppConfig, service_index: usize) -> Option<usize> {
    if service_index >= config.proxy_ports.services.len() {
        return None;
    }
    Some(1 + service_index)
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
            "PROFILE={} {} SYS={} TUN={}",
            row.subscription,
            mode,
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

    app.current_group(config)
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
    if proxy_context_is_profile(app) {
        return selected_proxy_profile(config, app).and_then(|profile| profile.proxy.clone());
    }
    if let Some(service) = service_for_main_proxy(config, app) {
        return service.proxy.clone();
    }
    config
        .proxy_selections
        .get("GLOBAL")
        .or_else(|| config.proxy_selections.values().next())
        .cloned()
}

fn route_proxy_names(_paths: &Paths, config: &AppConfig, app: &ConfigApp) -> Vec<String> {
    if let Some(subscription) = proxy_subscription_name(config, app).and_then(|name| {
        config
            .subscriptions
            .iter()
            .find(|subscription| subscription.name == name)
    }) {
        let proxies = cached_subscription_profile(app, subscription)
            .map(|profile| profile.proxies.clone())
            .unwrap_or_default();
        if !proxies.is_empty() {
            return proxies;
        }
    }
    runtime_proxy_names(selected_mihomo_runtime(config, app))
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
        ProxyConfigField::ServiceStatus => {
            process_status_summary(&selected_mihomo_runtime(config, app).process)
        }
        ProxyConfigField::TrafficStatus => {
            traffic_detail_summary(selected_mihomo_runtime(config, app).traffic.as_ref())
        }
        ProxyConfigField::Connections => connection_summary(selected_mihomo_runtime(config, app)),
        ProxyConfigField::Enabled => {
            on_off(main_proxy_row(config, app.selected_main).enabled).into()
        }
        ProxyConfigField::Profile => config.active_proxy_profile.clone(),
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
        ProxyConfigField::Delete => "confirm".into(),
        ProxyConfigField::OsProxy => on_off(config.system_proxy.enabled).into(),
        ProxyConfigField::Pac => "off".into(),
        ProxyConfigField::Tun => on_off(config.tun.enable).into(),
        ProxyConfigField::Logs => "open".into(),
    }
}

fn proxy_profile_field_value(
    config: &AppConfig,
    app: &ConfigApp,
    field: ProfileConfigField,
) -> String {
    let Some(profile) = selected_proxy_profile(config, app) else {
        return "-".into();
    };
    match field {
        ProfileConfigField::Name => profile.name.clone(),
        ProfileConfigField::Active => {
            if profile.name == config.active_proxy_profile {
                "active".into()
            } else {
                "inactive".into()
            }
        }
        ProfileConfigField::Subscription => profile
            .subscription
            .as_deref()
            .map_or_else(|| "-".into(), display_name),
        ProfileConfigField::Mode => profile.mode.clone(),
        ProfileConfigField::ProxyGroups => proxy_server_value(config, app),
        ProfileConfigField::Activate => {
            if profile.name == config.active_proxy_profile {
                "active".into()
            } else {
                "apply".into()
            }
        }
        ProfileConfigField::Delete => {
            if is_default_proxy_profile(config, app.selected_profile) {
                "locked".into()
            } else {
                "confirm".into()
            }
        }
    }
}

const fn proxy_profile_field_help(field: ProfileConfigField) -> &'static str {
    match field {
        ProfileConfigField::Name => "Edit this reusable profile name.",
        ProfileConfigField::Active => "Shows whether Global Proxy currently uses this profile.",
        ProfileConfigField::Subscription => "Choose the subscription used by this profile.",
        ProfileConfigField::Mode => "Choose rule, global, or direct behavior for this profile.",
        ProfileConfigField::ProxyGroups => "Choose the group and concrete server for this profile.",
        ProfileConfigField::Activate => "Apply this profile to Global Proxy.",
        ProfileConfigField::Delete => "Delete this profile after confirmation.",
    }
}

fn proxy_config_field_help(
    _config: &AppConfig,
    app: &ConfigApp,
    field: ProxyConfigField,
) -> &'static str {
    match field {
        ProxyConfigField::ServiceStatus => {
            "Read-only runtime process status for this Mihomo instance."
        }
        ProxyConfigField::TrafficStatus => {
            "Read-only upload/download speeds and cumulative traffic."
        }
        ProxyConfigField::Connections => "Open a compact read-only list of active connections.",
        ProxyConfigField::Enabled if app.selected_main == 0 => {
            "Main mihomo proxy is managed by runtime; use Sys Proxy for OS proxy settings."
        }
        ProxyConfigField::Enabled => "Enable or disable the selected port proxy.",
        ProxyConfigField::Profile => "Choose and activate a saved proxy profile.",
        ProxyConfigField::Subscription => "Choose the source from an inline dropdown.",
        ProxyConfigField::Mode => "Choose rule, global, or direct behavior.",
        ProxyConfigField::ProxyGroups => {
            "Choose the group and concrete server for the current mode."
        }
        ProxyConfigField::LocalPort => "Edit the local listener port for this proxy.",
        ProxyConfigField::Delete => "Delete this port proxy after confirmation.",
        ProxyConfigField::OsProxy => "Point the operating system proxy settings to the mixed port.",
        ProxyConfigField::Pac => "PAC support is planned; it will live on Global Proxy.",
        ProxyConfigField::Tun => {
            "TUN is transparent system traffic and belongs only to Global Proxy."
        }
        ProxyConfigField::Logs => "Open the selected Mihomo runtime log tail.",
    }
}

fn runtime_item_value(
    paths: &Paths,
    config: &AppConfig,
    app: &ConfigApp,
    item: RuntimeItem,
) -> String {
    match item {
        RuntimeItem::Service => match service::status() {
            Ok(status) if status.reachable => "ready".into(),
            Ok(status) if status.installed => "installed/offline".into(),
            Ok(_) => "not installed".into(),
            Err(_) => "unavailable".into(),
        },
        RuntimeItem::Autostart => {
            let status = autostart::status(config);
            if status.configured && status.installed {
                "on".into()
            } else if status.configured {
                "on/pending".into()
            } else if status.installed {
                "off/installed".into()
            } else {
                "off".into()
            }
        }
        RuntimeItem::Logs => "open planned".into(),
        RuntimeItem::CorePath => core_source_value(config),
        RuntimeItem::CoreUpdate => config.mihomo.update.clone(),
        RuntimeItem::Controller => config.controller.url.clone(),
        RuntimeItem::Refresh => app.runtime.error.as_deref().unwrap_or("online").to_string(),
        RuntimeItem::LlmSection => String::new(),
        RuntimeItem::LlmProvider => llm_provider_label(&config.llm.provider),
        RuntimeItem::LlmBaseUrl => config.llm.base_url.clone(),
        RuntimeItem::LlmModel => {
            if config.llm.model.trim().is_empty() {
                "missing".into()
            } else {
                config.llm.model.clone()
            }
        }
        RuntimeItem::LlmApiKey => agent::api_key_status(paths, config),
        RuntimeItem::LlmProvidersUpdate => "manual".into(),
        RuntimeItem::TestAssistant => "run".into(),
    }
}

fn core_source_value(config: &AppConfig) -> String {
    let source = core::selected_core_source(config);
    if source == core::CoreSource::Custom {
        let path = config.core_path.as_deref().unwrap_or("-");
        return format!("{} {}", source.label(), path);
    }
    source.label().into()
}

fn core_source_index(source: core::CoreSource) -> usize {
    core::CoreSource::ALL
        .iter()
        .position(|candidate| *candidate == source)
        .unwrap_or_default()
}

fn llm_provider_index(provider: &str) -> usize {
    llm_providers::presets()
        .iter()
        .position(|preset| preset.id == provider)
        .unwrap_or_default()
}

fn llm_provider_label(provider: &str) -> String {
    llm_providers::presets()
        .iter()
        .find(|preset| preset.id == provider)
        .map(|preset| preset.label.clone())
        .unwrap_or_else(|| "OpenAI Compatible".into())
}

fn llm_provider_note(provider: &str) -> String {
    llm_providers::presets()
        .iter()
        .find(|preset| preset.id == provider)
        .map(|preset| preset.note.clone())
        .unwrap_or_else(|| "Custom compatible endpoint.".into())
}

fn llm_provider_note_by_index(index: usize) -> String {
    llm_providers::presets()
        .get(index)
        .map(|preset| preset.note.clone())
        .unwrap_or_else(|| "Custom compatible endpoint.".into())
}

fn llm_model_options(config: &AppConfig) -> Vec<String> {
    llm_providers::model_options(&config.llm.provider)
}

fn llm_model_dropdown_options(config: &AppConfig) -> Vec<String> {
    let mut options = llm_model_options(config);
    options.push("Custom...".into());
    options
}

fn llm_model_index(config: &AppConfig) -> usize {
    let options = llm_model_options(config);
    options
        .iter()
        .position(|model| model == &config.llm.model)
        .unwrap_or(options.len())
}

fn dns_item_value(config: &AppConfig, item: DnsItem) -> String {
    match item {
        DnsItem::Enabled => on_off(config.dns.enable).into(),
        DnsItem::Listen => config.dns.listen.clone(),
        DnsItem::LanDomains => compact_list(&config.dns.lan_domains),
        DnsItem::LanNameserver => compact_list(&config.dns.lan_nameserver),
        DnsItem::NameserverPolicy => compact_nameserver_policy(&config.dns.nameserver_policy),
        DnsItem::DirectNameserver => compact_list(&config.dns.direct_nameserver),
        DnsItem::DirectFollowPolicy => on_off(config.dns.direct_nameserver_follow_policy).into(),
        DnsItem::Nameserver => compact_list(&config.dns.nameserver),
        DnsItem::Fallback => compact_list(&config.dns.fallback),
        DnsItem::FakeIpFilter => compact_list(&config.dns.fake_ip_filter),
    }
}

const fn dns_item_help(item: DnsItem, language: Language) -> &'static str {
    if language.is_zh_cn() {
        return match item {
            DnsItem::Enabled => "启用或关闭 mihomo 内置 DNS。",
            DnsItem::Listen => "mihomo DNS 本地监听地址。默认避开 Clash Verge 的 1053。",
            DnsItem::LanDomains => "应使用 LAN DNS 且跳过 fake IP 的 domain patterns。",
            DnsItem::LanNameserver => "LAN domains 使用的 DNS servers。可用 system 或 router DNS。",
            DnsItem::NameserverPolicy => {
                "按 domain 指定 DNS policy，例如 +.taobao.net=30.30.30.30。"
            }
            DnsItem::DirectNameserver => "DIRECT 出站流量使用的 DNS servers。",
            DnsItem::DirectFollowPolicy => "开启后，DIRECT DNS 也会遵循 nameserver-policy。",
            DnsItem::Nameserver => "普通域名解析使用的默认 DNS servers。",
            DnsItem::Fallback => "污染或海外结果的备用 DNS servers。",
            DnsItem::FakeIpFilter => "应返回真实 IP 而不是 fake IP 的 domain patterns。",
        };
    }
    match item {
        DnsItem::Enabled => "Enable or disable mihomo built-in DNS.",
        DnsItem::Listen => "Local mihomo DNS listen address. Default avoids Clash Verge 1053.",
        DnsItem::LanDomains => "Domain patterns that should resolve with LAN DNS and skip fake IP.",
        DnsItem::LanNameserver => {
            "DNS servers for LAN domains. Use system or router DNS addresses."
        }
        DnsItem::NameserverPolicy => {
            "Domain-specific DNS policy entries, for example +.taobao.net=30.30.30.30."
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
    let usage = subscription_usage_value(subscription);
    let state = if subscription.last_error.is_some() {
        "error"
    } else {
        "ok"
    };
    format!("{usage} {state}")
}

fn subscription_usage_value(subscription: &Subscription) -> String {
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
        .map(format_unix_date)
        .unwrap_or_else(|| "-".into());
    format!("used {used}/{total} exp {expire}")
}

fn subscription_state_label(subscription: &Subscription) -> &'static str {
    if subscription.last_error.is_some() {
        "ERR"
    } else {
        "OK"
    }
}

fn subscription_proxy_count_summary(app: &ConfigApp, subscription: &Subscription) -> String {
    cached_subscription_profile(app, subscription)
        .map(|profile| profile.summary.proxies.to_string())
        .unwrap_or_else(|| {
            if app.subscription_profile_refresh.is_some() {
                "...".into()
            } else {
                "-".into()
            }
        })
}

fn subscription_list_help(subscription: &Subscription) -> String {
    let error = subscription
        .last_error
        .as_deref()
        .map(|error| format!(" | last error: {error}"))
        .unwrap_or_default();
    format!(
        "Enter opens details. refresh={} updated={}{}",
        subscription_refresh_label(subscription.refresh),
        format_optional_timestamp(subscription.updated_at.as_deref(), "-"),
        error
    )
}

fn format_unix_timestamp(value: u64) -> String {
    format_unix_timestamp_with_offset(
        value,
        UtcOffset::current_local_offset().unwrap_or(UtcOffset::UTC),
    )
}

fn format_unix_date(value: u64) -> String {
    format_unix_date_with_offset(
        value,
        UtcOffset::current_local_offset().unwrap_or(UtcOffset::UTC),
    )
}

fn format_unix_timestamp_with_offset(value: u64, offset: UtcOffset) -> String {
    let Ok(timestamp) = i64::try_from(value) else {
        return value.to_string();
    };
    let Ok(datetime) = OffsetDateTime::from_unix_timestamp(timestamp) else {
        return value.to_string();
    };
    let format = format_description!("[year]/[month]/[day] [hour]:[minute]:[second]");
    datetime
        .to_offset(offset)
        .format(format)
        .unwrap_or_else(|_| value.to_string())
}

fn format_unix_date_with_offset(value: u64, offset: UtcOffset) -> String {
    let Ok(timestamp) = i64::try_from(value) else {
        return value.to_string();
    };
    let Ok(datetime) = OffsetDateTime::from_unix_timestamp(timestamp) else {
        return value.to_string();
    };
    let format = format_description!("[year]/[month]/[day]");
    datetime
        .to_offset(offset)
        .format(format)
        .unwrap_or_else(|_| value.to_string())
}

fn format_optional_timestamp(value: Option<&str>, empty: &str) -> String {
    let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else {
        return empty.into();
    };
    format_timestamp_text(value)
}

fn format_timestamp_text(value: &str) -> String {
    value
        .parse::<u64>()
        .map(format_unix_timestamp)
        .unwrap_or_else(|_| value.to_string())
}

fn runtime_proxy_names(runtime: &RuntimeState) -> Vec<String> {
    let mut proxies = Vec::new();
    for group in &runtime.groups {
        for proxy in &group.all {
            if !proxies.iter().any(|existing| existing == proxy) {
                proxies.push(proxy.clone());
            }
        }
    }
    proxies
}

fn subscription_proxy_names_for_view(
    _paths: &Paths,
    config: &AppConfig,
    app: &ConfigApp,
) -> Vec<String> {
    let Some(subscription) = selected_subscription(config, app) else {
        return Vec::new();
    };
    let proxies = cached_subscription_profile(app, subscription)
        .map(|profile| profile.proxies.clone())
        .unwrap_or_default();
    if !proxies.is_empty() {
        return proxies;
    }
    if is_selected_subscription_active(config, subscription) {
        return runtime_proxy_names(&app.runtime);
    }
    Vec::new()
}

fn subscription_rule_groups_for_view(
    _paths: &Paths,
    config: &AppConfig,
    app: &ConfigApp,
) -> Vec<ProxyGroup> {
    let Some(subscription) = selected_subscription(config, app) else {
        return Vec::new();
    };

    let mut groups = cached_subscription_profile(app, subscription)
        .map(|profile| profile.groups.clone())
        .unwrap_or_default();
    if groups.is_empty() && is_selected_subscription_active(config, subscription) {
        groups.clone_from(&app.runtime.groups);
    }

    for group in &mut groups {
        if let Some(saved) = subscription.rule_selections.get(&group.name) {
            group.now.clone_from(saved);
        } else if group.now.is_empty() {
            group.now = group.all.first().cloned().unwrap_or_default();
        }
    }

    for (group_name, proxy) in &subscription.rule_selections {
        if groups.iter().any(|group| group.name == *group_name) {
            continue;
        }
        groups.push(ProxyGroup {
            name: group_name.clone(),
            kind: "saved".into(),
            now: proxy.clone(),
            all: Vec::new(),
        });
    }

    groups
}

fn selected_rule_group_for_proxy_selection(
    paths: &Paths,
    config: &AppConfig,
    app: &ConfigApp,
) -> Option<ProxyGroup> {
    let context = app.rule_group_selection.as_ref()?;
    subscription_rule_groups_for_view(paths, config, app)
        .into_iter()
        .find(|group| group.name == context.group_name)
}

fn subscription_proxy_delay_key(subscription: &Subscription, proxy: &str) -> String {
    subscription_proxy_delay_key_for_name(&subscription.name, proxy)
}

fn subscription_proxy_delay_key_for_name(subscription_name: &str, proxy: &str) -> String {
    format!("{subscription_name}::{proxy}")
}

fn fetch_subscription_profile_cache(paths: &Paths, config: &AppConfig) -> SubscriptionProfileCache {
    let profiles = config
        .subscriptions
        .iter()
        .map(|subscription| {
            (
                subscription.name.clone(),
                fetch_cached_subscription_profile(paths, subscription),
            )
        })
        .collect();

    SubscriptionProfileCache {
        profiles,
        loaded: true,
    }
}

fn fetch_cached_subscription_profile(
    paths: &Paths,
    subscription: &Subscription,
) -> CachedSubscriptionProfile {
    let profile = subscription::profile_path(paths, subscription);
    let content = match std::fs::read_to_string(&profile) {
        Ok(content) => content,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return CachedSubscriptionProfile::default();
        }
        Err(err) => {
            return CachedSubscriptionProfile {
                summary: SubscriptionProfileSummary {
                    exists: true,
                    error: Some(err.to_string()),
                    ..SubscriptionProfileSummary::default()
                },
                ..CachedSubscriptionProfile::default()
            };
        }
    };
    cached_subscription_profile_from_str(&content)
}

fn cached_subscription_profile_from_str(content: &str) -> CachedSubscriptionProfile {
    let value: serde_yaml_ng::Value = match serde_yaml_ng::from_str(content) {
        Ok(value) => value,
        Err(err) => {
            return CachedSubscriptionProfile {
                summary: SubscriptionProfileSummary {
                    exists: true,
                    error: Some(err.to_string()),
                    ..SubscriptionProfileSummary::default()
                },
                ..CachedSubscriptionProfile::default()
            };
        }
    };

    CachedSubscriptionProfile {
        summary: subscription_profile_summary_from_value(&value),
        proxies: subscription_profile_proxy_names_from_value(&value),
        groups: subscription_profile_proxy_groups_from_value(&value),
        rule_rows: subscription_profile_rule_rows_from_value(&value),
    }
}

fn subscription_profile_proxy_names_from_str(content: &str) -> Vec<String> {
    let Ok(value) = serde_yaml_ng::from_str::<serde_yaml_ng::Value>(content) else {
        return Vec::new();
    };
    subscription_profile_proxy_names_from_value(&value)
}

fn subscription_profile_proxy_names_from_value(value: &serde_yaml_ng::Value) -> Vec<String> {
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

fn subscription_profile_proxy_groups_from_str(content: &str) -> Vec<ProxyGroup> {
    let Ok(value) = serde_yaml_ng::from_str::<serde_yaml_ng::Value>(content) else {
        return Vec::new();
    };
    subscription_profile_proxy_groups_from_value(&value)
}

fn subscription_profile_proxy_groups_from_value(value: &serde_yaml_ng::Value) -> Vec<ProxyGroup> {
    let Some(groups) = value
        .as_mapping()
        .and_then(|mapping| mapping.get("proxy-groups"))
        .and_then(serde_yaml_ng::Value::as_sequence)
    else {
        return Vec::new();
    };

    groups
        .iter()
        .filter_map(|group| {
            let mapping = group.as_mapping()?;
            let name = mapping.get("name")?.as_str()?.to_string();
            let kind = mapping
                .get("type")
                .and_then(serde_yaml_ng::Value::as_str)
                .unwrap_or("select")
                .to_string();
            let all = mapping
                .get("proxies")
                .and_then(serde_yaml_ng::Value::as_sequence)
                .map(|proxies| {
                    proxies
                        .iter()
                        .filter_map(serde_yaml_ng::Value::as_str)
                        .map(str::to_string)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            let now = all.first().cloned().unwrap_or_default();
            Some(ProxyGroup {
                name,
                kind,
                now,
                all,
            })
        })
        .collect()
}

fn subscription_profile_rule_rows_from_str(content: &str) -> Vec<SettingRow> {
    let Ok(value) = serde_yaml_ng::from_str::<serde_yaml_ng::Value>(content) else {
        return Vec::new();
    };
    subscription_profile_rule_rows_from_value(&value)
}

fn subscription_profile_rule_rows_from_value(value: &serde_yaml_ng::Value) -> Vec<SettingRow> {
    let Some(mapping) = value.as_mapping() else {
        return Vec::new();
    };

    let mut rows = Vec::new();
    if let Some(providers) = mapping
        .get("rule-providers")
        .and_then(serde_yaml_ng::Value::as_mapping)
    {
        for (name, provider) in providers {
            let name = yaml_value_inline(name);
            let provider_type = provider
                .as_mapping()
                .and_then(|mapping| mapping.get("type"))
                .map(yaml_value_inline)
                .unwrap_or_else(|| "provider".into());
            rows.push(SettingRow {
                label: format!("Provider {}", display_name(&name)),
                value: provider_type,
                help: yaml_value_inline(provider),
                kind: RowKind::Info,
            });
        }
    }

    if let Some(rules) = mapping
        .get("rules")
        .and_then(serde_yaml_ng::Value::as_sequence)
    {
        rows.extend(rules.iter().enumerate().map(|(index, rule)| {
            let rule = yaml_value_inline(rule);
            SettingRow {
                label: format!("Rule {}", index + 1),
                value: rule.clone(),
                help: rule,
                kind: RowKind::Info,
            }
        }));
    }
    rows
}

fn yaml_value_inline(value: &serde_yaml_ng::Value) -> String {
    if let Some(value) = value.as_str() {
        return value.to_string();
    }
    match serde_yaml_ng::to_string(value) {
        Ok(value) => value
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && *line != "---")
            .collect::<Vec<_>>()
            .join(" "),
        Err(_) => "-".into(),
    }
}

fn empty_rows(label: &str, help: &str) -> Vec<SettingRow> {
    vec![SettingRow {
        label: label.into(),
        value: "-".into(),
        help: help.into(),
        kind: RowKind::Info,
    }]
}

#[derive(Debug, Clone, Default)]
struct SubscriptionProfileSummary {
    loading: bool,
    exists: bool,
    proxies: usize,
    proxy_groups: usize,
    rules: usize,
    rule_providers: usize,
    error: Option<String>,
}

impl SubscriptionProfileSummary {
    fn short_counts(&self) -> String {
        if self.loading {
            return "profile loading".into();
        }
        if !self.exists {
            return "profile missing".into();
        }
        if self.error.is_some() {
            return "profile error".into();
        }
        format!("groups={} rules={}", self.proxy_groups, self.rules)
    }

    fn rules_value(&self) -> String {
        if self.loading {
            return "loading".into();
        }
        if !self.exists {
            return "missing".into();
        }
        if self.error.is_some() {
            return "parse error".into();
        }
        format!("{} rules / {} providers", self.rules, self.rule_providers)
    }
}

fn subscription_profile_summary_from_value(
    value: &serde_yaml_ng::Value,
) -> SubscriptionProfileSummary {
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
        ..SubscriptionProfileSummary::default()
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

fn main_proxy_runtime<'a>(
    config: &AppConfig,
    app: &'a ConfigApp,
    index: usize,
) -> &'a RuntimeState {
    let key = main_proxy_runtime_key(config, index);
    app.proxy_runtimes.get(&key).unwrap_or(&app.runtime)
}

fn selected_mihomo_runtime<'a>(config: &AppConfig, app: &'a ConfigApp) -> &'a RuntimeState {
    main_proxy_runtime(config, app, app.selected_main)
}

fn proxy_runtime_line(config: &AppConfig, app: &ConfigApp, index: usize) -> String {
    let runtime = main_proxy_runtime(config, app, index);
    format!(
        "{} {} {}",
        traffic_speed_summary(runtime.traffic.as_ref()),
        traffic_total_summary(runtime.traffic.as_ref()),
        ip_info_summary(runtime)
    )
}

fn traffic_speed_summary(traffic: Option<&TrafficState>) -> String {
    let Some(traffic) = traffic else {
        return "↑- ↓-".into();
    };
    let upload = traffic
        .upload_speed
        .map(format_bytes_short)
        .unwrap_or_else(|| "-".into());
    let download = traffic
        .download_speed
        .map(format_bytes_short)
        .unwrap_or_else(|| "-".into());
    format!("↑{upload}/s ↓{download}/s")
}

fn traffic_total_summary(traffic: Option<&TrafficState>) -> String {
    let Some(traffic) = traffic else {
        return "-".into();
    };
    if traffic.upload_total.is_none() && traffic.download_total.is_none() {
        return "-".into();
    }
    format_bytes_one_decimal(
        traffic
            .upload_total
            .unwrap_or_default()
            .saturating_add(traffic.download_total.unwrap_or_default()),
    )
}

fn connection_summary(runtime: &RuntimeState) -> String {
    runtime
        .connections
        .as_ref()
        .map_or_else(|| "-".into(), |connections| connections.active.to_string())
}

fn ip_info_summary(runtime: &RuntimeState) -> String {
    if let Some(info) = &runtime.ip_info {
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
    } else if runtime.ip_info_error.is_some() {
        "unavailable".into()
    } else {
        "pending".into()
    }
}

fn traffic_detail_summary(traffic: Option<&TrafficState>) -> String {
    let Some(traffic) = traffic else {
        return "↑- ↓- total -".into();
    };
    let upload_speed = traffic
        .upload_speed
        .map(format_bytes_short)
        .unwrap_or_else(|| "-".into());
    let download_speed = traffic
        .download_speed
        .map(format_bytes_short)
        .unwrap_or_else(|| "-".into());
    let upload_total = traffic
        .upload_total
        .map(format_bytes_one_decimal)
        .unwrap_or_else(|| "-".into());
    let download_total = traffic
        .download_total
        .map(format_bytes_one_decimal)
        .unwrap_or_else(|| "-".into());
    format!("↑{upload_speed}/s ↓{download_speed}/s  ↑{upload_total} ↓{download_total}")
}

fn process_status_summary(process: &ProcessState) -> String {
    let Some(pid) = process.pid else {
        return "not running".into();
    };
    if !process.running {
        return format!("pid {pid} not running");
    }
    let cpu = process
        .cpu_percent
        .map(|value| format!("{value:.1}%"))
        .unwrap_or_else(|| "-".into());
    let mem = process
        .rss_bytes
        .map(format_bytes_short)
        .or_else(|| process.mem_percent.map(|value| format!("{value:.1}%")))
        .unwrap_or_else(|| "-".into());
    format!("pid {pid}  cpu {cpu}  mem {mem}")
}

fn proxy_connection_rows(config: &AppConfig, app: &ConfigApp) -> Vec<CompactRow> {
    let runtime = selected_mihomo_runtime(config, app);
    let Some(connections) = &runtime.connections else {
        return vec![CompactRow {
            primary: "  1  no connection data".into(),
            secondary: runtime.error.clone(),
        }];
    };
    if connections.items.is_empty() {
        return vec![CompactRow {
            primary: "  1  no active connections".into(),
            secondary: None,
        }];
    }
    connections
        .items
        .iter()
        .enumerate()
        .map(|(index, connection)| connection_compact_row(index, connection))
        .collect()
}

fn connection_compact_row(index: usize, connection: &ConnectionInfo) -> CompactRow {
    let transfer = format!(
        "↑{} ↓{}",
        format_bytes_short(connection.upload),
        format_bytes_short(connection.download)
    );
    let primary = format!(
        "{:>3}  {:<4} {:<10} {:>14}  {} -> {}",
        index + 1,
        connection.network,
        connection.inbound,
        transfer,
        connection.source,
        connection.destination
    );
    let chains = if connection.chains.is_empty() {
        "-".into()
    } else {
        connection.chains.join(" > ")
    };
    let mut secondary = format!(
        "     {}  rule={}  chain={}  start={}",
        connection.host,
        compact_rule_label(connection),
        chains,
        connection.start
    );
    if !connection.process.is_empty() {
        secondary.push_str(&format!("  proc={}", connection.process));
    }
    CompactRow {
        primary,
        secondary: Some(secondary),
    }
}

fn compact_rule_label(connection: &ConnectionInfo) -> String {
    if connection.rule_payload.is_empty() {
        connection.rule.clone()
    } else {
        format!("{}:{}", connection.rule, connection.rule_payload)
    }
}

fn proxy_log_rows(_paths: &Paths, config: &AppConfig, app: &ConfigApp) -> Vec<CompactRow> {
    let key = main_proxy_runtime_key(config, app.selected_main);
    if let Some(rows) = app.proxy_log_cache.get(&key) {
        return rows.clone();
    }
    if app
        .proxy_log_refresh
        .as_ref()
        .is_some_and(|task| task.key == key)
    {
        return vec![CompactRow {
            primary: "  1  loading log tail".into(),
            secondary: None,
        }];
    }
    vec![CompactRow {
        primary: "  1  log cache is empty".into(),
        secondary: None,
    }]
}

fn fetch_proxy_log_rows(
    paths: &Paths,
    config: &AppConfig,
    selected_main: usize,
) -> Vec<CompactRow> {
    let Some(instance) = main_proxy_instance(paths, config, selected_main) else {
        return vec![CompactRow {
            primary: "  1  no runtime log".into(),
            secondary: None,
        }];
    };
    let lines = read_log_tail(&instance.log_file, 120);
    if lines.is_empty() {
        return vec![CompactRow {
            primary: "  1  log is empty".into(),
            secondary: Some(instance.log_file.display().to_string()),
        }];
    }
    lines
        .into_iter()
        .enumerate()
        .map(|(index, line)| CompactRow {
            primary: format!("{:>3}  {}", index + 1, line),
            secondary: None,
        })
        .collect()
}

fn subscription_rule_compact_rows(
    paths: &Paths,
    config: &AppConfig,
    app: &ConfigApp,
) -> Vec<CompactRow> {
    subscription_rule_rows(paths, config, app)
        .into_iter()
        .enumerate()
        .map(|(index, row)| CompactRow {
            primary: format!("{:>3}  {:<22} {}", index + 1, row.label, row.value),
            secondary: (!row.help.trim().is_empty()).then_some(format!("     {}", row.help)),
        })
        .collect()
}

fn compact_setting_rows(rows: Vec<CompactRow>) -> Vec<SettingRow> {
    rows.into_iter()
        .map(|row| SettingRow {
            label: row.primary,
            value: row.secondary.unwrap_or_default(),
            help: "Read-only compact list row.".into(),
            kind: RowKind::Info,
        })
        .collect()
}

fn traffic_from_values(
    connections: Option<&serde_json::Value>,
    traffic: Option<&serde_json::Value>,
) -> Option<TrafficState> {
    let (upload_total, download_total) = traffic
        .and_then(traffic_totals_from_traffic)
        .or_else(|| connections.and_then(traffic_totals_from_connections))
        .unwrap_or((None, None));
    let (upload_speed, download_speed) = traffic
        .and_then(traffic_speeds_from_value)
        .unwrap_or((None, None));
    if upload_total.is_none()
        && download_total.is_none()
        && upload_speed.is_none()
        && download_speed.is_none()
    {
        return None;
    }
    Some(TrafficState {
        upload_total,
        download_total,
        upload_speed,
        download_speed,
        sampled_at: Some(Instant::now()),
    })
}

fn traffic_from_connection_state(connections: &ConnectionState) -> TrafficState {
    TrafficState {
        upload_total: Some(
            connections
                .items
                .iter()
                .map(|connection| connection.upload)
                .sum(),
        ),
        download_total: Some(
            connections
                .items
                .iter()
                .map(|connection| connection.download)
                .sum(),
        ),
        sampled_at: Some(Instant::now()),
        ..TrafficState::default()
    }
}

fn estimate_runtime_traffic_speeds(previous: Option<&RuntimeState>, runtime: &mut RuntimeState) {
    let previous = previous.and_then(|runtime| runtime.traffic.as_ref());
    let Some(current) = runtime.traffic.as_mut() else {
        return;
    };
    estimate_traffic_speeds(previous, current);
}

fn estimate_traffic_speeds(previous: Option<&TrafficState>, current: &mut TrafficState) {
    let Some(previous) = previous else {
        return;
    };
    let (Some(previous_at), Some(current_at)) = (previous.sampled_at, current.sampled_at) else {
        return;
    };
    let Some(elapsed) = current_at.checked_duration_since(previous_at) else {
        return;
    };
    let elapsed = elapsed.as_secs_f64();
    if elapsed <= 0.0 {
        return;
    }

    if current.upload_speed.is_none() {
        current.upload_speed = estimate_speed(previous.upload_total, current.upload_total, elapsed);
    }
    if current.download_speed.is_none() {
        current.download_speed =
            estimate_speed(previous.download_total, current.download_total, elapsed);
    }
}

fn estimate_speed(previous: Option<u64>, current: Option<u64>, elapsed_secs: f64) -> Option<u64> {
    let previous = previous?;
    let current = current?;
    if current < previous {
        return None;
    }
    Some(((current - previous) as f64 / elapsed_secs).round() as u64)
}

fn connection_state_from_value(value: &serde_json::Value) -> Option<ConnectionState> {
    let connections = value
        .get("connections")
        .and_then(serde_json::Value::as_array)
        .map(|connections| {
            connections
                .iter()
                .map(connection_info_from_value)
                .collect::<Vec<_>>()
        })?;
    Some(ConnectionState {
        active: connections.len(),
        items: connections,
    })
}

fn connection_info_from_value(value: &serde_json::Value) -> ConnectionInfo {
    let metadata = value.get("metadata").unwrap_or(&serde_json::Value::Null);
    let source = address_from_metadata(metadata, "sourceIP", "sourcePort");
    let destination = value_text(metadata, "remoteDestination")
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| address_from_metadata(metadata, "destinationIP", "destinationPort"));
    let host = value_text(metadata, "host")
        .or_else(|| value_text(metadata, "sniffHost"))
        .or_else(|| value_text(metadata, "remoteDestination"))
        .or_else(|| value_text(metadata, "destinationIP"))
        .unwrap_or_else(|| "-".into());
    ConnectionInfo {
        id: value_text(value, "id").unwrap_or_else(|| "-".into()),
        network: value_text(metadata, "network").unwrap_or_else(|| "-".into()),
        inbound: value_text(metadata, "inboundName")
            .or_else(|| value_text(metadata, "type"))
            .unwrap_or_else(|| "-".into()),
        host,
        source,
        destination,
        upload: value_u64(value, "upload").unwrap_or_default(),
        download: value_u64(value, "download").unwrap_or_default(),
        start: value_text(value, "start")
            .map(|value| format_timestamp_text(&value))
            .unwrap_or_else(|| "-".into()),
        chains: value_string_array(value, "chains"),
        rule: value_text(value, "rule").unwrap_or_else(|| "-".into()),
        rule_payload: value_text(value, "rulePayload").unwrap_or_default(),
        process: value_text(metadata, "process")
            .or_else(|| value_text(metadata, "processPath"))
            .unwrap_or_default(),
    }
}

fn address_from_metadata(metadata: &serde_json::Value, ip_key: &str, port_key: &str) -> String {
    let ip = value_text(metadata, ip_key).unwrap_or_default();
    let port = value_text(metadata, port_key).unwrap_or_default();
    match (ip.is_empty(), port.is_empty()) {
        (true, true) => "-".into(),
        (false, true) => ip,
        (true, false) => format!(":{port}"),
        (false, false) => format!("{ip}:{port}"),
    }
}

fn traffic_totals_from_traffic(value: &serde_json::Value) -> Option<(Option<u64>, Option<u64>)> {
    let upload_total = value_u64(value, "upTotal").or_else(|| value_u64(value, "uploadTotal"));
    let download_total =
        value_u64(value, "downTotal").or_else(|| value_u64(value, "downloadTotal"));
    (upload_total.is_some() || download_total.is_some()).then_some((upload_total, download_total))
}

fn traffic_totals_from_connections(
    value: &serde_json::Value,
) -> Option<(Option<u64>, Option<u64>)> {
    let upload_total = value_u64(value, "uploadTotal");
    let download_total = value_u64(value, "downloadTotal");
    (upload_total.is_some() || download_total.is_some()).then_some((upload_total, download_total))
}

fn traffic_speeds_from_value(value: &serde_json::Value) -> Option<(Option<u64>, Option<u64>)> {
    let upload_speed = value_u64(value, "up").or_else(|| value_u64(value, "upload"));
    let download_speed = value_u64(value, "down").or_else(|| value_u64(value, "download"));
    (upload_speed.is_some() || download_speed.is_some()).then_some((upload_speed, download_speed))
}

fn value_u64(value: &serde_json::Value, key: &str) -> Option<u64> {
    value
        .get(key)
        .and_then(|value| value.as_u64().or_else(|| value.as_i64()?.try_into().ok()))
}

fn value_text(value: &serde_json::Value, key: &str) -> Option<String> {
    let value = value.get(key)?;
    if let Some(text) = value.as_str() {
        return Some(text.to_string());
    }
    if let Some(number) = value.as_u64() {
        return Some(number.to_string());
    }
    if let Some(number) = value.as_i64() {
        return Some(number.to_string());
    }
    if let Some(boolean) = value.as_bool() {
        return Some(boolean.to_string());
    }
    None
}

fn value_string_array(value: &serde_json::Value, key: &str) -> Vec<String> {
    value
        .get(key)
        .and_then(serde_json::Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    item.as_str()
                        .map(str::to_string)
                        .or_else(|| item.as_i64().map(|number| number.to_string()))
                        .or_else(|| item.as_u64().map(|number| number.to_string()))
                })
                .collect()
        })
        .unwrap_or_default()
}

async fn fetch_ip_info() -> Result<IpInfoState> {
    fetch_ip_info_with_proxy(None).await
}

async fn fetch_ip_info_with_proxy(proxy_url: Option<String>) -> Result<IpInfoState> {
    let mut builder = reqwest::Client::builder().timeout(Duration::from_secs(3));
    if let Some(proxy_url) = proxy_url {
        builder = builder.proxy(
            reqwest::Proxy::all(&proxy_url)
                .with_context(|| format!("invalid proxy URL {proxy_url}"))?,
        );
    }
    let client = builder.build().context("failed to build ipinfo client")?;
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

async fn fetch_runtime_refresh(
    paths: Paths,
    config: AppConfig,
    refresh_ip_info: bool,
) -> RuntimeRefreshResult {
    if refresh_ip_info {
        let service_config = config.clone();
        let (mut runtime, direct_ip_info, service_runtimes, proxy_ip_infos) = tokio::join!(
            fetch_mihomo_runtime(config.controller.clone()),
            fetch_ip_info(),
            fetch_service_runtimes_for_config(service_config),
            fetch_proxy_ip_infos(config.clone())
        );
        attach_runtime_process_state(&paths, &config, &mut runtime, 0);
        match direct_ip_info {
            Ok(ip_info) => {
                runtime.ip_info = Some(ip_info);
                runtime.ip_info_error = None;
            }
            Err(err) => {
                runtime.ip_info = None;
                runtime.ip_info_error = Some(err.to_string());
            }
        }
        let mut proxy_runtimes =
            build_proxy_runtimes(&config, &runtime, service_runtimes, proxy_ip_infos);
        attach_proxy_process_states(&paths, &config, &mut proxy_runtimes);
        return RuntimeRefreshResult {
            runtime,
            proxy_runtimes,
            refreshed_ip_info: true,
        };
    }

    let service_config = config.clone();
    let (mut runtime, service_runtimes) = tokio::join!(
        fetch_mihomo_runtime(config.controller.clone()),
        fetch_service_runtimes_for_config(service_config)
    );
    attach_runtime_process_state(&paths, &config, &mut runtime, 0);
    let mut proxy_runtimes =
        build_proxy_runtimes(&config, &runtime, service_runtimes, BTreeMap::new());
    attach_proxy_process_states(&paths, &config, &mut proxy_runtimes);
    RuntimeRefreshResult {
        runtime,
        proxy_runtimes,
        refreshed_ip_info: false,
    }
}

async fn fetch_service_runtimes_for_config(config: AppConfig) -> BTreeMap<String, RuntimeState> {
    if config.use_single_runtime() {
        return BTreeMap::new();
    }
    fetch_service_runtimes(config).await
}

async fn fetch_mihomo_runtime(controller: ControllerConfig) -> RuntimeState {
    let client = MihomoClient::new(&controller);
    let (version, configs, groups, connections, traffic) = tokio::join!(
        client.version(),
        client.configs(),
        client.proxy_groups(),
        client.connections(),
        client.traffic()
    );
    let traffic = traffic_from_values(connections.as_ref().ok(), traffic.as_ref().ok());
    let connections = connections
        .as_ref()
        .ok()
        .and_then(connection_state_from_value);

    match (version, configs, groups) {
        (Ok(version), Ok(configs), Ok(groups)) => RuntimeState {
            version: Some(version),
            mode: configs
                .get("mode")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string),
            groups,
            traffic,
            connections,
            process: ProcessState::default(),
            error: None,
            ip_info: None,
            ip_info_error: None,
        },
        (version, configs, groups) => RuntimeState {
            version: version.ok(),
            mode: configs.ok().and_then(|value| {
                value
                    .get("mode")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string)
            }),
            groups: groups.unwrap_or_default(),
            traffic,
            connections,
            process: ProcessState::default(),
            error: Some("mihomo offline or /proxies unavailable".into()),
            ip_info: None,
            ip_info_error: None,
        },
    }
}

async fn fetch_service_runtimes(config: AppConfig) -> BTreeMap<String, RuntimeState> {
    let mut handles = Vec::new();
    for (index, service) in config.proxy_ports.services.iter().enumerate() {
        if !service.enabled {
            continue;
        }
        let key = service_runtime_key(index);
        let controller = ControllerConfig {
            url: port_allocator::service_controller_url(&config, index),
            secret: config.controller.secret.clone(),
        };
        handles.push(tokio::spawn(async move {
            (key, fetch_mihomo_runtime(controller).await)
        }));
    }

    let mut runtimes = BTreeMap::new();
    for handle in handles {
        if let Ok((key, runtime)) = handle.await {
            runtimes.insert(key, runtime);
        }
    }
    runtimes
}

async fn fetch_proxy_ip_infos(config: AppConfig) -> BTreeMap<String, Result<IpInfoState, String>> {
    let mut handles = Vec::new();
    for index in 0..main_proxy_count(&config) {
        if matches!(main_proxy_kind(&config, index), MainProxyKind::AddPortProxy) {
            continue;
        }
        if matches!(main_proxy_kind(&config, index), MainProxyKind::Service)
            && !service_index_for_main_proxy(&config, index)
                .and_then(|index| config.proxy_ports.services.get(index))
                .is_some_and(|service| service.enabled)
        {
            continue;
        }
        let key = main_proxy_runtime_key(&config, index);
        let proxy_url = main_proxy_ip_proxy_url(&config, index);
        handles.push(tokio::spawn(async move {
            let result: Result<IpInfoState> = match proxy_url {
                Some(proxy_url) => fetch_ip_info_with_proxy(Some(proxy_url)).await,
                None => Err(anyhow::anyhow!("proxy listener unavailable")),
            };
            (key, result.map_err(|err| err.to_string()))
        }));
    }

    let mut ip_infos = BTreeMap::new();
    for handle in handles {
        if let Ok((key, result)) = handle.await {
            ip_infos.insert(key, result);
        }
    }
    ip_infos
}

fn build_proxy_runtimes(
    config: &AppConfig,
    global_runtime: &RuntimeState,
    mut service_runtimes: BTreeMap<String, RuntimeState>,
    mut proxy_ip_infos: BTreeMap<String, Result<IpInfoState, String>>,
) -> BTreeMap<String, RuntimeState> {
    let mut runtimes = BTreeMap::new();
    for index in 0..main_proxy_count(config) {
        if matches!(main_proxy_kind(config, index), MainProxyKind::AddPortProxy) {
            continue;
        }
        let key = main_proxy_runtime_key(config, index);
        let mut runtime = match main_proxy_kind(config, index) {
            MainProxyKind::Service if config.use_single_runtime() => {
                let service_index = service_index_for_main_proxy(config, index).unwrap_or_default();
                config
                    .proxy_ports
                    .services
                    .get(service_index)
                    .map(|service| {
                        single_runtime_port_proxy_runtime(global_runtime, service_index, service)
                    })
                    .unwrap_or_else(|| RuntimeState {
                        error: Some("port proxy not found".into()),
                        ..RuntimeState::default()
                    })
            }
            MainProxyKind::Service => {
                service_runtimes
                    .remove(&key)
                    .unwrap_or_else(|| RuntimeState {
                        error: Some("mihomo offline or disabled".into()),
                        ..RuntimeState::default()
                    })
            }
            MainProxyKind::System => global_runtime.clone(),
            MainProxyKind::AddPortProxy => RuntimeState::default(),
        };
        if let Some(ip_info) = proxy_ip_infos.remove(&key) {
            match ip_info {
                Ok(ip_info) => {
                    runtime.ip_info = Some(ip_info);
                    runtime.ip_info_error = None;
                }
                Err(err) => {
                    runtime.ip_info = None;
                    runtime.ip_info_error = Some(err);
                }
            }
        }
        runtimes.insert(key, runtime);
    }
    runtimes
}

fn single_runtime_port_proxy_runtime(
    global_runtime: &RuntimeState,
    service_index: usize,
    service: &PortProxyService,
) -> RuntimeState {
    let mut runtime = global_runtime.clone();
    runtime.mode = Some(service.mode.clone());
    runtime.connections = global_runtime
        .connections
        .as_ref()
        .map(|connections| filter_connections_for_service(connections, service_index, service));
    runtime.traffic = runtime
        .connections
        .as_ref()
        .map(traffic_from_connection_state);
    if !service.enabled {
        runtime.error = Some("port proxy disabled".into());
    }
    runtime
}

fn filter_connections_for_service(
    connections: &ConnectionState,
    service_index: usize,
    service: &PortProxyService,
) -> ConnectionState {
    let inbound_names = service_inbound_names(service_index, service);
    let items = connections
        .items
        .iter()
        .filter(|connection| {
            inbound_names
                .iter()
                .any(|name| connection.inbound.eq_ignore_ascii_case(name))
        })
        .cloned()
        .collect::<Vec<_>>();
    ConnectionState {
        active: items.len(),
        items,
    }
}

fn service_inbound_names(service_index: usize, service: &PortProxyService) -> Vec<String> {
    let mut names = vec![runtime_profile::single_runtime_listener_name(
        service_index,
        service,
    )];
    if !service.name.trim().is_empty() {
        names.push(service.name.trim().to_string());
    }
    if service.port != 0 && !service.kind.trim().is_empty() {
        names.push(format!("{}-{}", service.kind.trim(), service.port));
    }
    names.sort();
    names.dedup();
    names
}

fn attach_proxy_process_states(
    paths: &Paths,
    config: &AppConfig,
    runtimes: &mut BTreeMap<String, RuntimeState>,
) {
    for index in 0..main_proxy_count(config) {
        if matches!(main_proxy_kind(config, index), MainProxyKind::AddPortProxy) {
            continue;
        }
        let key = main_proxy_runtime_key(config, index);
        if let Some(runtime) = runtimes.get_mut(&key) {
            attach_runtime_process_state(paths, config, runtime, index);
        }
    }
}

fn attach_runtime_process_state(
    paths: &Paths,
    config: &AppConfig,
    runtime: &mut RuntimeState,
    index: usize,
) {
    if config.use_service_runtime() {
        runtime.process = service_core_process_state();
        return;
    }
    if config.use_single_runtime() {
        let instance = core::global_instance(paths, config);
        runtime.process = process_state_for_instance(&instance);
        return;
    }
    if let Some(instance) = main_proxy_instance(paths, config, index) {
        runtime.process = process_state_for_instance(&instance);
    }
}

fn service_core_process_state() -> ProcessState {
    let Ok(status) = service::status() else {
        return ProcessState::default();
    };
    let Some(pid) = status.core_pid else {
        return ProcessState::default();
    };
    let running = status.core_running || process_is_running(pid);
    let mut state = ProcessState {
        pid: Some(pid),
        running,
        ..ProcessState::default()
    };
    if running && let Some((cpu, mem, rss)) = process_usage(pid) {
        state.cpu_percent = cpu;
        state.mem_percent = mem;
        state.rss_bytes = rss;
    }
    state
}

fn main_proxy_instance(paths: &Paths, config: &AppConfig, index: usize) -> Option<RuntimePaths> {
    match main_proxy_kind(config, index) {
        MainProxyKind::System => Some(core::global_instance(paths, config)),
        MainProxyKind::Service => {
            let service_index = service_index_for_main_proxy(config, index)?;
            let service = config.proxy_ports.services.get(service_index)?;
            Some(core::service_instance(
                paths,
                config,
                service_index,
                service,
            ))
        }
        MainProxyKind::AddPortProxy => None,
    }
}

fn process_state_for_instance(instance: &RuntimePaths) -> ProcessState {
    let Some(pid) = read_pid_file_sync(&instance.pid_file) else {
        return ProcessState::default();
    };
    let running = process_is_running(pid);
    let mut state = ProcessState {
        pid: Some(pid),
        running,
        ..ProcessState::default()
    };
    if running && let Some((cpu, mem, rss)) = process_usage(pid) {
        state.cpu_percent = cpu;
        state.mem_percent = mem;
        state.rss_bytes = rss;
    }
    state
}

fn read_pid_file_sync(path: &Path) -> Option<u32> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|content| content.trim().parse().ok())
}

fn process_is_running(pid: u32) -> bool {
    #[cfg(unix)]
    {
        let status = unsafe { libc::kill(pid as libc::pid_t, 0) };
        if status == 0 {
            return true;
        }
        std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
    #[cfg(windows)]
    {
        Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
            .output()
            .is_ok_and(|output| {
                output.status.success()
                    && String::from_utf8_lossy(&output.stdout).contains(&format!("\"{pid}\""))
            })
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = pid;
        false
    }
}

fn process_usage(pid: u32) -> Option<(Option<f32>, Option<f32>, Option<u64>)> {
    let output = Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "%cpu=,%mem=,rss="])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut parts = stdout.split_whitespace();
    let cpu = parts.next().and_then(|value| value.parse::<f32>().ok());
    let mem = parts.next().and_then(|value| value.parse::<f32>().ok());
    let rss = parts
        .next()
        .and_then(|value| value.parse::<u64>().ok())
        .map(|kb| kb.saturating_mul(1024));
    Some((cpu, mem, rss))
}

fn read_log_tail(path: &Path, max_lines: usize) -> Vec<String> {
    let Ok(content) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut lines = content
        .lines()
        .map(str::trim_end)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect::<Vec<_>>();
    if lines.len() > max_lines {
        lines.drain(0..lines.len() - max_lines);
    }
    lines
}

fn preserve_ip_info(previous: &RuntimeState, runtime: &mut RuntimeState) {
    runtime.ip_info.clone_from(&previous.ip_info);
    runtime.ip_info_error.clone_from(&previous.ip_info_error);
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

fn format_bytes_one_decimal(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    const TB: f64 = GB * 1024.0;
    let bytes = bytes as f64;
    if bytes >= TB * 0.1 {
        format!("{:.1}TB", bytes / TB)
    } else if bytes >= GB * 0.1 {
        format!("{:.1}GB", bytes / GB)
    } else if bytes >= MB {
        format!("{:.1}MB", bytes / MB)
    } else if bytes >= KB {
        format!("{:.1}KB", bytes / KB)
    } else {
        format!("{bytes:.0}B")
    }
}

fn format_count(value: usize) -> String {
    if value >= 1_000_000 {
        format!("{:.1}M", value as f64 / 1_000_000.0)
    } else if value >= 10_000 {
        format!("{:.0}K", value as f64 / 1_000.0)
    } else if value >= 1_000 {
        format!("{:.1}K", value as f64 / 1_000.0)
    } else {
        value.to_string()
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

fn bios_panel_preserve_indent<'a>(title: &'a str, lines: Vec<Line<'a>>) -> Paragraph<'a> {
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
        .wrap(Wrap { trim: false })
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

fn nearest_selectable_runtime_index(index: usize) -> usize {
    let len = RuntimeItem::ALL.len();
    if len == 0 {
        return 0;
    }
    let index = index.min(len - 1);
    if runtime_index_is_selectable(index) {
        return index;
    }
    selectable_runtime_index_from(index, true)
}

fn next_selectable_runtime_index(current: usize) -> usize {
    let len = RuntimeItem::ALL.len();
    if len == 0 {
        return 0;
    }
    let current = current.min(len - 1);
    for offset in 1..=len {
        let index = (current + offset) % len;
        if runtime_index_is_selectable(index) {
            return index;
        }
    }
    current
}

fn prev_selectable_runtime_index(current: usize) -> usize {
    let len = RuntimeItem::ALL.len();
    if len == 0 {
        return 0;
    }
    let current = current.min(len - 1);
    for offset in 1..=len {
        let index = (current + len - (offset % len)) % len;
        if runtime_index_is_selectable(index) {
            return index;
        }
    }
    current
}

fn selectable_runtime_index_from(index: usize, forward: bool) -> usize {
    let len = RuntimeItem::ALL.len();
    if len == 0 {
        return 0;
    }
    let index = index.min(len - 1);
    if runtime_index_is_selectable(index) {
        return index;
    }
    if forward {
        for next in index + 1..len {
            if runtime_index_is_selectable(next) {
                return next;
            }
        }
        for next in 0..index {
            if runtime_index_is_selectable(next) {
                return next;
            }
        }
    } else {
        if index == 0 {
            for next in 1..len {
                if runtime_index_is_selectable(next) {
                    return next;
                }
            }
        }
        for prev in (0..index).rev() {
            if runtime_index_is_selectable(prev) {
                return prev;
            }
        }
        for prev in (index + 1..len).rev() {
            if runtime_index_is_selectable(prev) {
                return prev;
            }
        }
    }
    index
}

fn runtime_index_is_selectable(index: usize) -> bool {
    RuntimeItem::ALL
        .get(index)
        .is_some_and(|item| !item.is_section())
}

fn first_selectable_exit_index() -> usize {
    nearest_selectable_exit_index(0)
}

fn nearest_selectable_exit_index(index: usize) -> usize {
    let len = ExitItem::ALL.len();
    if len == 0 {
        return 0;
    }
    let index = index.min(len - 1);
    if exit_index_is_selectable(index) {
        return index;
    }
    selectable_exit_index_from(index, true)
}

fn next_selectable_exit_index(current: usize) -> usize {
    let len = ExitItem::ALL.len();
    if len == 0 {
        return 0;
    }
    let current = current.min(len - 1);
    for offset in 1..=len {
        let index = (current + offset) % len;
        if exit_index_is_selectable(index) {
            return index;
        }
    }
    current
}

fn prev_selectable_exit_index(current: usize) -> usize {
    let len = ExitItem::ALL.len();
    if len == 0 {
        return 0;
    }
    let current = current.min(len - 1);
    for offset in 1..=len {
        let index = (current + len - (offset % len)) % len;
        if exit_index_is_selectable(index) {
            return index;
        }
    }
    current
}

fn selectable_exit_index_from(index: usize, forward: bool) -> usize {
    let len = ExitItem::ALL.len();
    if len == 0 {
        return 0;
    }
    let index = index.min(len - 1);
    if exit_index_is_selectable(index) {
        return index;
    }
    if forward {
        for next in index + 1..len {
            if exit_index_is_selectable(next) {
                return next;
            }
        }
        for next in 0..index {
            if exit_index_is_selectable(next) {
                return next;
            }
        }
    } else {
        if index == 0 {
            for next in 1..len {
                if exit_index_is_selectable(next) {
                    return next;
                }
            }
        }
        for prev in (0..index).rev() {
            if exit_index_is_selectable(prev) {
                return prev;
            }
        }
        for prev in (index + 1..len).rev() {
            if exit_index_is_selectable(prev) {
                return prev;
            }
        }
    }
    index
}

fn exit_index_is_selectable(index: usize) -> bool {
    ExitItem::ALL
        .get(index)
        .is_some_and(|item| !item.is_section())
}

fn clamp_index(index: &mut usize, len: usize) {
    if len == 0 {
        *index = 0;
    } else if *index >= len {
        *index = len - 1;
    }
}

fn page_down_index(current: usize, len: usize, step: usize) -> usize {
    if len == 0 {
        0
    } else {
        current.saturating_add(step.max(1)).min(len - 1)
    }
}

fn page_up_index(current: usize, step: usize) -> usize {
    current.saturating_sub(step.max(1))
}

fn terminal_page_step() -> usize {
    crossterm::terminal::size()
        .map(|(_, height)| usize::from(height.saturating_sub(8)).clamp(1, PAGE_JUMP_MAX))
        .unwrap_or(PAGE_JUMP_FALLBACK)
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

fn parse_nameserver_policy(value: &str) -> Result<BTreeMap<String, Vec<String>>> {
    let mut policy = BTreeMap::new();
    for entry in value.split([';', '\n']).map(str::trim) {
        if entry.is_empty() {
            continue;
        }
        let (domain, servers) = split_nameserver_policy_entry(entry)
            .with_context(|| format!("invalid DNS policy entry: {entry}"))?;
        let domain = domain.trim();
        if domain.is_empty() {
            anyhow::bail!("DNS policy domain is empty");
        }
        let servers = split_list(servers);
        if servers.is_empty() {
            anyhow::bail!("DNS policy nameserver list is empty for {domain}");
        }
        policy.insert(domain.to_string(), servers);
    }
    Ok(policy)
}

fn split_nameserver_policy_entry(entry: &str) -> Option<(&str, &str)> {
    entry
        .split_once('=')
        .or_else(|| entry.split_once(": "))
        .or_else(|| entry.split_once(":\t"))
}

fn parse_nameserver_policy_or_alert(
    app: &mut ConfigApp,
    value: &str,
) -> Option<BTreeMap<String, Vec<String>>> {
    match parse_nameserver_policy(value) {
        Ok(policy) => Some(policy),
        Err(_) => {
            app.alert(
                "Invalid DNS Policy",
                "Use entries like +.taobao.net=30.30.30.30; +.example.com=1.1.1.1, 8.8.8.8.",
            );
            None
        }
    }
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

fn is_multiline_input(input_mode: InputMode) -> bool {
    matches!(
        input_mode,
        InputMode::DnsLanDomains
            | InputMode::DnsLanNameserver
            | InputMode::DnsNameserverPolicy
            | InputMode::DnsDirectNameserver
            | InputMode::DnsNameserver
            | InputMode::DnsFallback
            | InputMode::DnsFakeIpFilter
    )
}

fn should_insert_input_newline(input_mode: InputMode, modifiers: KeyModifiers) -> bool {
    is_multiline_input(input_mode)
        && modifiers.intersects(KeyModifiers::SHIFT | KeyModifiers::ALT | KeyModifiers::CONTROL)
}

fn adjust_number_input(value: &mut String, cursor: &mut usize, delta: i32) {
    let current = value.parse::<i32>().unwrap_or_default();
    let next = (current + delta).clamp(1, u16::MAX as i32);
    *value = next.to_string();
    *cursor = value.len();
}

fn push_number_digit(value: &mut String, cursor: &mut usize, digit: char) {
    let cursor_index = normalize_cursor(value, *cursor);
    let mut next = value.clone();
    if next == "0" && cursor_index == next.len() {
        next.clear();
    } else {
        next.insert(cursor_index, digit);
    }
    if matches!(next.parse::<u32>(), Ok(number) if number <= u16::MAX as u32) {
        *value = next;
        *cursor = if value == "0" {
            value.len()
        } else {
            next_char_boundary(value, cursor_index)
        };
    }
}

fn clamp_input_cursor(app: &mut ConfigApp) {
    app.input_cursor = normalize_cursor(&app.input, app.input_cursor);
}

fn normalize_cursor(value: &str, index: usize) -> usize {
    prev_char_boundary(value, index.min(value.len()))
}

fn prev_char_boundary(value: &str, index: usize) -> usize {
    let mut index = index.min(value.len());
    while index > 0 && !value.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn next_char_boundary(value: &str, index: usize) -> usize {
    let mut index = index.min(value.len());
    if index >= value.len() {
        return value.len();
    }
    index += 1;
    while index < value.len() && !value.is_char_boundary(index) {
        index += 1;
    }
    index
}

fn insert_input_text(app: &mut ConfigApp, text: &str) {
    clamp_input_cursor(app);
    app.input.insert_str(app.input_cursor, text);
    app.input_cursor += text.len();
    app.input_desired_column = None;
}

fn insert_input_char(app: &mut ConfigApp, ch: char) {
    clamp_input_cursor(app);
    app.input.insert(app.input_cursor, ch);
    app.input_cursor += ch.len_utf8();
    app.input_desired_column = None;
}

fn delete_input_char_before_cursor(app: &mut ConfigApp) {
    clamp_input_cursor(app);
    if app.input_cursor == 0 {
        return;
    }
    let previous = prev_char_boundary(&app.input, app.input_cursor.saturating_sub(1));
    app.input.drain(previous..app.input_cursor);
    app.input_cursor = previous;
    app.input_desired_column = None;
}

fn delete_input_char_at_cursor(app: &mut ConfigApp) {
    clamp_input_cursor(app);
    if app.input_cursor >= app.input.len() {
        return;
    }
    let next = next_char_boundary(&app.input, app.input_cursor);
    app.input.drain(app.input_cursor..next);
    app.input_desired_column = None;
}

fn move_input_cursor_left(app: &mut ConfigApp) {
    clamp_input_cursor(app);
    if app.input_cursor == 0 {
        return;
    }
    app.input_cursor = prev_char_boundary(&app.input, app.input_cursor.saturating_sub(1));
    app.input_desired_column = None;
}

fn move_input_cursor_right(app: &mut ConfigApp) {
    clamp_input_cursor(app);
    app.input_cursor = next_char_boundary(&app.input, app.input_cursor);
    app.input_desired_column = None;
}

fn move_input_cursor_line_start(app: &mut ConfigApp) {
    clamp_input_cursor(app);
    app.input_cursor = line_start(&app.input, app.input_cursor);
    app.input_desired_column = None;
}

fn move_input_cursor_line_end(app: &mut ConfigApp) {
    clamp_input_cursor(app);
    app.input_cursor = line_end(&app.input, app.input_cursor);
    app.input_desired_column = None;
}

fn move_input_cursor_line(app: &mut ConfigApp, delta: i32) {
    clamp_input_cursor(app);
    let current_column = app
        .input_desired_column
        .unwrap_or_else(|| cursor_column(&app.input, app.input_cursor));
    let target = if delta < 0 {
        previous_line_bounds(&app.input, app.input_cursor)
    } else {
        next_line_bounds(&app.input, app.input_cursor)
    };
    let Some((start, end)) = target else {
        return;
    };
    app.input_cursor = cursor_at_column(&app.input, start, end, current_column);
    app.input_desired_column = Some(current_column);
}

fn line_start(value: &str, index: usize) -> usize {
    let index = normalize_cursor(value, index);
    value[..index]
        .rfind('\n')
        .map_or(0, |position| position + 1)
}

fn line_end(value: &str, index: usize) -> usize {
    let index = normalize_cursor(value, index);
    value[index..]
        .find('\n')
        .map_or(value.len(), |position| index + position)
}

fn cursor_column(value: &str, index: usize) -> usize {
    let index = normalize_cursor(value, index);
    value[line_start(value, index)..index].chars().count()
}

fn cursor_at_column(value: &str, start: usize, end: usize, column: usize) -> usize {
    value[start..end]
        .char_indices()
        .nth(column)
        .map_or(end, |(offset, _)| start + offset)
}

fn previous_line_bounds(value: &str, index: usize) -> Option<(usize, usize)> {
    let current_start = line_start(value, index);
    if current_start == 0 {
        return None;
    }
    let previous_end = current_start.saturating_sub(1);
    let previous_start = line_start(value, previous_end);
    Some((previous_start, previous_end))
}

fn next_line_bounds(value: &str, index: usize) -> Option<(usize, usize)> {
    let current_end = line_end(value, index);
    if current_end >= value.len() {
        return None;
    }
    let next_start = current_end + 1;
    Some((next_start, line_end(value, next_start)))
}

fn join_list(values: &[String]) -> String {
    values.join(", ")
}

fn join_multiline_list(values: &[String]) -> String {
    values.join("\n")
}

fn format_nameserver_policy(policy: &BTreeMap<String, Vec<String>>) -> String {
    policy
        .iter()
        .map(|(domain, servers)| format!("{domain}={}", join_list(servers)))
        .collect::<Vec<_>>()
        .join("; ")
}

fn format_multiline_nameserver_policy(policy: &BTreeMap<String, Vec<String>>) -> String {
    policy
        .iter()
        .map(|(domain, servers)| format!("{domain}={}", join_list(servers)))
        .collect::<Vec<_>>()
        .join("\n")
}

fn compact_nameserver_policy(policy: &BTreeMap<String, Vec<String>>) -> String {
    let formatted = format_nameserver_policy(policy);
    if formatted.is_empty() {
        return "-".into();
    }
    const MAX: usize = 44;
    if formatted.chars().count() <= MAX {
        formatted
    } else {
        let mut value = formatted.chars().take(MAX).collect::<String>();
        value.push_str("...");
        value
    }
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
        execute!(stdout, EnterAlternateScreen, EnableBracketedPaste)?;
        let backend = CrosstermBackend::new(stdout);
        let terminal = Terminal::new(backend)?;
        Ok(Self { terminal })
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(
            self.terminal.backend_mut(),
            DisableBracketedPaste,
            LeaveAlternateScreen
        );
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
    fn nameserver_policy_accepts_domain_server_mappings() -> Result<()> {
        let policy = parse_nameserver_policy(
            "+.taobao.net=30.30.30.30; +.example.com=1.1.1.1, https://dns.example/dns-query",
        )?;

        assert_eq!(
            policy.get("+.taobao.net"),
            Some(&vec!["30.30.30.30".to_string()])
        );
        assert_eq!(
            policy.get("+.example.com"),
            Some(&vec![
                "1.1.1.1".to_string(),
                "https://dns.example/dns-query".to_string()
            ])
        );
        assert_eq!(
            format_nameserver_policy(&policy),
            "+.example.com=1.1.1.1, https://dns.example/dns-query; +.taobao.net=30.30.30.30"
        );
        Ok(())
    }

    #[test]
    fn dns_rows_show_nameserver_policy() -> Result<()> {
        let mut config = AppConfig::default();
        config
            .dns
            .nameserver_policy
            .insert("+.taobao.net".into(), vec!["30.30.30.30".into()]);

        let rows = dns_rows(&config, Language::En);
        let policy_row = rows
            .iter()
            .find(|row| row.label == "DNS Policy")
            .context("DNS policy row")?;

        assert_eq!(policy_row.value, "+.taobao.net=30.30.30.30");
        assert!(matches!(
            policy_row.kind,
            RowKind::Input(InputMode::DnsNameserverPolicy)
        ));
        Ok(())
    }

    #[test]
    fn dns_is_top_level_section() {
        assert!(Page::Dns.is_section());
        assert_eq!(Page::Main.next(), Page::Profile);
        assert_eq!(Page::Profile.prev(), Page::Main);
        assert_eq!(Page::Profile.next(), Page::Subscription);
        assert_eq!(Page::Subscription.next(), Page::Dns);
        assert_eq!(Page::Dns.prev(), Page::Subscription);
        assert_eq!(Page::Dns.next(), Page::Runtime);
        assert_eq!(Page::Runtime.prev(), Page::Dns);
        assert_eq!(Page::Chat.prev(), Page::Runtime);
        assert_eq!(Page::Dns.title(), "DNS");
    }

    #[test]
    fn profile_rows_expose_default_and_add_action() {
        let config = AppConfig::default();
        let rows = proxy_profile_rows(&config);

        assert_eq!(rows.first().map(|row| row.label.as_str()), Some("default"));
        assert_eq!(rows.first().map(|row| row.value.as_str()), Some("active"));
        assert_eq!(
            rows.get(1).map(|row| row.label.as_str()),
            Some("Add Profile")
        );
    }

    #[test]
    fn global_proxy_config_uses_profile_instead_of_inline_route_settings() {
        let config = AppConfig::default();
        let app = ConfigApp::new(&config);
        let rows = proxy_config_rows(&config, &app);

        assert!(rows.iter().any(|row| row.label == "Profile"));
        assert!(!rows.iter().any(|row| row.label == "Subscription"));
        assert!(!rows.iter().any(|row| row.label == "Mode"));
        assert!(!rows.iter().any(|row| row.label == "Proxy Server"));
    }

    #[test]
    fn global_proxy_enabled_is_independent_from_system_proxy() -> Result<()> {
        let config = AppConfig::default();
        let app = ConfigApp::new(&config);

        let row = main_proxy_row(&config, 0);
        assert!(row.enabled);
        assert!(row.features.contains("SYS=OFF"));

        let rows = proxy_config_rows(&config, &app);
        let enabled = rows
            .iter()
            .find(|row| row.label == "Enabled")
            .context("enabled row")?;
        assert_eq!(enabled.value, "on");
        assert!(matches!(enabled.kind, RowKind::Status));

        let os_proxy = rows
            .iter()
            .find(|row| row.label == "Sys Proxy")
            .context("system proxy row")?;
        assert_eq!(os_proxy.value, "off");
        assert!(matches!(
            os_proxy.kind,
            RowKind::Toggle(ToggleAction::SystemProxy)
        ));
        Ok(())
    }

    #[tokio::test]
    async fn global_proxy_enabled_submit_does_not_toggle_system_proxy() -> Result<()> {
        let paths = test_paths("global-enabled-decoupled")?;
        let mut config = AppConfig::default();
        let mut app = ConfigApp::new(&config);
        app.page = Page::ProxyConfig;
        app.selected_proxy_field = ProxyConfigField::SYSTEM_ALL
            .iter()
            .position(|field| *field == ProxyConfigField::Enabled)
            .context("enabled field")?;

        submit_proxy_config_item(&paths, &mut config, &mut app).await?;

        assert!(!config.system_proxy.enabled);
        assert!(!app.dirty);
        assert!(app.status.contains("Main mihomo proxy"));

        app.selected_proxy_field = ProxyConfigField::SYSTEM_ALL
            .iter()
            .position(|field| *field == ProxyConfigField::OsProxy)
            .context("system proxy field")?;
        submit_proxy_config_item(&paths, &mut config, &mut app).await?;

        assert!(config.system_proxy.enabled);
        assert!(app.dirty);
        Ok(())
    }

    #[tokio::test]
    async fn main_global_proxy_enter_opens_config_screen() -> Result<()> {
        let paths = test_paths("main-global-config")?;
        let mut config = AppConfig::default();
        let mut app = ConfigApp::new(&config);

        submit_selection(&paths, &mut config, &mut app).await?;

        assert_eq!(app.page, Page::ProxyConfig);
        assert_eq!(app.dropdown, None);
        Ok(())
    }

    #[tokio::test]
    async fn global_proxy_profile_row_opens_profile_dropdown() -> Result<()> {
        let paths = test_paths("global-profile-dropdown")?;
        let mut config = AppConfig::default();
        let mut app = ConfigApp::new(&config);
        app.page = Page::ProxyConfig;
        app.selected_proxy_field = ProxyConfigField::SYSTEM_ALL
            .iter()
            .position(|field| *field == ProxyConfigField::Profile)
            .context("profile field")?;

        submit_selection(&paths, &mut config, &mut app).await?;

        assert_eq!(app.dropdown, Some(Dropdown::ProxyProfile));
        Ok(())
    }

    #[tokio::test]
    async fn profile_dropdown_activates_selected_profile() -> Result<()> {
        let paths = test_paths("profile-dropdown-activates")?;
        let mut config = AppConfig::default();
        config.proxy_profiles.push(ProxyProfile {
            name: "work".into(),
            subscription: Some("sub-a".into()),
            mode: "global".into(),
            proxy: Some("JP 01".into()),
            rule_selections: BTreeMap::new(),
        });
        let mut app = ConfigApp::new(&config);
        app.open_proxy_profile_dropdown(&config);
        app.selected_dropdown = 1;

        submit_dropdown(&paths, &mut config, &mut app).await?;

        assert_eq!(config.active_proxy_profile, "work");
        assert_eq!(config.active_profile.as_deref(), Some("sub-a"));
        assert_eq!(config.runtime_mode, "global");
        assert_eq!(
            config.proxy_selections.get("GLOBAL").map(String::as_str),
            Some("JP 01")
        );
        assert!(app.dirty);
        Ok(())
    }

    #[tokio::test]
    async fn chat_page_keeps_section_navigation_keys() -> Result<()> {
        let paths = test_paths("chat-section-navigation")?;
        let mut config = AppConfig::default();
        let mut app = ConfigApp::new(&config);

        app.switch_section(Page::Chat);
        handle_chat_key(
            &paths,
            &mut config,
            &mut app,
            KeyEvent::new(KeyCode::Left, KeyModifiers::empty()),
        )
        .await?;
        assert_eq!(app.page, Page::Runtime);

        app.switch_section(Page::Chat);
        handle_chat_key(
            &paths,
            &mut config,
            &mut app,
            KeyEvent::new(KeyCode::Right, KeyModifiers::empty()),
        )
        .await?;
        assert_eq!(app.page, Page::Exit);

        app.switch_section(Page::Chat);
        handle_chat_key(
            &paths,
            &mut config,
            &mut app,
            KeyEvent::new(KeyCode::Tab, KeyModifiers::empty()),
        )
        .await?;
        assert_eq!(app.page, Page::Exit);

        app.switch_section(Page::Chat);
        handle_chat_key(
            &paths,
            &mut config,
            &mut app,
            KeyEvent::new(KeyCode::F(10), KeyModifiers::empty()),
        )
        .await?;
        assert_eq!(app.confirm, Some(ConfirmAction::SaveRestart));

        Ok(())
    }

    #[test]
    fn chat_input_renders_prompt_and_text_on_same_line() {
        let lines = chat_input_lines("hello", 20, 2);
        let rendered = lines.iter().map(line_text).collect::<Vec<_>>();

        assert_eq!(rendered, vec!["> hello ".to_string(), "  ".to_string()]);
        assert!(
            lines[0]
                .spans
                .iter()
                .any(|span| span.style.bg == Some(Color::White))
        );
    }

    #[test]
    fn chat_input_indents_continuation_lines() {
        let lines = chat_input_lines("one\ntwo", 20, 2);
        let rendered = lines.iter().map(line_text).collect::<Vec<_>>();

        assert_eq!(rendered, vec!["> one".to_string(), "  two ".to_string()]);
    }

    #[test]
    fn zh_cn_language_keeps_terms_and_translates_actions() -> Result<()> {
        let paths = test_paths("zh-cn-runtime-labels")?;
        let config = AppConfig::default();
        let app = ConfigApp::new_with_language(&config, Language::ZhCn);
        let rows = runtime_rows(&paths, &config, &app);

        assert!(rows.iter().any(|row| row.label == "刷新 Runtime"));
        assert!(rows.iter().any(|row| row.label == "LLM Provider"));
        assert_eq!(Page::Subscription.title_for(Language::ZhCn), "订阅");
        assert_eq!(Page::Runtime.title_for(Language::ZhCn), "Runtime");
        Ok(())
    }

    #[test]
    fn chat_input_keeps_cursor_visible_in_two_rows() {
        let lines = chat_input_lines("one\ntwo\nthree", 20, 2);
        let rendered = lines.iter().map(line_text).collect::<Vec<_>>();

        assert_eq!(rendered, vec!["> …two".to_string(), "  three ".to_string()]);
        assert!(
            lines[1]
                .spans
                .iter()
                .any(|span| span.style.bg == Some(Color::White))
        );
    }

    #[test]
    fn chat_transcript_wraps_before_visible_window_is_applied() {
        let mut app = ConfigApp::new(&AppConfig::default());
        app.chat.entries.push(ChatEntry {
            kind: ChatEntryKind::Assistant,
            content: "abcdefghijklmnopqrstuvwxyz".into(),
        });

        let rendered = chat_transcript_lines(&app, 10, 12)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>();

        assert_eq!(
            rendered,
            vec![
                "assistant".to_string(),
                "  abcdefghij".to_string(),
                "  klmnopqrst".to_string(),
                "  uvwxyz".to_string(),
                String::new(),
            ]
        );
    }

    #[test]
    fn chat_transcript_preserves_trailing_stream_newline() {
        let mut app = ConfigApp::new(&AppConfig::default());
        app.chat.entries.push(ChatEntry {
            kind: ChatEntryKind::Assistant,
            content: "line\n".into(),
        });

        let rendered = chat_transcript_lines(&app, 10, 20)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>();

        assert_eq!(
            rendered,
            vec![
                "assistant".to_string(),
                "  line".to_string(),
                "  ".to_string(),
                String::new(),
            ]
        );
    }

    #[test]
    fn config_patch_diff_marks_yaml_changes() -> Result<()> {
        let before = AppConfig::default();
        let mut after = before.clone();
        after.proxy_host = "0.0.0.0".into();

        let diff = config_patch_diff(&before, &after)?;

        assert!(diff.contains("-proxy_host: 127.0.0.1"));
        assert!(diff.contains("+proxy_host: 0.0.0.0"));
        Ok(())
    }

    #[test]
    fn chat_patch_diff_lines_use_red_and_green_backgrounds() {
        let mut app = ConfigApp::new(&AppConfig::default());
        app.chat.entries.push(ChatEntry {
            kind: ChatEntryKind::Patch,
            content:
                "Applied to draft: test\n context\n-old\n+new\n\nPress F10 to save and restart the service."
                    .into(),
        });

        let lines = chat_transcript_lines(&app, 20, 40);
        assert!(!lines.iter().any(|line| line_text(line) == "patch"));
        assert!(lines.iter().any(|line| line_text(line) == "  context"));
        let remove_bg = lines
            .iter()
            .find(|line| line_text(line) == " -old")
            .and_then(|line| line.spans.first())
            .and_then(|span| span.style.bg);
        let add_bg = lines
            .iter()
            .find(|line| line_text(line) == " +new")
            .and_then(|line| line.spans.first())
            .and_then(|span| span.style.bg);

        assert_eq!(remove_bg, Some(PATCH_DIFF_REMOVE_BG));
        assert_eq!(add_bg, Some(PATCH_DIFF_ADD_BG));
    }

    #[test]
    fn chat_conversation_context_keeps_user_assistant_and_patch_entries() {
        let entries = vec![
            ChatEntry {
                kind: ChatEntryKind::User,
                content: "first".into(),
            },
            ChatEntry {
                kind: ChatEntryKind::Tool,
                content: "running read_config".into(),
            },
            ChatEntry {
                kind: ChatEntryKind::Assistant,
                content: "answer".into(),
            },
            ChatEntry {
                kind: ChatEntryKind::Patch,
                content: "changed mode".into(),
            },
            ChatEntry {
                kind: ChatEntryKind::User,
                content: "follow up".into(),
            },
        ];

        let context = chat_conversation_context(&entries);

        assert_eq!(context.len(), 4);
        assert_eq!(context[0].role, crate::agent::ConversationRole::User);
        assert_eq!(context[0].content, "first");
        assert_eq!(context[1].role, crate::agent::ConversationRole::Assistant);
        assert_eq!(context[1].content, "answer");
        assert_eq!(context[2].role, crate::agent::ConversationRole::Assistant);
        assert!(context[2].content.contains("changed mode"));
        assert_eq!(context[3].role, crate::agent::ConversationRole::User);
        assert_eq!(context[3].content, "follow up");
    }

    #[test]
    fn progress_helpers_handle_empty_and_partial_work() {
        assert_eq!(progress_percent(0, 0), 100);
        assert_eq!(progress_percent(45, 180), 25);
        assert_eq!(progress_bar(2, 4, 8), "[████░░░░]");
    }

    #[test]
    fn page_index_helpers_jump_and_clamp() {
        assert_eq!(page_down_index(0, 20, 8), 8);
        assert_eq!(page_down_index(16, 20, 8), 19);
        assert_eq!(page_down_index(0, 0, 8), 0);
        assert_eq!(page_up_index(12, 8), 4);
        assert_eq!(page_up_index(3, 8), 0);
    }

    #[test]
    fn root_escape_opens_exit_screen_before_confirming_exit() {
        let config = AppConfig::default();
        let mut app = ConfigApp::new(&config);

        app.back_or_exit_screen();
        assert_eq!(app.page, Page::Exit);
        assert!(app.confirm.is_none());

        app.back_or_exit_screen();
        assert_eq!(app.confirm, Some(ConfirmAction::ExitWithoutSaving));
    }

    #[test]
    fn exit_rows_expose_function_actions_without_f11() {
        let rows = exit_rows(Language::En);
        assert!(matches!(
            rows.first().map(|row| row.kind),
            Some(RowKind::Section)
        ));
        assert_eq!(row_style(&rows[0], false).fg, Some(Color::DarkGray));
        assert_eq!(row_style(&rows[0], true).fg, Some(Color::DarkGray));
        assert_eq!(rows.first().map(|row| row.label.as_str()), Some("Save"));
        assert_eq!(rows.get(4).map(|row| row.label.as_str()), Some("Runtime"));
        assert_eq!(rows.get(9).map(|row| row.label.as_str()), Some("Exit"));
        assert_eq!(
            rows.get(first_selectable_exit_index())
                .map(|row| row.label.as_str()),
            Some("Save")
        );
        assert!(
            rows.iter()
                .any(|row| row.label == "Load Defaults" && row.value == "F9")
        );
        assert!(
            rows.iter()
                .any(|row| row.label == "Save & Restart" && row.value == "F10")
        );
        assert!(
            rows.iter()
                .any(|row| row.label == "Save" && row.value == "disk")
        );
        assert!(rows.iter().any(|row| row.label == "Save, Restart & Exit"));
        assert!(rows.iter().any(|row| row.label == "Start"));
        assert!(rows.iter().any(|row| row.label == "Stop"));
        assert!(rows.iter().any(|row| row.label == "Reload"));
        assert!(rows.iter().any(|row| row.label == "Restart"));
        assert_eq!(rows.last().map(|row| row.label.as_str()), Some("Exit"));
        assert!(!rows.iter().any(|row| row.label == "Back To Main"));
        assert!(!rows.iter().any(|row| row.value == "F11"));
    }

    #[test]
    fn exit_navigation_skips_section_headers() -> Result<()> {
        let paths = test_paths("exit-section-navigation")?;
        let config = AppConfig::default();
        let mut app = ConfigApp::new(&config);
        app.page = Page::Exit;

        assert_eq!(app.selected_exit_item(), ExitItem::SaveConfig);

        app.selected_exit = 3;
        app.move_next(&paths, &config);
        assert_eq!(app.selected_exit_item(), ExitItem::StartRuntime);

        app.move_prev(&paths, &config);
        assert_eq!(app.selected_exit_item(), ExitItem::SaveRestartExit);

        app.selected_exit = 0;
        app.clamp_selection(&paths, &config);
        assert_eq!(app.selected_exit_item(), ExitItem::SaveConfig);

        app.selected_exit = 4;
        app.move_page_prev(&paths, &config);
        assert_eq!(app.selected_exit_item(), ExitItem::SaveConfig);

        Ok(())
    }

    #[tokio::test]
    async fn exit_item_closes_without_confirm() -> Result<()> {
        let paths = test_paths("exit-direct")?;
        let mut config = AppConfig::default();
        let mut app = ConfigApp::new(&config);
        app.page = Page::Exit;
        app.selected_exit = ExitItem::ALL.len() - 1;

        submit_exit_item(&paths, &mut config, &mut app).await?;

        assert!(app.should_quit);
        assert!(app.confirm.is_none());
        Ok(())
    }

    #[test]
    fn runtime_offline_uses_non_blocking_notice() {
        let config = AppConfig::default();
        let mut app = ConfigApp::new(&config);
        assert_eq!(service_state(&app), "svc checking");

        app.runtime_checked = true;
        app.runtime.error = Some("mihomo offline".into());
        assert_eq!(service_state(&app), "svc offline");
        assert!(runtime_offline_notice(&app).is_some());
    }

    #[test]
    fn unix_timestamps_are_rendered_as_dates() {
        assert_eq!(
            format_unix_timestamp_with_offset(1_779_520_560, UtcOffset::UTC),
            "2026/05/23 07:16:00"
        );
        assert_eq!(
            format_unix_date_with_offset(1_779_520_560, UtcOffset::UTC),
            "2026/05/23"
        );
        assert_eq!(format_optional_timestamp(Some("manual"), "-"), "manual");
        assert_eq!(format_optional_timestamp(None, "never"), "never");
    }

    #[test]
    fn subscription_url_input_uses_multiline_box() {
        assert_eq!(
            input_dialog_spec(InputMode::SubscriptionUrl),
            (URL_INPUT_WIDTH, URL_INPUT_ROWS)
        );
        assert_eq!(
            input_dialog_spec(InputMode::SubscriptionName),
            (DEFAULT_INPUT_WIDTH, DEFAULT_INPUT_ROWS)
        );

        let lines = wrapped_input_lines("https://example.invalid/abcdef", 10, 3);
        assert_eq!(
            lines,
            vec![
                "https://ex".to_string(),
                "ample.inva".to_string(),
                "lid/abcdef".to_string()
            ]
        );

        let truncated = wrapped_input_lines("abcdefghijklmnopqrstuvwxyz", 8, 2);
        assert_eq!(truncated, vec!["…qrstuvw".to_string(), "yz".to_string()]);
    }

    #[test]
    fn dns_text_inputs_use_multiline_box() {
        for mode in [
            InputMode::DnsLanDomains,
            InputMode::DnsLanNameserver,
            InputMode::DnsNameserverPolicy,
            InputMode::DnsDirectNameserver,
            InputMode::DnsNameserver,
            InputMode::DnsFallback,
            InputMode::DnsFakeIpFilter,
        ] {
            assert_eq!(
                input_dialog_spec(mode),
                (DNS_TEXT_INPUT_WIDTH, DNS_TEXT_INPUT_ROWS)
            );
            assert!(is_multiline_input(mode));
        }
        assert_eq!(
            input_dialog_spec(InputMode::DnsListen),
            (DEFAULT_INPUT_WIDTH, DEFAULT_INPUT_ROWS)
        );
        assert!(!is_multiline_input(InputMode::DnsListen));
    }

    #[test]
    fn wrapped_input_lines_preserve_explicit_newlines() {
        let lines = wrapped_input_lines("one\ntwo\n\nthree", 8, 5);
        assert_eq!(
            lines,
            vec![
                "one".to_string(),
                "two".to_string(),
                String::new(),
                "three".to_string(),
                String::new()
            ]
        );
    }

    #[test]
    fn input_editor_inserts_and_deletes_at_cursor() {
        let config = AppConfig::default();
        let mut app = ConfigApp::new(&config);
        app.begin_input(InputMode::DnsNameserver, "one\nthree", "edit");
        app.input_cursor = 4;

        insert_input_text(&mut app, "two\n");
        assert_eq!(app.input, "one\ntwo\nthree");
        assert_eq!(app.input_cursor, "one\ntwo\n".len());

        move_input_cursor_left(&mut app);
        assert_eq!(app.input_cursor, "one\ntwo".len());
        delete_input_char_before_cursor(&mut app);
        assert_eq!(app.input, "one\ntw\nthree");

        delete_input_char_at_cursor(&mut app);
        assert_eq!(app.input, "one\ntwthree");
    }

    #[test]
    fn input_editor_moves_between_lines_by_column() {
        let config = AppConfig::default();
        let mut app = ConfigApp::new(&config);
        app.begin_input(InputMode::DnsNameserver, "abcd\nef\nxyz", "edit");
        app.input_cursor = 3;

        move_input_cursor_line(&mut app, 1);
        assert_eq!(app.input_cursor, "abcd\nef".len());

        move_input_cursor_line(&mut app, 1);
        assert_eq!(app.input_cursor, "abcd\nef\nxyz".len());

        move_input_cursor_line(&mut app, -1);
        assert_eq!(app.input_cursor, "abcd\nef".len());
    }

    #[test]
    fn number_input_inserts_digits_at_cursor() {
        let mut value = "80".to_string();
        let mut cursor = 1;

        push_number_digit(&mut value, &mut cursor, '8');
        assert_eq!(value, "880");
        assert_eq!(cursor, 2);

        push_number_digit(&mut value, &mut cursor, '8');
        assert_eq!(value, "8880");
        assert_eq!(cursor, 3);

        push_number_digit(&mut value, &mut cursor, '8');
        assert_eq!(value, "8880");
        assert_eq!(cursor, 3);

        adjust_number_input(&mut value, &mut cursor, 1);
        assert_eq!(value, "8881");
        assert_eq!(cursor, 4);
    }

    #[tokio::test]
    async fn input_enter_edits_multiline_and_save_button_submits() -> Result<()> {
        let paths = test_paths("input-editor-save-button")?;
        let mut config = AppConfig::default();
        let mut app = ConfigApp::new(&config);
        app.begin_input(InputMode::DnsLanDomains, "one", "edit");

        handle_input_key(
            &paths,
            &mut config,
            &mut app,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()),
        )
        .await?;
        assert_eq!(app.input, "one\n");
        assert_eq!(app.input_mode, InputMode::DnsLanDomains);

        handle_input_key(
            &paths,
            &mut config,
            &mut app,
            KeyEvent::new(KeyCode::Char('t'), KeyModifiers::empty()),
        )
        .await?;
        handle_input_key(
            &paths,
            &mut config,
            &mut app,
            KeyEvent::new(KeyCode::Tab, KeyModifiers::empty()),
        )
        .await?;
        assert_eq!(app.input_focus, InputFocus::Save);

        handle_input_key(
            &paths,
            &mut config,
            &mut app,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()),
        )
        .await?;

        assert_eq!(
            config.dns.lan_domains,
            vec!["one".to_string(), "t".to_string()]
        );
        assert_eq!(app.input_mode, InputMode::Normal);
        Ok(())
    }

    #[test]
    fn input_editor_lines_keep_cursor_visible() {
        let lines = input_editor_lines("one\ntwo\nthree", "one\ntwo\nthr".len(), 8, 2);
        let rendered = lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>();

        assert_eq!(rendered, vec!["…two".to_string(), "three".to_string()]);
        assert!(
            lines[1]
                .spans
                .iter()
                .any(|span| span.style.bg == Some(Color::White))
        );
    }

    #[test]
    fn dns_policy_formats_for_multiline_editing() {
        let mut policy = BTreeMap::new();
        policy.insert("+.example.com".into(), vec!["1.1.1.1".into()]);
        policy.insert("+.taobao.net".into(), vec!["30.30.30.30".into()]);

        assert_eq!(
            format_multiline_nameserver_policy(&policy),
            "+.example.com=1.1.1.1\n+.taobao.net=30.30.30.30"
        );
    }

    #[test]
    fn traffic_speed_uses_traffic_endpoint_not_connection_totals() -> Result<()> {
        let connections = serde_json::json!({
            "uploadTotal": 1024,
            "downloadTotal": 2048,
            "connections": [
                { "upload": 50_000_000, "download": 60_000_000 }
            ]
        });
        let traffic = serde_json::json!({
            "up": 300_000,
            "down": 400_000,
            "upTotal": 4096,
            "downTotal": 8192
        });

        let state =
            traffic_from_values(Some(&connections), Some(&traffic)).context("traffic missing")?;

        assert_eq!(state.upload_total, Some(4096));
        assert_eq!(state.download_total, Some(8192));
        assert_eq!(state.upload_speed, Some(300_000));
        assert_eq!(state.download_speed, Some(400_000));
        Ok(())
    }

    #[test]
    fn traffic_speed_is_unknown_without_traffic_endpoint() -> Result<()> {
        let connections = serde_json::json!({
            "uploadTotal": 1024,
            "downloadTotal": 2048,
            "connections": [
                { "upload": 50_000_000, "download": 60_000_000 }
            ]
        });

        let state = traffic_from_values(Some(&connections), None).context("traffic missing")?;

        assert_eq!(state.upload_total, Some(1024));
        assert_eq!(state.download_total, Some(2048));
        assert_eq!(state.upload_speed, None);
        assert_eq!(state.download_speed, None);
        assert_eq!(traffic_speed_summary(Some(&state)), "↑-/s ↓-/s");
        Ok(())
    }

    #[test]
    fn traffic_total_summary_adds_upload_and_download() {
        let traffic = TrafficState {
            upload_total: Some(38_168_166),
            download_total: Some(322_122_547),
            ..TrafficState::default()
        };

        assert_eq!(traffic_total_summary(Some(&traffic)), "0.3GB");
    }

    #[test]
    fn connection_state_counts_active_connections() -> Result<()> {
        let connections = serde_json::json!({
            "uploadTotal": 1024,
            "downloadTotal": 2048,
            "connections": [{ "id": "a" }, { "id": "b" }]
        });

        let state = connection_state_from_value(&connections).context("connections missing")?;

        assert_eq!(state.active, 2);
        Ok(())
    }

    #[test]
    fn traffic_parser_accepts_mihomo_signed_integer_fields() -> Result<()> {
        let traffic = serde_json::json!({
            "up": 0_i64,
            "down": 1_i64,
            "upTotal": 2_i64,
            "downTotal": 3_i64
        });

        let state = traffic_from_values(None, Some(&traffic)).context("traffic missing")?;

        assert_eq!(state.upload_speed, Some(0));
        assert_eq!(state.download_speed, Some(1));
        assert_eq!(state.upload_total, Some(2));
        assert_eq!(state.download_total, Some(3));
        Ok(())
    }

    #[test]
    fn main_proxy_rows_are_mihomo_instances_only() {
        let mut config = AppConfig::default();
        config.proxy_ports.http = Some(18080);
        config.proxy_ports.socks = Some(18081);
        config.proxy_ports.services.push(PortProxyService {
            name: "Port Proxy 1".into(),
            kind: "mixed".into(),
            listen: "127.0.0.1".into(),
            port: 18082,
            ..PortProxyService::default()
        });

        let rows = main_proxy_rows(&config);
        let names = rows.iter().map(|row| row.name.as_str()).collect::<Vec<_>>();
        assert_eq!(
            names,
            vec!["Global Proxy", "Port Proxy 1", "Add Port Proxy"]
        );
        assert_eq!(rows[0].listener, "MIX");
        assert_eq!(rows[1].listener, "MIX");
        assert!(!main_proxy_settings_line(&config, &rows[1]).contains("udp="));
        assert_eq!(rows[1].udp, Some(true));
        assert_eq!(service_index_for_main_proxy(&config, 1), Some(0));
        assert_eq!(main_proxy_index_for_service(&config, 0), Some(1));
    }

    #[test]
    fn port_proxy_config_rows_include_delete_action() -> Result<()> {
        let mut config = AppConfig::default();
        config.proxy_ports.services.push(PortProxyService {
            name: "Port Proxy 1".into(),
            ..PortProxyService::default()
        });
        let mut app = ConfigApp::new(&config);

        let system_rows = proxy_config_rows(&config, &app);
        assert!(!system_rows.iter().any(|row| row.label == "Delete"));
        assert!(!system_rows.iter().any(|row| row.label == "DNS"));

        app.selected_main = 1;
        let rows = proxy_config_rows(&config, &app);
        assert!(!rows.iter().any(|row| row.label == "DNS"));
        let delete = rows
            .iter()
            .find(|row| row.label == "Delete")
            .context("delete action")?;
        assert_eq!(delete.value, "confirm");
        assert!(matches!(
            delete.kind,
            RowKind::Action(ActionKind::DeletePortProxy)
        ));
        Ok(())
    }

    #[test]
    fn runtime_rows_do_not_include_dns_shortcut() -> Result<()> {
        let paths = test_paths("runtime-rows")?;
        let config = AppConfig::default();
        let app = ConfigApp::new(&config);
        let rows = runtime_rows(&paths, &config, &app);

        assert!(!rows.iter().any(|row| row.label == "DNS"));
        let llm_index = rows
            .iter()
            .position(|row| row.label == "LLM")
            .context("LLM section row")?;
        let refresh_index = rows
            .iter()
            .position(|row| row.label == "Refresh Runtime")
            .context("Refresh Runtime row")?;
        let provider_index = rows
            .iter()
            .position(|row| row.label == "LLM Provider")
            .context("LLM Provider row")?;

        assert!(refresh_index < llm_index);
        assert!(llm_index < provider_index);
        assert!(matches!(rows[llm_index].kind, RowKind::SoftSection));
        Ok(())
    }

    #[test]
    fn runtime_navigation_skips_llm_section_header() -> Result<()> {
        let paths = test_paths("runtime-skip-llm-section")?;
        let config = AppConfig::default();
        let mut app = ConfigApp::new(&config);
        app.page = Page::Runtime;
        app.selected_runtime = RuntimeItem::ALL
            .iter()
            .position(|item| *item == RuntimeItem::Refresh)
            .context("Refresh Runtime item")?;

        app.move_next(&paths, &config);
        assert_eq!(app.selected_runtime_item(), RuntimeItem::LlmProvider);
        assert_ne!(app.selected_runtime_item(), RuntimeItem::LlmSection);

        app.move_prev(&paths, &config);
        assert_eq!(app.selected_runtime_item(), RuntimeItem::Refresh);
        Ok(())
    }

    #[tokio::test]
    async fn delete_port_proxy_requires_confirm_then_removes_selected_service() -> Result<()> {
        let paths = test_paths("delete-port-proxy")?;
        let mut config = AppConfig::default();
        config.proxy_ports.services.push(PortProxyService {
            name: "Port Proxy 1".into(),
            port: 17071,
            ..PortProxyService::default()
        });
        config.proxy_ports.services.push(PortProxyService {
            name: "Port Proxy 2".into(),
            port: 17072,
            ..PortProxyService::default()
        });
        let mut app = ConfigApp::new(&config);
        app.page = Page::ProxyConfig;
        app.selected_main = 1;
        app.selected_proxy_field = ProxyConfigField::PORT_ALL
            .iter()
            .position(|field| *field == ProxyConfigField::Delete)
            .context("delete field")?;

        submit_proxy_config_item(&paths, &mut config, &mut app).await?;
        assert_eq!(app.confirm, Some(ConfirmAction::DeletePortProxy));
        assert_eq!(config.proxy_ports.services.len(), 2);

        app.confirm_yes = true;
        submit_confirm(&paths, &mut config, &mut app).await?;

        assert_eq!(config.proxy_ports.services.len(), 1);
        assert_eq!(
            config
                .proxy_ports
                .services
                .first()
                .map(|service| service.name.as_str()),
            Some("Port Proxy 2")
        );
        assert_eq!(app.page, Page::Main);
        assert_eq!(app.selected_main, 1);
        assert!(app.status.contains("Deleted Port Proxy 1"));
        assert!(app.dirty);
        Ok(())
    }

    #[test]
    fn selected_port_proxy_uses_its_mihomo_runtime_groups() {
        let mut config = AppConfig::default();
        config.proxy_ports.services.push(PortProxyService {
            name: "Port Proxy 1".into(),
            mode: "rule".into(),
            ..PortProxyService::default()
        });
        let mut app = ConfigApp::new(&config);
        app.selected_main = 1;
        app.runtime.groups = vec![ProxyGroup {
            name: "Global Group".into(),
            kind: "select".into(),
            now: "Global Node".into(),
            all: vec!["Global Node".into()],
        }];
        app.proxy_runtimes.insert(
            service_runtime_key(0),
            RuntimeState {
                groups: vec![ProxyGroup {
                    name: "Service Group".into(),
                    kind: "select".into(),
                    now: "Service Node".into(),
                    all: vec!["Service Node".into()],
                }],
                ..RuntimeState::default()
            },
        );

        assert_eq!(
            app.current_group(&config).map(|group| group.name.as_str()),
            Some("Service Group")
        );
        assert_eq!(
            proxy_server_value(&config, &app),
            "Service Group -> Service Node"
        );
    }

    #[test]
    fn single_runtime_port_proxy_uses_global_connections_and_process() -> Result<()> {
        let mut config = AppConfig::default();
        let service = PortProxyService {
            name: "Port Proxy 1".into(),
            port: 7071,
            ..PortProxyService::default()
        };
        let inbound = runtime_profile::single_runtime_listener_name(0, &service);
        config.proxy_ports.services.push(service);

        let global_runtime = RuntimeState {
            connections: Some(ConnectionState {
                active: 2,
                items: vec![
                    ConnectionInfo {
                        inbound,
                        upload: 1024,
                        download: 2048,
                        ..ConnectionInfo::default()
                    },
                    ConnectionInfo {
                        inbound: "mixed".into(),
                        upload: 4096,
                        download: 8192,
                        ..ConnectionInfo::default()
                    },
                ],
            }),
            process: ProcessState {
                pid: Some(42),
                running: true,
                ..ProcessState::default()
            },
            ..RuntimeState::default()
        };

        let runtimes =
            build_proxy_runtimes(&config, &global_runtime, BTreeMap::new(), BTreeMap::new());
        let runtime = runtimes
            .get(&service_runtime_key(0))
            .context("service runtime missing")?;
        let connections = runtime
            .connections
            .as_ref()
            .context("connections missing")?;
        let traffic = runtime.traffic.as_ref().context("traffic missing")?;

        assert_eq!(connections.active, 1);
        assert_eq!(traffic.upload_total, Some(1024));
        assert_eq!(traffic.download_total, Some(2048));
        assert_eq!(runtime.process.pid, Some(42));
        Ok(())
    }

    #[test]
    fn runtime_refresh_without_ip_keeps_previous_ip_info() {
        let mut config = AppConfig::default();
        config.proxy_ports.services.push(PortProxyService {
            name: "Port Proxy 1".into(),
            ..PortProxyService::default()
        });
        let mut app = ConfigApp::new(&config);
        app.runtime.ip_info = Some(IpInfoState {
            ip: "198.51.100.1".into(),
            country: Some("US".into()),
            city: Some("Los Angeles".into()),
        });
        app.proxy_runtimes.insert(
            "global".into(),
            RuntimeState {
                ip_info: Some(IpInfoState {
                    ip: "203.0.113.1".into(),
                    country: Some("JP".into()),
                    city: Some("Tokyo".into()),
                }),
                ..RuntimeState::default()
            },
        );
        app.proxy_runtimes.insert(
            service_runtime_key(0),
            RuntimeState {
                ip_info: Some(IpInfoState {
                    ip: "203.0.113.2".into(),
                    country: Some("SG".into()),
                    city: Some("Singapore".into()),
                }),
                ..RuntimeState::default()
            },
        );

        app.apply_runtime_refresh(
            &config,
            RuntimeRefreshResult {
                runtime: RuntimeState::default(),
                proxy_runtimes: BTreeMap::from([
                    ("global".into(), RuntimeState::default()),
                    (service_runtime_key(0), RuntimeState::default()),
                ]),
                refreshed_ip_info: false,
            },
        );

        assert_eq!(
            app.runtime.ip_info.as_ref().map(|info| info.ip.as_str()),
            Some("198.51.100.1")
        );
        assert_eq!(
            app.proxy_runtimes
                .get("global")
                .and_then(|runtime| runtime.ip_info.as_ref())
                .map(|info| info.ip.as_str()),
            Some("203.0.113.1")
        );
        assert_eq!(
            app.proxy_runtimes
                .get(&service_runtime_key(0))
                .and_then(|runtime| runtime.ip_info.as_ref())
                .map(|info| info.ip.as_str()),
            Some("203.0.113.2")
        );
    }

    #[test]
    fn runtime_refresh_preserves_proxy_group_selection_when_groups_are_temporarily_empty() {
        let config = AppConfig::default();
        let mut app = ConfigApp::new(&config);
        app.page = Page::ProxyGroups;
        app.proxy_pane = ProxyPane::Proxies;
        app.runtime.groups = vec![
            ProxyGroup {
                name: "Auto".into(),
                kind: "select".into(),
                now: "HK 01".into(),
                all: vec!["HK 01".into(), "JP 01".into()],
            },
            ProxyGroup {
                name: "Media".into(),
                kind: "select".into(),
                now: "US 01".into(),
                all: vec!["US 01".into(), "US 02".into()],
            },
        ];
        app.selected_group = 1;
        app.selected_proxy = 1;

        app.apply_runtime_refresh(
            &config,
            RuntimeRefreshResult {
                runtime: RuntimeState::default(),
                proxy_runtimes: BTreeMap::new(),
                refreshed_ip_info: false,
            },
        );

        assert_eq!(app.selected_group, 1);
        assert_eq!(app.selected_proxy, 1);
    }

    #[test]
    fn runtime_refresh_restores_proxy_group_selection_by_name_after_reorder() {
        let config = AppConfig::default();
        let mut app = ConfigApp::new(&config);
        app.page = Page::ProxyGroups;
        app.proxy_pane = ProxyPane::Proxies;
        app.runtime.groups = vec![
            ProxyGroup {
                name: "Auto".into(),
                kind: "select".into(),
                now: "HK 01".into(),
                all: vec!["HK 01".into()],
            },
            ProxyGroup {
                name: "Media".into(),
                kind: "select".into(),
                now: "US 01".into(),
                all: vec!["US 01".into(), "US 02".into()],
            },
        ];
        app.selected_group = 1;
        app.selected_proxy = 1;

        app.apply_runtime_refresh(
            &config,
            RuntimeRefreshResult {
                runtime: RuntimeState {
                    groups: vec![
                        ProxyGroup {
                            name: "Media".into(),
                            kind: "select".into(),
                            now: "US 01".into(),
                            all: vec!["US 02".into(), "US 01".into()],
                        },
                        ProxyGroup {
                            name: "Auto".into(),
                            kind: "select".into(),
                            now: "HK 01".into(),
                            all: vec!["HK 01".into()],
                        },
                    ],
                    ..RuntimeState::default()
                },
                proxy_runtimes: BTreeMap::new(),
                refreshed_ip_info: false,
            },
        );

        assert_eq!(app.selected_group, 0);
        assert_eq!(app.selected_proxy, 0);
    }

    #[test]
    fn runtime_refresh_does_not_reset_global_proxy_list_selection() {
        let mut config = AppConfig {
            runtime_mode: "global".into(),
            ..AppConfig::default()
        };
        config
            .proxy_selections
            .insert("GLOBAL".into(), "US 02".into());
        let mut app = ConfigApp::new(&config);
        app.page = Page::ProxyGroups;
        app.proxy_pane = ProxyPane::Proxies;
        app.selected_proxy = 2;

        app.apply_runtime_refresh(
            &config,
            RuntimeRefreshResult {
                runtime: RuntimeState::default(),
                proxy_runtimes: BTreeMap::new(),
                refreshed_ip_info: false,
            },
        );

        assert_eq!(app.selected_proxy, 2);
    }

    #[test]
    fn runtime_refresh_does_not_reset_subscription_rule_proxy_selection() {
        let config = AppConfig::default();
        let mut app = ConfigApp::new(&config);
        app.page = Page::ProxyGroups;
        app.proxy_pane = ProxyPane::Proxies;
        app.rule_group_selection = Some(RuleGroupSelection {
            subscription_index: 0,
            group_name: "Auto".into(),
        });
        app.selected_proxy = 2;

        app.apply_runtime_refresh(
            &config,
            RuntimeRefreshResult {
                runtime: RuntimeState::default(),
                proxy_runtimes: BTreeMap::new(),
                refreshed_ip_info: false,
            },
        );

        assert_eq!(app.selected_proxy, 2);
    }

    #[test]
    fn clamp_selection_does_not_reset_proxy_selection_outside_proxy_groups() -> Result<()> {
        let paths = test_paths("clamp-selection-off-page")?;
        let config = AppConfig::default();
        let mut app = ConfigApp::new(&config);
        app.page = Page::Subscription;
        app.selected_group = 7;
        app.selected_proxy = 5;

        app.clamp_selection(&paths, &config);

        assert_eq!(app.selected_group, 7);
        assert_eq!(app.selected_proxy, 5);
        Ok(())
    }

    #[test]
    fn failed_ip_info_uses_short_retry_interval() {
        let config = AppConfig::default();
        let mut app = ConfigApp::new(&config);
        app.proxy_runtimes.insert(
            "global".into(),
            RuntimeState {
                ip_info_error: Some("proxy listener unavailable".into()),
                ..RuntimeState::default()
            },
        );

        app.last_ip_info_refresh =
            Some(Instant::now() - (IPINFO_RETRY_INTERVAL - Duration::from_secs(1)));
        assert!(!app.should_refresh_ip_info());

        app.last_ip_info_refresh =
            Some(Instant::now() - (IPINFO_RETRY_INTERVAL + Duration::from_secs(1)));
        assert!(app.should_refresh_ip_info());
    }

    #[test]
    fn successful_ip_info_uses_normal_refresh_interval() {
        let config = AppConfig::default();
        let mut app = ConfigApp::new(&config);
        app.runtime.ip_info = Some(IpInfoState {
            ip: "198.51.100.1".into(),
            country: Some("US".into()),
            city: Some("Los Angeles".into()),
        });

        app.last_ip_info_refresh =
            Some(Instant::now() - (IPINFO_RETRY_INTERVAL + Duration::from_secs(1)));
        assert!(!app.should_refresh_ip_info());

        app.last_ip_info_refresh =
            Some(Instant::now() - (IPINFO_REFRESH_INTERVAL + Duration::from_secs(1)));
        assert!(app.should_refresh_ip_info());
    }

    #[test]
    fn subscription_profile_groups_and_rules_are_listed() {
        let profile = r"
proxy-groups:
  - name: Auto
    type: select
    proxies:
      - HK 01
      - JP 01
rule-providers:
  reject:
    type: http
    behavior: domain
    url: https://example.invalid/reject.yaml
rules:
  - DOMAIN-SUFFIX,example.com,Auto
  - MATCH,DIRECT
";

        let groups = subscription_profile_proxy_groups_from_str(profile);
        assert_eq!(groups.len(), 1);
        assert_eq!(
            groups.first().map(|group| group.name.as_str()),
            Some("Auto")
        );
        assert_eq!(
            groups.first().map(|group| group.now.as_str()),
            Some("HK 01")
        );
        assert_eq!(
            groups.first().map(|group| group.all.as_slice()),
            Some(["HK 01".to_string(), "JP 01".to_string()].as_slice())
        );

        let rows = subscription_profile_rule_rows_from_str(profile);
        assert_eq!(rows.len(), 3);
        assert_eq!(
            rows.first().map(|row| row.label.as_str()),
            Some("Provider reject")
        );
        assert_eq!(
            rows.get(1).map(|row| row.value.as_str()),
            Some("DOMAIN-SUFFIX,example.com,Auto")
        );
        assert_eq!(
            rows.get(2).map(|row| row.value.as_str()),
            Some("MATCH,DIRECT")
        );
    }

    #[test]
    fn proxy_delay_rows_use_status_colors() {
        let mut row = SettingRow {
            label: "Proxy".into(),
            value: "120ms".into(),
            help: String::new(),
            kind: RowKind::Action(ActionKind::TestProxyDelay),
        };
        assert_eq!(row_style(&row, false).fg, Some(Color::White));
        assert_eq!(row_value_style(&row, false).fg, Some(Color::Green));

        row.value = "1200ms".into();
        assert_eq!(row_value_style(&row, false).fg, Some(Color::Yellow));

        row.value = "120ms selected".into();
        assert_eq!(row_value_style(&row, false).fg, Some(Color::Green));

        row.value = "fail".into();
        assert_eq!(row_value_style(&row, false).fg, Some(Color::Red));

        row.kind = RowKind::Action(ActionKind::SelectProxy);
        row.value = "120ms".into();
        assert_eq!(row_value_style(&row, false).fg, Some(Color::Green));

        row.value = "fail selected".into();
        assert_eq!(row_value_style(&row, false).fg, Some(Color::Red));

        row.kind = RowKind::Action(ActionKind::TestProxyDelay);
        row.value = "Check".into();
        assert_eq!(row_value_style(&row, false).fg, Some(Color::White));

        row.kind = RowKind::Action(ActionKind::TestAllProxyDelays);
        assert_eq!(row_style(&row, false).fg, Some(Color::Yellow));
    }

    #[test]
    fn row_palette_keeps_choices_white_actions_yellow_and_readonly_gray() {
        let selectable = SettingRow {
            label: "Subscription".into(),
            value: "used 1.0MB".into(),
            help: String::new(),
            kind: RowKind::Submenu(Page::SubscriptionDetail),
        };
        assert_eq!(row_style(&selectable, false).fg, Some(Color::White));

        let action = SettingRow {
            label: "Update Now".into(),
            value: "manual".into(),
            help: String::new(),
            kind: RowKind::Action(ActionKind::UpdateSubscription),
        };
        assert_eq!(row_style(&action, false).fg, Some(Color::Yellow));

        let readonly = SettingRow {
            label: "Overview".into(),
            value: "-".into(),
            help: String::new(),
            kind: RowKind::Info,
        };
        assert_eq!(row_style(&readonly, false).fg, Some(Color::Gray));
    }

    #[test]
    fn row_palette_uses_semantic_status_colors() {
        let mut row = SettingRow {
            label: "State".into(),
            value: "ok".into(),
            help: String::new(),
            kind: RowKind::Info,
        };
        assert_eq!(row_style(&row, false).fg, Some(Color::Gray));
        assert_eq!(row_value_style(&row, false).fg, Some(Color::Green));

        row.value = "slow".into();
        assert_eq!(row_value_style(&row, false).fg, Some(Color::Yellow));

        row.value = "fail".into();
        assert_eq!(row_value_style(&row, false).fg, Some(Color::Red));

        row.value = "selected".into();
        assert_eq!(row_value_style(&row, false).fg, Some(Color::Cyan));
    }

    #[test]
    fn subscription_rule_groups_are_submenus() -> Result<()> {
        let paths = test_paths("rule-groups")?;
        let subscription = Subscription {
            name: "demo".into(),
            url: "https://example.invalid/demo.yaml".into(),
            ..Subscription::default()
        };
        std::fs::write(
            subscription::profile_path(&paths, &subscription),
            r"
proxy-groups:
  - name: Auto
    type: select
    proxies:
      - HK 01
      - JP 01
rules:
  - DOMAIN-SUFFIX,example.com,Auto
",
        )?;
        let mut config = AppConfig::default();
        config.subscriptions.push(subscription);
        let mut app = ConfigApp::new(&config);
        app.subscription_profiles = fetch_subscription_profile_cache(&paths, &config);

        let rows = subscription_rule_group_rows(&paths, &config, &app);
        assert_eq!(rows.first().map(|row| row.label.as_str()), Some("Auto"));
        assert!(matches!(
            rows.first().map(|row| row.kind),
            Some(RowKind::Submenu(Page::ProxyGroups))
        ));
        assert!(
            rows.first()
                .is_some_and(|row| row.value.contains("current"))
        );
        Ok(())
    }

    #[test]
    fn subscription_proxy_rows_keep_unchecked_state_as_check() -> Result<()> {
        let paths = test_paths("proxy-check")?;
        let mut subscription = Subscription {
            name: "demo".into(),
            url: "https://example.invalid/demo.yaml".into(),
            ..Subscription::default()
        };
        subscription
            .rule_selections
            .insert("Auto".into(), "JP 01".into());
        std::fs::write(
            subscription::profile_path(&paths, &subscription),
            r"
proxies:
  - name: HK 01
    type: socks5
    server: 127.0.0.1
    port: 1080
  - name: JP 01
    type: socks5
    server: 127.0.0.1
    port: 1081
",
        )?;
        let mut config = AppConfig::default();
        config.subscriptions.push(subscription);
        let mut app = ConfigApp::new(&config);
        app.subscription_profiles = fetch_subscription_profile_cache(&paths, &config);
        app.proxy_delays.insert(
            subscription_proxy_delay_key_for_name("demo", "HK 01"),
            "120ms".into(),
        );

        let rows = subscription_proxy_rows(&paths, &config, &app);
        assert_eq!(rows.first().map(|row| row.value.as_str()), Some("120ms"));
        assert_eq!(rows.get(1).map(|row| row.label.as_str()), Some("* JP 01"));
        assert_eq!(rows.get(1).map(|row| row.value.as_str()), Some("selected"));
        assert_eq!(
            rows.get(1).map(|row| row_value_style(row, false).fg),
            Some(Some(Color::Cyan))
        );
        Ok(())
    }

    #[test]
    fn proxy_group_selection_rows_show_check_or_delay_not_select() -> Result<()> {
        let paths = test_paths("proxy-group-delay-values")?;
        let subscription = Subscription {
            name: "demo".into(),
            url: "https://example.invalid/demo.yaml".into(),
            ..Subscription::default()
        };
        std::fs::write(
            subscription::profile_path(&paths, &subscription),
            r"
proxies:
  - name: HK 01
    type: socks5
    server: 127.0.0.1
    port: 1080
  - name: JP 01
    type: socks5
    server: 127.0.0.1
    port: 1081
  - name: US 01
    type: socks5
    server: 127.0.0.1
    port: 1082
",
        )?;
        let mut config = AppConfig::default();
        config.subscriptions.push(subscription);
        config.proxy_profiles[0].subscription = Some("demo".into());
        config.proxy_profiles[0].mode = "global".into();
        config.proxy_profiles[0].proxy = Some("HK 01".into());

        let mut app = ConfigApp::new(&config);
        app.page = Page::ProxyGroups;
        app.history.push(Location {
            section: Page::Profile,
            page: Page::ProfileConfig,
            selected: 0,
        });
        app.subscription_profiles = fetch_subscription_profile_cache(&paths, &config);
        app.proxy_delays.insert(
            subscription_proxy_delay_key_for_name("demo", "HK 01"),
            "80ms".into(),
        );
        app.proxy_delays.insert(
            subscription_proxy_delay_key_for_name("demo", "JP 01"),
            "120ms".into(),
        );

        let rows = proxy_group_rows(&paths, &config, &app);

        assert_eq!(
            rows.first().map(|row| row.value.as_str()),
            Some("80ms selected")
        );
        assert_eq!(rows.get(1).map(|row| row.value.as_str()), Some("120ms"));
        assert_eq!(rows.get(2).map(|row| row.value.as_str()), Some("Check"));
        assert!(!rows.iter().any(|row| row.value == "select"));
        Ok(())
    }

    #[test]
    fn subscription_detail_rows_expose_direct_settings_without_edit() -> Result<()> {
        let paths = test_paths("subscription-settings")?;
        let mut config = AppConfig::default();
        config.subscriptions.push(Subscription {
            name: "demo".into(),
            url: "https://example.invalid/demo.yaml".into(),
            refresh: SubscriptionRefresh::Weekly,
            ..Subscription::default()
        });
        let app = ConfigApp::new(&config);

        let rows = subscription_detail_rows(&paths, &config, &app);

        assert_eq!(rows.len(), SUBSCRIPTION_DETAIL_COUNT);
        assert_eq!(rows.first().map(|row| row.label.as_str()), Some("Overview"));
        assert!(!rows.first().is_some_and(|row| row.value.contains("ok")));
        assert!(
            !rows
                .first()
                .is_some_and(|row| row.value.contains("groups="))
        );
        assert!(!rows.first().is_some_and(|row| row.value.contains("rules=")));
        assert_eq!(rows.get(1).map(|row| row.label.as_str()), Some("URL"));
        assert!(matches!(
            rows.get(1).map(|row| row.kind),
            Some(RowKind::Input(InputMode::SubscriptionUrl))
        ));
        assert_eq!(rows.get(2).map(|row| row.label.as_str()), Some("Refresh"));
        assert!(matches!(
            rows.get(2).map(|row| row.kind),
            Some(RowKind::Choice(ChoiceAction::SubscriptionRefresh))
        ));
        assert!(!rows.iter().any(|row| row.label == "Edit"));
        Ok(())
    }

    #[tokio::test]
    async fn subscription_detail_settings_do_not_switch_main_profile() -> Result<()> {
        let paths = test_paths("subscription-setting-save")?;
        let mut config = AppConfig {
            active_profile: Some("oist".into()),
            ..AppConfig::default()
        };
        config.subscriptions.push(Subscription {
            name: "oist".into(),
            url: "https://example.invalid/oist.yaml".into(),
            ..Subscription::default()
        });
        config.subscriptions.push(Subscription {
            name: "amy".into(),
            url: "https://example.invalid/old.yaml".into(),
            refresh: SubscriptionRefresh::Daily,
            ..Subscription::default()
        });
        let mut app = ConfigApp::new(&config);
        app.page = Page::SubscriptionDetail;
        app.selected_subscription = 1;

        app.input_mode = InputMode::SubscriptionUrl;
        app.input = "https://example.invalid/new.yaml".into();
        submit_input(&paths, &mut config, &mut app).await?;

        assert_eq!(config.active_profile.as_deref(), Some("oist"));
        assert_eq!(
            config.subscriptions.get(1).map(|sub| sub.url.as_str()),
            Some("https://example.invalid/new.yaml")
        );

        app.selected_dropdown = subscription_refresh_index(SubscriptionRefresh::Weekly);
        submit_subscription_refresh_dropdown(&paths, &mut config, &mut app).await?;

        assert_eq!(config.active_profile.as_deref(), Some("oist"));
        assert_eq!(
            config.subscriptions.get(1).map(|sub| sub.refresh),
            Some(SubscriptionRefresh::Weekly)
        );
        Ok(())
    }

    #[test]
    fn inactive_subscription_delay_check_provider_does_not_switch_main_profile() {
        let mut config = AppConfig {
            active_profile: Some("oist".into()),
            ..AppConfig::default()
        };
        config.subscriptions.push(Subscription {
            name: "oist".into(),
            ..Subscription::default()
        });
        config.subscriptions.push(Subscription {
            name: "amy".into(),
            ..Subscription::default()
        });
        let mut app = ConfigApp::new(&config);
        app.selected_subscription = 1;

        let provider_name = config
            .subscriptions
            .get(1)
            .map(runtime_profile::subscription_check_provider_name)
            .expect("subscription exists");

        assert_eq!(config.active_profile.as_deref(), Some("oist"));
        assert!(provider_name.starts_with("__clashtui_check_"));
        assert!(app.alert.is_none());
    }

    fn test_paths(name: &str) -> Result<Paths> {
        let root = std::env::temp_dir().join(format!(
            "clashtui-config-menu-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let profiles_dir = root.join("profiles");
        std::fs::create_dir_all(&profiles_dir)?;
        Ok(Paths {
            config_dir: root.clone(),
            config_file: root.join("config.yaml"),
            pid_file: root.join("clashtui.pid"),
            core_pid_file: root.join("mihomo.pid"),
            core_config_file: root.join("mihomo-run.yaml"),
            active_config_file: root.join("mihomo-active.yaml"),
            log_file: root.join("clashtui.log"),
            core_log_file: root.join("mihomo.log"),
            llm_api_key_file: root.join("llm-api-key"),
            llm_providers_file: root.join("llm-providers.yaml"),
            profiles_dir,
            cores_dir: root.join("cores"),
        })
    }

    fn line_text(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>()
    }
}
