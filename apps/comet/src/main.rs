//! comet — headed by default; `comet headless` runs the engine alone. Auth is
//! decoupled from the daemon: `comet login` persists the session and exits, so a
//! service-managed `comet headless` only ever loads saved credentials.

mod auth_cli;
mod daemon;
mod update_cli;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "comet", about = "Multi-device controller for coding agents")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run the engine without a UI (VPS / remote device mode).
    Headless,
    /// Sign in (paste-code flow), persist the session, and exit.
    Login,
    /// Remove the saved session.
    Logout,
    /// Show auth + engine status (exits nonzero when a sign-in is needed).
    Status,
    /// Manage `comet headless` as a background service (launchd / systemd --user).
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
    Tui(comet_tui::cli::TuiArgs),
}

#[derive(Subcommand)]
enum DaemonCommand {
    /// Install, enable, and start the service (captures COMET_* env).
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

/// the pi orchestrator is local-first. an edge url is retained only for the
/// compatibility sync path when explicitly configured with a bearer.
const DEFAULT_EDGE_URL: &str = "http://127.0.0.1:1";

fn edge_url_from_env() -> String {
    std::env::var("COMET_EDGE_URL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_EDGE_URL.into())
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
        Some(Command::Tui(args)) => comet_tui::cli::run(args),
        Some(Command::Headless) => {
            let runtime = tokio::runtime::Runtime::new()?;
            runtime.block_on(async {
                let engine = comet_engine::Engine::new(engine_config_from_env());
                engine.run().await
            })
        }
        Some(Command::Login) => {
            let runtime = tokio::runtime::Runtime::new()?;
            runtime.block_on(auth_cli::login(engine_config_from_env()))
        }
        Some(Command::Logout) => {
            let runtime = tokio::runtime::Runtime::new()?;
            runtime.block_on(auth_cli::logout(engine_config_from_env()))
        }
        Some(Command::Status) => {
            let runtime = tokio::runtime::Runtime::new()?;
            runtime.block_on(auth_cli::status(engine_config_from_env()))
        }
        Some(Command::Update { check }) => {
            let runtime = tokio::runtime::Runtime::new()?;
            runtime.block_on(update_cli::update(&edge_url_from_env(), check))
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
            let edge_token = std::env::var("COMET_EDGE_TOKEN").ok();
            // Headed: the UI probes COMET_IPC_PORT and connects to a running
            // daemon, or embeds the engine in-process (ARCHITECTURE §1).
            comet_ui::run_app(comet_ui::UiConfig {
                data_dir: std::env::var_os("COMET_DATA_DIR")
                    .map(std::path::PathBuf::from)
                    .unwrap_or_else(dirs_data_dir),
                ipc_port: std::env::var("COMET_IPC_PORT")
                    .ok()
                    .and_then(|p| p.parse().ok())
                    .unwrap_or(27654),
                edge_url: edge_url_from_env(),
                // the ui is a local pi viewport; no hosted auth gate is needed.
                workos_client_id: None,
                edge_token,
                org_id: std::env::var("COMET_ORG_ID").ok(),
                default_harness: comet_ui::HarnessId::Pi,
            });
            Ok(())
        }
    }
}

/// The env-resolved engine configuration shared by `headless`, `login`,
/// `logout`, and `status` — one resolution so the CLI auth commands always
/// operate on the exact session the daemon will load.
fn engine_config_from_env() -> comet_engine::EngineConfig {
    // Dev-mode bearer (no WorkOS): an explicit token enables sync.
    let edge_token = std::env::var("COMET_EDGE_TOKEN").ok();
    comet_engine::EngineConfig {
        data_dir: std::env::var_os("COMET_DATA_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(dirs_data_dir),
        edge_url: edge_url_from_env(),
        ipc_port: std::env::var("COMET_IPC_PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(27654),
        default_harness: harness_from_env(),
        // WorkOS mode: the signed-in session's org wins; COMET_ORG_ID (dev
        // default "dev-org") scopes the workspace room otherwise.
        org_id: std::env::var("COMET_ORG_ID").ok(),
        // pi sessions are local and do not require the hosted auth backend.
        workos_client_id: None,
        edge_token,
    }
}

/// `COMET_HARNESS` (kebab-case id) picks the default harness for chats without a
/// config row — `mock` powers the e2e smoke; default `claude-code`.
fn harness_from_env() -> comet_engine::HarnessId {
    match std::env::var("COMET_HARNESS").as_deref().map(str::trim) {
        Ok("mock") => comet_engine::HarnessId::Mock,
        Ok("pi") => comet_engine::HarnessId::Pi,
        Ok("codex") => comet_engine::HarnessId::Codex,
        Ok("cursor") => comet_engine::HarnessId::Cursor,
        _ => comet_engine::HarnessId::Pi,
    }
}

fn dirs_data_dir() -> std::path::PathBuf {
    let home = std::env::var_os("HOME").expect("HOME not set");
    std::path::PathBuf::from(home).join(".comet-native")
}
