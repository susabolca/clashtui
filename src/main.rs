mod config;
mod config_menu;
mod core;
mod daemon;
mod dns;
mod mihomo;
mod platform;
mod privilege;
mod runtime_profile;
mod subscription;
mod system_proxy;
mod tun;

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};

use crate::config::{AppConfig, Paths};
use crate::mihomo::MihomoClient;

#[derive(Parser, Debug)]
#[command(name = "clashtui")]
#[command(about = "A small background controller for mihomo / Clash.Meta")]
#[command(arg_required_else_help = true)]
struct Cli {
    #[arg(long, env = "MIHOMO_CONTROLLER")]
    controller: Option<String>,

    #[arg(long, env = "MIHOMO_SECRET")]
    secret: Option<String>,

    #[arg(long, hide = true)]
    daemon_run: bool,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Start clashtui in the background.
    Start,
    /// Stop the background process and clear runtime hooks.
    Stop,
    /// Show daemon and mihomo status.
    Status,
    /// Open the interactive config menu.
    Config,
    /// Install Linux capabilities needed for TUN/DNS.
    TunInstall {
        /// Override the clashtui binary path to update.
        #[arg(long)]
        path: Option<PathBuf>,
    },
    /// Remove Linux capabilities installed for TUN/DNS.
    TunUninstall {
        /// Override the clashtui binary path to update.
        #[arg(long)]
        path: Option<PathBuf>,
    },
    #[command(name = "__tun-install-root", hide = true)]
    PrivilegedTunInstall {
        #[arg(long)]
        path: PathBuf,
        #[arg(long)]
        user: String,
    },
    #[command(name = "__tun-uninstall-root", hide = true)]
    PrivilegedTunUninstall {
        #[arg(long)]
        path: PathBuf,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match &cli.command {
        Some(Command::TunInstall { path }) => return privilege::tun_install(path.clone()),
        Some(Command::TunUninstall { path }) => return privilege::tun_uninstall(path.clone()),
        Some(Command::PrivilegedTunInstall { path, user }) => {
            return privilege::tun_install_privileged(path.clone(), user.clone());
        }
        Some(Command::PrivilegedTunUninstall { path }) => return privilege::tun_uninstall_privileged(path.clone()),
        _ => {}
    }

    let paths = Paths::new()?;
    let mut config = AppConfig::load_or_init(&paths).await?;

    if let Some(controller) = &cli.controller {
        config.controller.url.clone_from(controller);
    }
    if cli.secret.is_some() {
        config.controller.secret.clone_from(&cli.secret);
    }

    if cli.daemon_run {
        return daemon::run(paths, config, cli.controller, cli.secret).await;
    }

    let client = MihomoClient::new(&config.controller);
    match cli.command {
        Some(Command::Start) => daemon::start(&paths, cli.controller.as_deref(), cli.secret.as_deref()).await,
        Some(Command::Stop) => daemon::stop(&paths, &config, &client).await,
        Some(Command::Status) => daemon::status(&paths, &config, &client).await,
        Some(Command::Config) => config_menu::run(&paths, &mut config).await,
        Some(
            Command::TunInstall { .. }
            | Command::TunUninstall { .. }
            | Command::PrivilegedTunInstall { .. }
            | Command::PrivilegedTunUninstall { .. },
        ) => Ok(()),
        None => Ok(()),
    }
}
