//! nova — headed by default; `nova headless` runs the local engine alone.

mod daemon;
mod update_cli;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "nova", about = "Multi-device controller for coding agents")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run the engine without a UI (VPS / remote device mode).
    Headless,
    /// Manage `nova headless` as a background service (launchd / systemd --user).
    Daemon {
        #[command(subcommand)]
        command: DaemonCommand,
    },
    /// Check for a newer release and apply it (download → verify → swap →
    /// service restart). `--check` only reports (exits 1 when one is available).
    Update {
        #[arg(long)]
        check: bool,
    },
    /// Terminal viewport over the same engine — attaches to a running app or
    /// daemon, or starts one, and detaches (leaving work running) when it exits.
    Tui(nova_tui::cli::TuiArgs),
}

#[derive(Subcommand)]
enum DaemonCommand {
    /// Install, enable, and start the service (captures NOVA_* env).
    Install,
    /// Stop and remove the service.
    Uninstall,
    /// Start the installed service.
    Start,
    /// Stop the service.
    Stop,
    /// Restart the service.
    Restart,
    /// Show the service manager's view of the daemon.
    Status,
}

fn update_url_from_env() -> Option<String> {
    env_var("NOVA_UPDATE_URL", "COMET_UPDATE_URL")
        .ok()
        .filter(|s| !s.trim().is_empty())
}

fn env_var(primary: &str, legacy: &str) -> Result<String, std::env::VarError> {
    std::env::var(primary).or_else(|_| std::env::var(legacy))
}

fn env_os(primary: &str, legacy: &str) -> Option<std::ffi::OsString> {
    std::env::var_os(primary).or_else(|| std::env::var_os(legacy))
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    // The TUI owns its own tracing (to a file — a line on stdout would land
    // inside the alternate screen and corrupt it), so skip the global stdout
    // subscriber entirely for it. Everything else logs to stdout: long-running
    // modes at info, one-shot CLI commands at warn (RUST_LOG overrides either).
    if !matches!(cli.command, Some(Command::Tui(_))) {
        // loro's internal block-encode diagnostics log at info and flood
        // journald on every snapshot export — enough to fill a disk on a
        // long-running headless host. Quiet them by default (RUST_LOG still
        // overrides the whole filter).
        let default_filter = match &cli.command {
            None | Some(Command::Headless) => "info,loro_internal=warn,loro=warn",
            Some(_) => "warn",
        };
        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| default_filter.into()),
            )
            .init();
    }

    match cli.command {
        Some(Command::Tui(args)) => nova_tui::cli::run(args),
        Some(Command::Headless) => {
            let runtime = tokio::runtime::Runtime::new()?;
            runtime.block_on(async {
                let engine = nova_engine::Engine::new(engine_config_from_env());
                engine.run().await
            })
        }
        Some(Command::Update { check }) => {
            let update_url = update_url_from_env()
                .ok_or_else(|| anyhow::anyhow!("set NOVA_UPDATE_URL to the Nova release server"))?;
            let runtime = tokio::runtime::Runtime::new()?;
            runtime.block_on(update_cli::update(&update_url, check))
        }
        Some(Command::Daemon { command }) => match command {
            DaemonCommand::Install => daemon::install(&engine_config_from_env().data_dir),
            DaemonCommand::Uninstall => daemon::uninstall(),
            DaemonCommand::Start => daemon::start(),
            DaemonCommand::Stop => daemon::stop(),
            DaemonCommand::Restart => daemon::restart(),
            DaemonCommand::Status => daemon::status(),
        },
        None => {
            // Headed: the UI probes NOVA_IPC_PORT and connects to a running
            // daemon, or embeds the engine in-process (ARCHITECTURE §1).
            nova_ui::run_app(nova_ui::UiConfig {
                data_dir: data_dir_from_env(),
                ipc_port: env_var("NOVA_IPC_PORT", "COMET_IPC_PORT")
                    .ok()
                    .and_then(|p| p.parse().ok())
                    .unwrap_or(27654),
                nova_port: nova_port_from_env(),
                update_url: update_url_from_env(),
                default_harness: nova_ui::HarnessId::Pi,
            });
            Ok(())
        }
    }
}

/// Environment-resolved configuration shared by headed and headless engines.
fn engine_config_from_env() -> nova_engine::EngineConfig {
    nova_engine::EngineConfig {
        data_dir: data_dir_from_env(),
        update_url: update_url_from_env(),
        ipc_port: env_var("NOVA_IPC_PORT", "COMET_IPC_PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(27654),
        nova_port: nova_port_from_env(),
        default_harness: harness_from_env(),
    }
}

fn nova_port_from_env() -> u16 {
    std::env::var("NOVA_PORT")
        .ok()
        .and_then(|port| port.parse().ok())
        .unwrap_or(27655)
}

/// `NOVA_HARNESS` (kebab-case id) picks the default harness for chats without a
/// config row — `mock` powers the e2e smoke; default `pi`.
fn harness_from_env() -> nova_engine::HarnessId {
    match env_var("NOVA_HARNESS", "COMET_HARNESS")
        .as_deref()
        .map(str::trim)
    {
        Ok("mock") => nova_engine::HarnessId::Mock,
        Ok("pi") => nova_engine::HarnessId::Pi,
        Ok("codex") => nova_engine::HarnessId::Codex,
        Ok("cursor") => nova_engine::HarnessId::Cursor,
        _ => nova_engine::HarnessId::Pi,
    }
}

fn data_dir_from_env() -> std::path::PathBuf {
    env_os("NOVA_DATA_DIR", "COMET_DATA_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(dirs_data_dir)
}

fn dirs_data_dir() -> std::path::PathBuf {
    let home = std::env::var_os("HOME").expect("HOME not set");
    preferred_data_dir(&std::path::PathBuf::from(home))
}

fn preferred_data_dir(home: &std::path::Path) -> std::path::PathBuf {
    let current = home.join(".nova-native");
    let legacy = home.join(".comet-native");
    if !current.exists() && legacy.exists() {
        legacy
    } else {
        current
    }
}

#[cfg(test)]
mod tests {
    use super::preferred_data_dir;

    #[test]
    fn existing_legacy_data_survives_the_product_rename() {
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir(home.path().join(".comet-native")).unwrap();
        assert_eq!(
            preferred_data_dir(home.path()),
            home.path().join(".comet-native")
        );

        std::fs::create_dir(home.path().join(".nova-native")).unwrap();
        assert_eq!(
            preferred_data_dir(home.path()),
            home.path().join(".nova-native")
        );
    }
}
