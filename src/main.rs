mod agent;
mod autostart;
mod config;
mod config_menu;
mod core;
mod daemon;
mod dns;
mod i18n;
mod llm;
mod llm_providers;
mod mihomo;
mod platform;
mod port_allocator;
mod runtime_profile;
mod service;
mod subscription;
mod system_proxy;
mod tun;

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};

use crate::config::{AppConfig, Paths};
use crate::i18n::Language;
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

    /// Print detailed paths, network state, and log tails for start/status/stop.
    #[arg(long, global = true)]
    verbose: bool,

    /// UI and assistant language.
    #[arg(short = 'l', long, global = true, value_enum, default_value = "en")]
    language: Language,

    #[arg(long, hide = true)]
    daemon_run: bool,

    #[command(subcommand)]
    command: Option<Command>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_language_defaults_to_english() -> Result<()> {
        let cli = Cli::try_parse_from(["clashtui", "config"])?;

        assert_eq!(cli.language, Language::En);
        Ok(())
    }

    #[test]
    fn cli_accepts_short_zh_language() -> Result<()> {
        let cli = Cli::try_parse_from(["clashtui", "-l", "zh", "config"])?;

        assert_eq!(cli.language, Language::ZhCn);
        Ok(())
    }

    #[test]
    fn cli_accepts_cn_language_alias() -> Result<()> {
        let cli = Cli::try_parse_from(["clashtui", "--language", "cn", "config"])?;

        assert_eq!(cli.language, Language::ZhCn);
        Ok(())
    }

    #[test]
    fn cli_accepts_legacy_zh_cn_language_alias() -> Result<()> {
        let cli = Cli::try_parse_from(["clashtui", "--language", "zh-CN", "config"])?;

        assert_eq!(cli.language, Language::ZhCn);
        Ok(())
    }
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Start clashtui in the background.
    Start,
    /// Stop the background process and clear runtime hooks.
    Stop,
    /// Restart the background process and reapply saved config.
    Restart,
    /// Show daemon and mihomo status.
    Status,
    /// Open the interactive config menu.
    Config,
    /// Install the privileged clashtui service.
    ServiceInstall {
        /// Override the clashtui binary path to install.
        #[arg(long)]
        path: Option<PathBuf>,
    },
    /// Remove the privileged clashtui service.
    ServiceUninstall,
    /// Show privileged service status.
    ServiceStatus,
    #[command(name = "__service-install-root", hide = true)]
    PrivilegedServiceInstall {
        #[arg(long)]
        path: PathBuf,
        #[arg(long)]
        user: String,
    },
    #[command(name = "__service-uninstall-root", hide = true)]
    PrivilegedServiceUninstall,
    #[command(name = "__service-run", hide = true)]
    ServiceRun,
    #[command(name = "__write-single-runtime-config", hide = true)]
    WriteSingleRuntimeConfig,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match &cli.command {
        Some(Command::ServiceInstall { path }) => return service::install(path.clone()),
        Some(Command::ServiceUninstall) => return service::uninstall(),
        Some(Command::ServiceStatus) => return service::print_status(),
        Some(Command::PrivilegedServiceInstall { path, user }) => {
            return service::install_privileged(path.clone(), user.clone());
        }
        Some(Command::PrivilegedServiceUninstall) => return service::uninstall_privileged(),
        Some(Command::ServiceRun) => return service::run(),
        _ => {}
    }

    let paths = Paths::new()?;
    let mut config = AppConfig::load_or_init(&paths).await?;

    let should_allocate_ports = cli.daemon_run || matches!(cli.command, Some(Command::Config));
    if should_allocate_ports && port_allocator::ensure_allocated(&paths, &mut config).await? {
        config.save(&paths).await?;
    }

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
        Some(Command::Start) => {
            daemon::start(
                &paths,
                &mut config,
                cli.controller.as_deref(),
                cli.secret.as_deref(),
                cli.verbose,
            )
            .await
        }
        Some(Command::Stop) => daemon::stop(&paths, &config, &client, cli.verbose).await,
        Some(Command::Restart) => {
            daemon::restart(
                &paths,
                &mut config,
                cli.controller.as_deref(),
                cli.secret.as_deref(),
                cli.verbose,
            )
            .await
        }
        Some(Command::Status) => daemon::status(&paths, &config, &client, cli.verbose).await,
        Some(Command::Config) => config_menu::run(&paths, &mut config, cli.language).await,
        Some(Command::WriteSingleRuntimeConfig) => {
            let path = runtime_profile::write_single_runtime_config(&paths, &config).await?;
            println!("{}", path.display());
            Ok(())
        }
        Some(
            Command::ServiceInstall { .. }
            | Command::ServiceUninstall
            | Command::ServiceStatus
            | Command::PrivilegedServiceInstall { .. }
            | Command::PrivilegedServiceUninstall
            | Command::ServiceRun,
        ) => Ok(()),
        None => Ok(()),
    }
}
