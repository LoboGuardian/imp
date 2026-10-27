//! One-shot execution.

use std::path::Path;

use imp_core::agent::{AgentEvent, StopReason};
use imp_core::config::Config;
use imp_core::error::Result;
use imp_core::message::Message;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::cli::Cli;
use crate::markdown::Style;
use crate::render::{Activity, Renderer};
use crate::setup;
use crate::style::{self, Theme};

/// Run one prompt to completion and report why the turn ended.
pub async fn one_shot(
    cli: &Cli,
    config: &Config,
    cwd: &Path,
    prompt: String,
    resume: Option<&str>,
) -> Result<StopReason> {
    let mut session = setup::build(cli, config, cwd, resume).await?;

    // Jobs fire while imp runs, one-shot included: the scheduler is
    // in-process (D5), so this is the same service the REPL starts.
    let _cron = crate::cron::start(config, &session.store, session.cron_runner.clone())
        .await
        .ok()
        .flatten();

    // Everything from this index on belongs to the turn, and is persisted as a
    // unit so the transcript never contains half of one.
    let turn_start = session.history.len();
    for notice in session.refresh_mcp().await {
        eprintln!("… {notice}");
        session.history.push(Message::system(notice));
    }
    session.history.push(Message::user(prompt));

    // Printed before the renderer starts, so the two writers cannot interleave.
    // A session id is not an agent concern, so it does not go through AgentEvent.
    if cli.json {
        println!(
            "{}",
            serde_json::json!({ "type": "session", "id": session.session_id })
        );
    }

    let (sender, receiver) = mpsc::unbounded_channel();
    let rendering = tokio::spawn(render_events(
        receiver,
        cli.json,
        style_for(cli, config),
        style::stderr_theme(cli, config),
        activity_enabled(cli, config),
    ));

    let cancel = CancellationToken::new();
    let watcher = spawn_interrupt_watcher(cancel.clone());

    let agent = session.agent();
    let outcome = agent.run(&mut session.history, &sender, cancel).await;

    drop(sender);
    let _ = rendering.await;
    watcher.abort();

    session
        .persist_since(turn_start.saturating_sub(outcome.compacted))
        .await?;

    // Persist the turn's token usage against the session, so `/cost` and
    // `imp session` can aggregate it later without this process (§7). A
    // failure here must not lose the answer, so it is only logged.
    if let Err(err) = session
        .store
        .record_usage(&session.session_id, outcome.usage)
        .await
    {
        tracing::warn!(error = %err, "could not record token usage");
    }

    if !cli.json && !cli.quiet {
        // stderr, so `imp run ... > answer.txt` stays clean.
        eprintln!(
            "session {} · resume with: imp session resume {}",
            short_id(&session.session_id),
            session.session_id
        );
    }

    // The external servers belong to the session, so they go with it.
    session.shutdown().await;

    Ok(outcome.stop)
}

/// First few characters of an id, for one-line notes.
fn short_id(id: &str) -> String {
    id.chars().take(8).collect()
}

/// Drain agent events into a [`Renderer`] until the sender is dropped.
pub async fn render_events(
    mut receiver: mpsc::UnboundedReceiver<AgentEvent>,
    json: bool,
    style: Style,
    theme: Theme,
    activity: bool,
) {
    // The activity line lives only until the first real event, where the
    // renderer erases it. The task is separate so it can be aborted here.
    let (spinner, task) = if activity {
        let (spinner, task) = Activity::spawn(theme);
        (Some(spinner), Some(task))
    } else {
        (None, None)
    };
    let mut renderer = Renderer::new(json, style, theme);
    if let Some(spinner) = &spinner {
        renderer = renderer.with_activity(spinner.clone());
    }
    while let Some(event) = receiver.recv().await {
        renderer.handle(&event);
    }
    renderer.finish();
    if let Some(task) = task {
        task.abort();
    }
}

/// Cancel `cancel` on the first Ctrl-C.
pub fn spawn_interrupt_watcher(cancel: CancellationToken) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            cancel.cancel();
        }
    })
}

/// Whether the activity line may be drawn: asked for, and drawable.
///
/// It is on by default, but `--json` and `--quiet` are machine channels and a
/// pipe is not a terminal, so none of them gets a spinner.
pub fn activity_enabled(cli: &Cli, config: &Config) -> bool {
    config.ui.spinner
        && !cli.json
        && !cli.quiet
        && std::io::IsTerminal::is_terminal(&std::io::stderr())
}

/// The markdown style to render with.
///
/// Rendering follows the terminal, not the flag alone: on a pipe the raw
/// markdown is more useful than a drawn table, so `--markdown` cannot override
/// it. Colour is separately refused by `NO_COLOR`, `--no-color` and
/// `[ui].color`, but layout survives, so a `NO_COLOR` user still gets aligned
/// tables.
pub fn style_for(cli: &Cli, config: &Config) -> Style {
    if cli.no_markdown || !std::io::IsTerminal::is_terminal(&std::io::stdout()) {
        return Style::plain();
    }
    let theme = style::stdout_theme(cli, config);
    Style::rendered(theme.color(), cli.width.unwrap_or_else(terminal_width))
        .with_icons(theme.icons())
}

/// Prefer an explicit width, then `COLUMNS`, then a sane default.
fn terminal_width() -> usize {
    std::env::var("COLUMNS")
        .ok()
        .and_then(|value| value.trim().parse::<usize>().ok())
        .filter(|width| *width >= 20)
        .unwrap_or(80)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::markdown::Style;
    use clap::Parser;

    /// NFR-8: piped output is never rendered, so `imp run … > out.md` keeps
    /// the markdown source. A test process has no terminal on stdout.
    #[test]
    fn piped_output_is_not_rendered() {
        let cli = Cli::parse_from(["imp", "run", "hi"]);

        assert_eq!(style_for(&cli, &Config::default()), Style::plain());
    }

    /// The activity line is drawn only when it can be seen: a terminal, not
    /// `--json`, not `--quiet`. It is on by default.
    #[test]
    fn activity_follows_the_terminal_and_the_machine_flags() {
        let mut cli = Cli::parse_from(["imp", "run", "hi"]);
        let config = Config::default();

        assert!(
            config.ui.spinner,
            "the spinner is on by default, so the check below is meaningful"
        );
        assert!(
            !activity_enabled(&cli, &config),
            "a test process has no terminal to draw it on"
        );

        cli.quiet = true;
        assert!(
            !activity_enabled(&cli, &config),
            "--quiet is a machine channel"
        );

        cli.quiet = false;
        cli.json = true;
        assert!(
            !activity_enabled(&cli, &config),
            "--json is a machine channel"
        );
    }

    /// NFR-8: disabling colour keeps the layout, so a `NO_COLOR` user still
    /// gets aligned tables rather than a wall of raw markdown.
    #[test]
    fn no_color_keeps_the_layout_and_drops_the_escapes() {
        let style = Style::rendered(false, 80);

        assert!(style.enabled, "markdown still renders");
        assert!(!style.color, "but with no escapes");
    }
}
