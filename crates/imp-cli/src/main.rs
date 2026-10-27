//! `imp`: a minimal, Unix-native AI agent harness.

mod approval;
mod cli;
mod config_cmd;
mod cron;
mod doctor;
mod init;
mod logging;
mod markdown;
mod mcp;
mod mcp_serve;
mod render;
mod repl;
mod run;
mod session;
mod setup;
mod style;
mod summarize;
mod update;
mod version;

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use clap::Parser;

use cli::{Cli, Command, ConfigAction};
use imp_core::agent::StopReason;
use imp_core::config::{Config, LogFormat};
use imp_core::error::{Error, Result};
use imp_store::Store;

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    match execute(cli).await {
        Ok(code) => code,
        Err(err) => {
            eprintln!("imp: {err}");
            ExitCode::from(exit_code(&err))
        }
    }
}

async fn execute(cli: Cli) -> Result<ExitCode> {
    let cwd = resolve_cwd(&cli)?;

    // The project was renamed, and the config, the `0600` credentials file and
    // the session database all live under the old application directory. This
    // runs before the `init` branch below and before the configuration is
    // loaded, so `imp` moves the old data instead of starting empty beside it
    // (SDD D46). Nothing is printed on a machine that never had the old name.
    for note in imp_core::config::migrate_legacy_state() {
        eprintln!("imp: {note}");
    }

    // `init` runs before configuration is loaded: its whole purpose is to
    // repair a missing or broken config, so it must not depend on one.
    // `config init` is the same command under the read-only `config` group.
    if let Some(args) = init_args(&cli.command) {
        init_logging(
            "info",
            LogFormat::Text,
            cli.verbose,
            cli.quiet,
            None,
            Arc::new(Vec::new()),
        );
        return init::run(&cli, args.clone()).await;
    }

    let mut config = Config::load(cli.config.as_deref(), &cwd)?;
    if let Some(model) = &cli.model {
        config.provider.model = model.clone();
    }
    if let Some(base_url) = &cli.base_url {
        config.provider.base_url = base_url.clone();
    }
    if let Some(name) = &cli.api_key_env {
        config.provider.api_key_env = name.clone();
    }
    if let Some(iterations) = cli.max_iterations {
        // `--max-iterations 0` would end every turn immediately; the config
        // validation rejects that, so the flag does too — same message, same
        // exit code `2` as a `[agent] max_iterations = 0` in the file.
        if iterations == 0 {
            return Err(Error::Config(
                "agent.max_iterations must be at least 1".to_string(),
            ));
        }
        config.agent.max_iterations = iterations;
    }

    // The small-model profile is a global flag, so it is applied here — after
    // the file and the other flags, exactly as they are (D45).
    if cli.lean {
        config.apply_lean_profile();
    }

    // The redaction layer is built from the *resolved* configuration, so the
    // key the process will actually use is the one masked in the logs (NFR-9).
    let secrets = logging::Secrets::from_config(&config).into_shared();
    init_logging(
        &config.logging.level,
        config.logging.format,
        cli.verbose,
        cli.quiet,
        config.logging.file.as_deref(),
        secrets,
    );

    match &cli.command {
        Some(Command::Init(_)) => unreachable!("handled above"),
        Some(Command::Config(args)) => config_cmd::run(&cli, &config, &cwd, args.clone()),
        Some(Command::Doctor) => doctor::run(&cli, &config, &cwd).await,
        Some(Command::Session(args)) => session::run(&cli, &config, args.clone()).await,
        Some(Command::Cron(args)) => cron::run(&cli, &config, args.clone()).await,
        Some(Command::Mcp(args)) => mcp::run(&cli, &config, &cwd, args.clone()).await,
        Some(Command::Update(args)) => update::run(&cli, &config, args.clone()).await,
        Some(Command::Run { prompt }) => {
            let prompt = resolve_prompt(prompt)?;
            let resume = resolve_resume(&cli, &config, &cwd).await?;
            let stop = run::one_shot(&cli, &config, &cwd, prompt, resume.as_deref()).await?;
            Ok(if stop == StopReason::Completed {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(1)
            })
        }
        None => {
            let resume = resolve_resume(&cli, &config, &cwd).await?;
            repl::interactive(&cli, &config, &cwd, resume.as_deref()).await?;
            Ok(ExitCode::SUCCESS)
        }
    }
}

/// The `init` arguments, whether spelled `imp init` or `imp config init`.
fn init_args(command: &Option<Command>) -> Option<&cli::InitArgs> {
    match command {
        Some(Command::Init(args)) => Some(args),
        Some(Command::Config(config)) => match &config.action {
            ConfigAction::Init(args) => Some(args),
            _ => None,
        },
        _ => None,
    }
}

fn resolve_cwd(cli: &Cli) -> Result<PathBuf> {
    let cwd = match cli.cwd.clone() {
        Some(dir) => dir,
        None => std::env::current_dir()?,
    };
    Ok(cwd.canonicalize().unwrap_or(cwd))
}

/// Turn `--resume <needle>` into a full session id, accepting a prefix or a
/// position from `session list` exactly as `imp session resume` does.
///
/// `--continue`/`-c` is the other form: it picks the most recent conversation
/// whose stored workspace is this one, so a bare `imp -c` in a project resumes
/// that project rather than whatever was touched last anywhere.
async fn resolve_resume(cli: &Cli, config: &Config, cwd: &Path) -> Result<Option<String>> {
    if cli.resume.is_some() && cli.continue_session {
        // `conflicts_with` already rejects this, but keep it obvious if a caller
        // ever bypasses clap.
        return Err(Error::Config(
            "--resume and --continue cannot be used together".to_string(),
        ));
    }

    let database = cli.db.clone().unwrap_or_else(|| config.database_path());

    if cli.continue_session {
        let store = Store::open(&database).await?;
        let workspace = config.workspace_root(cwd)?;
        let key = workspace.display().to_string();
        return store
            .latest_session_in(&key)
            .await?
            .map(|row| row.id)
            .ok_or_else(|| {
                Error::Config(format!(
                    "no conversation in {}; run `imp` to start one",
                    workspace.display()
                ))
            })
            .map(Some);
    }

    let Some(needle) = &cli.resume else {
        return Ok(None);
    };
    let store = Store::open(&database).await?;
    Ok(Some(session::resolve(&store, needle).await?))
}

/// `-` means "read the prompt from stdin", which makes imp pipeable.
fn resolve_prompt(prompt: &str) -> Result<String> {
    if prompt != "-" {
        return Ok(prompt.to_string());
    }
    use std::io::Read;
    let mut buffer = String::new();
    std::io::stdin().read_to_string(&mut buffer)?;
    let trimmed = buffer.trim();
    if trimmed.is_empty() {
        return Err(Error::Config("no prompt was provided on stdin".to_string()));
    }
    Ok(trimmed.to_string())
}

/// Logs always go to stderr so stdout stays machine-readable.
///
/// Every line passes through [`logging::RedactingMakeWriter`], which masks the
/// configured credentials and any `Bearer` token before a byte is written
/// (NFR-9) — including to `logging.file`, when one is set.
fn init_logging(
    level: &str,
    format: LogFormat,
    verbose: u8,
    quiet: bool,
    file: Option<&str>,
    secrets: Arc<Vec<String>>,
) {
    let level = if quiet {
        // Quiet means errors and results: warnings still surface, chatter does
        // not.
        "warn".to_string()
    } else {
        match verbose {
            0 => level.to_string(),
            1 => "debug".to_string(),
            _ => "trace".to_string(),
        }
    };
    // `rmcp` announces every service, task cancellation and shutdown at INFO.
    // That is protocol chatter on a channel the REPL uses for tool activity, so
    // it is quieted at the default verbosity and left alone once `-v` asks for
    // the detail.
    let mut level = level;
    if verbose == 0 && !quiet && !level.contains("rmcp") {
        level.push_str(",rmcp=warn");
    }
    let filter = tracing_subscriber::EnvFilter::try_new(&level)
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));

    let writer = logging::RedactingMakeWriter::new(secrets, file);
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(writer);

    if format == LogFormat::Json {
        let _ = builder.json().try_init();
    } else {
        let _ = builder.try_init();
    }
}

/// Exit codes documented in the SDD.
pub(crate) fn exit_code(err: &Error) -> u8 {
    match err {
        Error::Config(_) => 2,
        Error::Auth(_)
        | Error::Provider(_)
        | Error::RateLimit { .. }
        | Error::BadRequest { .. } => 3,
        Error::Denied(_) | Error::Refused(_) => 4,
        _ => 5,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_codes_match_the_sdd() {
        assert_eq!(exit_code(&Error::Config("x".into())), 2);
        assert_eq!(exit_code(&Error::Auth("x".into())), 3);
        assert_eq!(exit_code(&Error::Provider("x".into())), 3);
        assert_eq!(
            exit_code(&Error::BadRequest {
                status: 400,
                message: "x".into()
            }),
            3
        );
        assert_eq!(exit_code(&Error::Denied("x".into())), 4);
        assert_eq!(
            exit_code(&Error::UnknownTool {
                name: "x".into(),
                available: Vec::new()
            }),
            5
        );
    }

    #[test]
    fn config_init_is_recognised_as_init() {
        let command = Command::Config(cli::ConfigArgs {
            action: ConfigAction::Init(cli::InitArgs {
                preset: None,
                base_url: None,
                model: None,
                api_key_env: None,
                no_api_key: false,
                no_store_token: false,
                credentials_file: None,
                headers: Vec::new(),
                workspace: None,
                project: false,
                force: false,
                non_interactive: false,
                check: false,
            }),
        });
        assert!(init_args(&Some(command)).is_some());
    }
}
