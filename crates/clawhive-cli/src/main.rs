use std::path::PathBuf;

use anyhow::Result;
use clap::{CommandFactory, Parser, Subcommand};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

mod commands;
mod runtime;

use clawhive_core::*;
use commands::auth::{handle_auth_command, AuthCommands};
use commands::setup::run_setup;
use runtime::bootstrap::resolve_security_override;

/// Default HTTP API server port.
const DEFAULT_PORT: u16 = 8848;

#[derive(Parser)]
#[command(name = "clawhive", version, about = "clawhive AI agent framework")]
struct Cli {
    #[arg(
        long,
        default_value = "~/.clawhive",
        help = "Config root directory (contains config/ and prompts/)"
    )]
    config_root: PathBuf,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    #[command(about = "Start all configured channel bots and HTTP API server")]
    Start {
        #[arg(long, short = 'd', help = "Run as a background daemon")]
        daemon: bool,
        #[arg(long, help = "Run TUI dashboard in the same process")]
        tui: bool,
        #[arg(long, default_value_t = DEFAULT_PORT, help = "HTTP API server port")]
        port: u16,
        /// Override security mode (overrides agent config)
        #[arg(long, value_name = "MODE")]
        security: Option<SecurityMode>,
        /// Shorthand for --security off
        #[arg(long)]
        no_security: bool,
    },
    #[command(about = "Start clawhive as a background daemon (alias for `start -d`)")]
    Up {
        #[arg(long, default_value_t = DEFAULT_PORT, help = "HTTP API server port")]
        port: u16,
        /// Override security mode (overrides agent config)
        #[arg(long, value_name = "MODE")]
        security: Option<SecurityMode>,
        /// Shorthand for --security off
        #[arg(long)]
        no_security: bool,
    },
    #[command(about = "Show clawhive status")]
    Status,
    #[command(about = "Stop a running clawhive process")]
    Stop,
    #[command(about = "Restart clawhive (stop + start as daemon)")]
    Restart {
        #[arg(long, default_value_t = DEFAULT_PORT, help = "HTTP API server port")]
        port: u16,
        /// Override security mode (overrides agent config)
        #[arg(long, value_name = "MODE")]
        security: Option<SecurityMode>,
        /// Shorthand for --security off
        #[arg(long)]
        no_security: bool,
    },
    #[command(about = "Code mode: open developer TUI")]
    Code {
        #[arg(long, default_value_t = DEFAULT_PORT, help = "HTTP API server port")]
        port: u16,
        /// Override security mode (overrides agent config)
        #[arg(long, value_name = "MODE")]
        security: Option<SecurityMode>,
        /// Shorthand for --security off
        #[arg(long)]
        no_security: bool,
    },
    #[command(about = "Dashboard mode: attach TUI observability panel to running gateway")]
    Dashboard {
        #[arg(long, default_value_t = DEFAULT_PORT, help = "HTTP API server port")]
        port: u16,
    },
    #[command(about = "Local REPL for testing (no Telegram needed)")]
    Chat {
        #[arg(long, default_value = "clawhive-main", help = "Agent ID to use")]
        agent: String,
        /// Override security mode (overrides agent config)
        #[arg(long, value_name = "MODE")]
        security: Option<SecurityMode>,
        /// Shorthand for --security off
        #[arg(long)]
        no_security: bool,
    },
    #[command(about = "Show current configuration (tokens masked)")]
    Config,
    #[command(about = "Validate config files")]
    Validate,
    #[command(about = "Run memory consolidation manually")]
    Consolidate,
    #[command(subcommand, about = "Agent management")]
    Agent(commands::agent::AgentCommands),
    #[command(subcommand, about = "Skill management")]
    Skill(commands::skill::SkillCommands),
    #[command(subcommand, about = "Session management")]
    Session(commands::session::SessionCommands),
    #[command(subcommand, about = "Task management")]
    Task(commands::task::TaskCommands),
    #[command(subcommand, about = "Auth management")]
    Auth(AuthCommands),
    #[command(subcommand, about = "Manage scheduled tasks")]
    Schedule(commands::schedule::ScheduleCommands),
    #[command(subcommand, about = "Manage wait tasks (background polling)")]
    Wait(commands::wait::WaitCommands),
    #[command(subcommand, about = "Manage runtime allowlist")]
    Allowlist(commands::allowlist::AllowlistCommands),
    #[command(about = "Interactive configuration manager")]
    Setup {
        #[arg(long, help = "Skip confirmation prompts on reconfigure/remove")]
        force: bool,
    },
    #[command(about = "Update clawhive to the latest version", alias = "upgrade")]
    Update {
        #[arg(long, help = "Check for updates without installing")]
        check: bool,
        #[arg(long, help = "Update channel (alpha, beta, rc, stable)")]
        channel: Option<String>,
        #[arg(long, help = "Install a specific version")]
        version: Option<String>,
        #[arg(long, short = 'y', help = "Skip confirmation prompt")]
        yes: bool,
    },
    #[command(about = "Tail the latest clawhive log file")]
    Logs {
        #[arg(
            long,
            short = 'n',
            default_value = "50",
            help = "Number of lines to show before following"
        )]
        lines: usize,
    },
}

/// Read log_level from main.yaml before full config loading.
/// Returns "info" on any failure (file missing, parse error, field absent).
fn read_log_level(config_root: &std::path::Path) -> String {
    let path = config_root.join("config/main.yaml");
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return "info".to_string(),
    };
    serde_yaml::from_str::<serde_yaml::Value>(&content)
        .ok()
        .and_then(|v| v.get("log_level")?.as_str().map(|s| s.to_string()))
        .unwrap_or_else(|| "info".to_string())
}

#[tokio::main]
async fn main() -> Result<()> {
    let mut cli = Cli::parse();

    // Expand ~ to home directory
    if cli.config_root.starts_with("~") {
        if let Some(home) = std::env::var_os("HOME") {
            cli.config_root = PathBuf::from(home).join(
                cli.config_root
                    .strip_prefix("~")
                    .unwrap_or(&cli.config_root),
            );
        }
    }

    let log_dir = cli.config_root.join("logs");
    std::fs::create_dir_all(&log_dir)?;
    let file_appender = tracing_appender::rolling::daily(&log_dir, "clawhive.log");
    let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);

    let config_log_level = read_log_level(&cli.config_root);
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(&config_log_level));

    // Suppress stderr logs when running TUI modes or Logs to avoid corrupting the terminal.
    let is_tui_mode = matches!(
        cli.command,
        Some(Commands::Code { .. })
            | Some(Commands::Dashboard { .. })
            | Some(Commands::Logs { .. })
    );

    if is_tui_mode {
        tracing_subscriber::registry()
            .with(env_filter)
            .with(
                tracing_subscriber::fmt::layer()
                    .with_ansi(false)
                    .with_writer(non_blocking),
            )
            .init();
    } else {
        tracing_subscriber::registry()
            .with(env_filter)
            .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
            .with(
                tracing_subscriber::fmt::layer()
                    .with_ansi(false)
                    .with_writer(non_blocking),
            )
            .init();
    }

    let Some(command) = cli.command else {
        Cli::command().print_help()?;
        println!();
        return Ok(());
    };

    match command {
        Commands::Config => {
            commands::config::print_config(&cli.config_root)?;
        }
        Commands::Validate => {
            commands::validate::run(&cli.config_root)?;
        }
        Commands::Start {
            daemon,
            tui,
            port,
            security,
            no_security,
        } => {
            let security_override = resolve_security_override(security, no_security);
            commands::start::run_start(&cli.config_root, daemon, tui, port, security_override)
                .await?;
        }
        Commands::Up {
            port,
            security,
            no_security,
        } => {
            let security_override = resolve_security_override(security, no_security);
            commands::start::run_up(&cli.config_root, port, security_override).await?;
        }
        Commands::Status => {
            commands::status::print_status(&cli.config_root);
        }
        Commands::Stop => {
            commands::start::run_stop(&cli.config_root)?;
        }
        Commands::Restart {
            port,
            security,
            no_security,
        } => {
            let security_override = resolve_security_override(security, no_security);
            commands::start::run_restart(&cli.config_root, port, security_override).await?;
        }
        Commands::Code {
            port,
            security,
            no_security,
        } => {
            commands::code::run(&cli.config_root, port, security, no_security).await?;
        }
        Commands::Dashboard { port } => {
            commands::dashboard::run(port).await?;
        }
        Commands::Chat {
            agent,
            security,
            no_security,
        } => {
            commands::chat::run(&cli.config_root, agent, security, no_security).await?;
        }
        Commands::Consolidate => {
            commands::consolidate::run(&cli.config_root).await?;
        }
        Commands::Agent(cmd) => {
            commands::agent::run(cmd, &cli.config_root)?;
        }
        Commands::Skill(cmd) => {
            commands::skill::run(cmd, &cli.config_root).await?;
        }
        Commands::Session(cmd) => {
            commands::session::run(cmd, &cli.config_root).await?;
        }
        Commands::Task(cmd) => {
            commands::task::run(cmd, &cli.config_root).await?;
        }
        Commands::Auth(cmd) => {
            handle_auth_command(cmd).await?;
        }
        Commands::Schedule(cmd) => {
            commands::schedule::run(cmd, &cli.config_root).await?;
        }
        Commands::Wait(cmd) => {
            commands::wait::run(cmd, &cli.config_root).await?;
        }
        Commands::Allowlist(cmd) => {
            commands::allowlist::run(cmd, &cli.config_root)?;
        }
        Commands::Setup { force } => {
            run_setup(&cli.config_root, force).await?;
        }
        Commands::Update {
            check,
            channel,
            version,
            yes,
        } => {
            commands::update::handle_update(check, channel, version, yes).await?;
        }
        Commands::Logs { lines } => {
            commands::logs::run(&cli.config_root, lines)?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn parses_consolidate_subcommand() {
        let cli = Cli::parse_from(["clawhive", "consolidate"]);
        assert!(matches!(cli.command.unwrap(), Commands::Consolidate));
    }

    #[test]
    fn parses_start_tui_flag() {
        let cli = Cli::try_parse_from(["clawhive", "start", "--tui"]).unwrap();
        assert!(matches!(
            cli.command.unwrap(),
            Commands::Start { tui: true, .. }
        ));
    }

    #[test]
    fn parses_start_no_security_flag() {
        let cli = Cli::try_parse_from(["clawhive", "start", "--no-security"]).unwrap();
        assert!(matches!(
            cli.command.unwrap(),
            Commands::Start {
                no_security: true,
                ..
            }
        ));
    }

    #[test]
    fn parses_start_security_off() {
        let cli = Cli::try_parse_from(["clawhive", "start", "--security", "off"]).unwrap();
        if let Commands::Start { security, .. } = cli.command.unwrap() {
            assert_eq!(security, Some(SecurityMode::Off));
        } else {
            panic!("expected Start command");
        }
    }

    #[test]
    fn parses_chat_no_security_flag() {
        let cli = Cli::try_parse_from(["clawhive", "chat", "--no-security"]).unwrap();
        assert!(matches!(
            cli.command.unwrap(),
            Commands::Chat {
                no_security: true,
                ..
            }
        ));
    }

    #[test]
    fn parses_agent_list_subcommand() {
        let cli = Cli::try_parse_from(["clawhive", "agent", "list"]).unwrap();
        assert!(matches!(
            cli.command.unwrap(),
            Commands::Agent(commands::agent::AgentCommands::List)
        ));
    }

    #[test]
    fn parses_skill_list_subcommand() {
        let cli = Cli::try_parse_from(["clawhive", "skill", "list"]).unwrap();
        assert!(matches!(
            cli.command.unwrap(),
            Commands::Skill(commands::skill::SkillCommands::List)
        ));
    }

    #[test]
    fn parses_session_reset_subcommand() {
        let cli = Cli::try_parse_from(["clawhive", "session", "reset", "my-session"]).unwrap();
        assert!(matches!(
            cli.command.unwrap(),
            Commands::Session(commands::session::SessionCommands::Reset { .. })
        ));
    }

    #[test]
    fn parses_task_trigger_subcommand() {
        let cli = Cli::try_parse_from(["clawhive", "task", "trigger", "main", "do stuff"]).unwrap();
        assert!(matches!(
            cli.command.unwrap(),
            Commands::Task(commands::task::TaskCommands::Trigger { .. })
        ));
    }

    #[test]
    fn parses_agent_enable_subcommand() {
        let cli = Cli::try_parse_from(["clawhive", "agent", "enable", "my-agent"]).unwrap();
        assert!(matches!(
            cli.command.unwrap(),
            Commands::Agent(commands::agent::AgentCommands::Enable { .. })
        ));
    }

    #[test]
    fn parses_auth_status_subcommand() {
        let cli = Cli::try_parse_from(["clawhive", "auth", "status"]).unwrap();
        assert!(matches!(
            cli.command.unwrap(),
            Commands::Auth(AuthCommands::Status)
        ));
    }

    #[test]
    fn parses_auth_login_openai_subcommand() {
        let cli = Cli::try_parse_from(["clawhive", "auth", "login", "openai"]).unwrap();
        assert!(matches!(
            cli.command.unwrap(),
            Commands::Auth(AuthCommands::Login { .. })
        ));
    }

    #[test]
    fn setup_ui_symbols_exist() {
        let _ = crate::commands::setup::ui::CHECKMARK;
        let _ = crate::commands::setup::ui::ARROW;
        let _ = crate::commands::setup::ui::CRAB;
    }

    #[test]
    fn parses_setup_force_flag() {
        let cli = Cli::try_parse_from(["clawhive", "setup", "--force"]).unwrap();
        assert!(matches!(
            cli.command.unwrap(),
            Commands::Setup { force: true }
        ));
    }

    #[test]
    fn parses_stop_subcommand() {
        let cli = Cli::try_parse_from(["clawhive", "stop"]).unwrap();
        assert!(matches!(cli.command.unwrap(), Commands::Stop));
    }

    #[test]
    fn parses_restart_subcommand() {
        let cli = Cli::try_parse_from(["clawhive", "restart"]).unwrap();
        assert!(matches!(cli.command.unwrap(), Commands::Restart { .. }));
    }

    #[test]
    fn parses_restart_with_port() {
        let cli = Cli::try_parse_from(["clawhive", "restart", "--port", "8080"]).unwrap();
        assert!(matches!(
            cli.command.unwrap(),
            Commands::Restart { port: 8080, .. }
        ));
    }

    #[test]
    fn parses_dashboard_with_port() {
        let cli = Cli::try_parse_from(["clawhive", "dashboard", "--port", "8081"]).unwrap();
        assert!(matches!(
            cli.command.unwrap(),
            Commands::Dashboard { port: 8081 }
        ));
    }

    #[test]
    fn parses_code_with_port() {
        let cli = Cli::try_parse_from(["clawhive", "code", "--port", "8082"]).unwrap();
        assert!(matches!(
            cli.command.unwrap(),
            Commands::Code { port: 8082, .. }
        ));
    }

    #[test]
    fn no_args_defaults_to_chat() {
        let cli = Cli::try_parse_from(["clawhive"]).unwrap();
        assert!(cli.command.is_none());
    }
}
