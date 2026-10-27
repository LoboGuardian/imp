//! `imp doctor` — one command that answers "why is this not working?" by
//! checking the environment, the configuration, the database and the provider
//! (§5.12).
//!
//! Exit codes match §5.12: `0` everything checked out, `3` the provider did not
//! answer (or refused the credentials), `5` the database could not be opened.
//! The provider check is the only network call, and it happens last so a broken
//! config is reported before anything is sent anywhere.

use std::io::IsTerminal;
use std::path::Path;
use std::process::ExitCode;
use std::time::Duration;

use imp_core::config::{Config, ContextProbe, default_credentials_path, user_config_path};
use imp_core::error::{Error, Result};
use imp_provider::OpenAiProvider;
use imp_store::Store;

use crate::cli::Cli;
use crate::config_cmd::mode_of;
use crate::style::{self, Glyph, Theme};

/// The provider probe gets its own, shorter budget than a chat request: a
/// doctor run should not hang for the length of a generation.
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// One line of the report.
struct Check {
    area: &'static str,
    ok: bool,
    detail: String,
}

impl Check {
    fn ok(area: &'static str, detail: impl Into<String>) -> Self {
        Self {
            area,
            ok: true,
            detail: detail.into(),
        }
    }

    fn fail(area: &'static str, detail: impl Into<String>) -> Self {
        Self {
            area,
            ok: false,
            detail: detail.into(),
        }
    }
}

/// Run the checks and pick the exit code.
pub async fn run(cli: &Cli, config: &Config, cwd: &Path) -> Result<ExitCode> {
    let mut checks = Vec::new();

    checks.push(environment_check(cwd));
    checks.push(config_check(config));
    checks.extend(path_checks(cwd));

    let database = cli.db.clone().unwrap_or_else(|| config.database_path());
    let (db_check, db_ok) = database_check(&database).await;
    checks.push(db_check);

    let provider_check = provider_check(config).await;
    let provider_ok = provider_check.ok;
    checks.push(provider_check);

    checks.push(context_check(config).await);

    let all_ok = checks.iter().all(|check| check.ok);
    report(cli, style::stdout_theme(cli, config), &checks);

    Ok(if all_ok {
        ExitCode::SUCCESS
    } else if !provider_ok {
        // A provider that does not answer is an auth/transport failure: §5.12's
        // code 3.
        ExitCode::from(3)
    } else if !db_ok {
        ExitCode::from(5)
    } else {
        ExitCode::from(1)
    })
}

/// What imp is, where it is running, and whether a human can be prompted.
fn environment_check(cwd: &Path) -> Check {
    let tty = std::io::stdin().is_terminal();
    Check::ok(
        "environment",
        format!(
            "imp {} · {} {} · cwd {} · {}",
            crate::version::VERSION,
            std::env::consts::OS,
            std::env::consts::ARCH,
            cwd.display(),
            if tty {
                "interactive terminal"
            } else {
                "no terminal (approval will fail closed)"
            }
        ),
    )
}

/// The resolved provider settings, and where a credential would come from.
fn config_check(config: &Config) -> Check {
    let key_source = if config.provider.api_key_env.trim().is_empty()
        && config.provider.api_key_file.trim().is_empty()
    {
        "no credentials needed".to_string()
    } else {
        match config.api_key() {
            Ok(Some(_)) => "a credential is available".to_string(),
            Ok(None) => "no credentials needed".to_string(),
            Err(_) => format!(
                "no credential found (set {} or run `imp init`)",
                config.provider.api_key_env
            ),
        }
    };
    Check::ok(
        "config",
        format!(
            "{} · model {} · {}",
            config.provider.base_url, config.provider.model, key_source
        ),
    )
}

/// Where the files live, and whether the private ones are private.
fn path_checks(cwd: &Path) -> Vec<Check> {
    let mut checks = Vec::new();
    if let Some(user) = user_config_path() {
        checks.push(describe_path("user config", &user, false));
    }
    checks.push(describe_path(
        "project config",
        &cwd.join("imp.toml"),
        false,
    ));
    if let Some(credentials) = default_credentials_path() {
        checks.push(describe_path("credentials", &credentials, true));
    }
    checks
}

/// One path, with a warning when a private file is more readable than it should
/// be (the `0600`/`0700` promise of §9).
fn describe_path(area: &'static str, path: &Path, private: bool) -> Check {
    if !path.exists() {
        return Check::ok(area, format!("{} (not created yet)", path.display()));
    }
    match mode_of(path) {
        Some(mode) if private && mode & 0o077 != 0 => Check::fail(
            area,
            format!(
                "{} is {mode:04o}; expected owner-only (0600). Run chmod 600 on it.",
                path.display()
            ),
        ),
        Some(mode) => Check::ok(area, format!("{} ({mode:04o})", path.display())),
        None => Check::ok(area, path.display().to_string()),
    }
}

/// Open the store, which is also a migration and integrity check (T10).
async fn database_check(path: &Path) -> (Check, bool) {
    match Store::open(path).await {
        Ok(store) => {
            let count = store
                .list_sessions(1)
                .await
                .map(|rows| rows.len())
                .unwrap_or(0);
            let audit = audit_summary(&store).await;
            (
                Check::ok(
                    "database",
                    format!(
                        "{} · schema {} · writable{}{audit}",
                        store.path().display(),
                        imp_store::SCHEMA_VERSION,
                        if count > 0 { "" } else { " · no sessions yet" }
                    ),
                ),
                true,
            )
        }
        Err(err) => (Check::fail("database", err.to_string()), false),
    }
}

/// The tail of the audit trail, so `doctor` shows whether the gate recorded
/// anything at all — and, if so, what it decided last (a refused tool is the
/// first thing an operator wants to see when something "did nothing").
async fn audit_summary(store: &Store) -> String {
    match store.recent_audit(1).await {
        Ok(rows) => match rows.first() {
            Some(row) => {
                let tool = row.tool.as_deref().unwrap_or("(unknown tool)");
                let decision = row.decision.as_deref().unwrap_or("(open)");
                format!(" · last decision {decision} {tool}")
            }
            None => " · no decisions recorded".to_string(),
        },
        Err(_) => String::new(),
    }
}

/// What the backend says about the model's context window and capabilities.
///
/// The OpenAI-compatible `/models` list says a model *exists*; it says nothing
/// about whether it can be loaded or whether it can call tools. A local server
/// is exactly where that distinction bites: a model that failed to load is
/// still advertised, so the reachability check above passes while every turn
/// fails with a 400. This check asks the one question the reachability check
/// cannot.
async fn context_check(config: &Config) -> Check {
    let explicit = config.provider.context_window;
    let info = match config.provider.context_probe {
        ContextProbe::Off => None,
        ContextProbe::Auto => {
            let key = config.api_key().ok().flatten().unwrap_or_default();
            let provider = OpenAiProvider::new(&config.provider.base_url, key)
                .with_request_timeout(Some(PROBE_TIMEOUT))
                .with_max_retries(1)
                .with_headers(config.request_headers());
            provider.probe_context(&config.provider.model).await
        }
    };

    if info.as_ref().and_then(|info| info.tool_use) == Some(false) {
        return Check::fail(
            "context",
            format!(
                "{} does not advertise tool calling; every tool call will be refused",
                config.provider.model
            ),
        );
    }

    let budget = crate::setup::effective_budget(config, info.as_ref());
    match (explicit, info) {
        (Some(window), _) => Check::ok(
            "context",
            format!("{window} configured · budget {}", describe_budget(budget)),
        ),
        (None, Some(info)) => {
            let window = match (info.context_window, info.max_context) {
                (Some(window), _) => format!("{window} window"),
                (None, Some(max)) => format!("{max} max"),
                (None, None) => "window unknown".to_string(),
            };
            let note = match info.context_window {
                Some(_) => format!(" · budget {}", describe_budget(budget)),
                // Nothing is loaded, so there is no runtime window to measure;
                // this is the state that produces a load error on first use.
                None => {
                    " · loads on the first request, which can fail for want of memory".to_string()
                }
            };
            Check::ok("context", format!("{} · {window}{note}", info.describe()))
        }
        (None, None) => Check::ok(
            "context",
            "not discovered — set provider.context_window or agent.context_tokens",
        ),
    }
}

/// Render a budget, or say that there is none.
fn describe_budget(budget: Option<usize>) -> String {
    match budget {
        Some(tokens) => tokens.to_string(),
        None => "none (no trimming)".to_string(),
    }
}

/// Ask the backend whether it answers.
///
/// `/models` is optional in the OpenAI-compatible world, so a 4xx that is not an
/// auth rejection still counts as "the provider is there": the point is
/// reachability and credentials, not a full capability probe.
async fn provider_check(config: &Config) -> Check {
    let key = match config.api_key() {
        Ok(key) => key.unwrap_or_default(),
        Err(err) => return Check::fail("provider", err.to_string()),
    };

    let provider = OpenAiProvider::new(&config.provider.base_url, key)
        .with_request_timeout(Some(PROBE_TIMEOUT))
        .with_max_retries(1)
        .with_headers(config.request_headers())
        .with_session_id(imp_core::new_session_id());

    match provider.list_models().await {
        Ok(models) => Check::ok(
            "provider",
            format!(
                "{} answered · {} model(s) advertised",
                config.provider.base_url,
                models.len()
            ),
        ),
        Err(Error::RateLimit { retry_after }) => Check::ok(
            "provider",
            format!(
                "{} is up but rate limited ({retry_after:?})",
                config.provider.base_url
            ),
        ),
        Err(Error::BadRequest { status, .. }) if status != 401 && status != 403 => Check::ok(
            "provider",
            format!(
                "{} answered (HTTP {status}); it does not implement /models",
                config.provider.base_url
            ),
        ),
        Err(err) => Check::fail(
            "provider",
            format!("{} did not answer: {err}", config.provider.base_url),
        ),
    }
}

/// Print the checks, as prose or as one JSON object.
fn report(cli: &Cli, theme: Theme, checks: &[Check]) {
    let ok = checks.iter().all(|check| check.ok);
    if cli.json {
        let items: Vec<serde_json::Value> = checks
            .iter()
            .map(|check| {
                serde_json::json!({
                    "type": "check",
                    "area": check.area,
                    "ok": check.ok,
                    "detail": check.detail,
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::json!({ "type": "doctor", "ok": ok, "checks": items })
        );
        return;
    }

    for check in checks {
        let (mark, label) = if check.ok {
            (
                theme.success(theme.glyph(Glyph::Check)),
                theme.success("ok  "),
            )
        } else {
            (theme.error(theme.glyph(Glyph::Cross)), theme.error("FAIL"))
        };
        println!(
            "{mark}  {label} {:<16} {}",
            theme.bold(&format!("{:<16}", check.area)),
            check.detail
        );
    }
    if ok {
        println!(
            "{}",
            theme.success("all checks passed — imp is ready to run")
        );
    } else {
        eprintln!(
            "{}",
            theme.error("imp: doctor found a problem; see the FAIL lines above")
        );
    }
}
