//! The interactive REPL.
//!
//! Deliberately not a full-screen TUI: input is line-oriented, output is
//! appended, and scrollback is preserved. Tool activity and approvals go to
//! stderr so that piping stdout yields only the conversation.

use std::path::Path;
use std::sync::Arc;

use imp_core::agent::StopReason;
use imp_core::config::{Config, Decision};
use imp_core::error::{Error, Result};
use imp_core::message::{Message, Role};
use imp_core::provider::Usage;
use imp_core::read_only::{AutoCommands, AutoMode};
use rustyline::DefaultEditor;
use rustyline::error::ReadlineError;
use tokio::sync::Mutex;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::cli::Cli;
use crate::run;
use crate::session;
use crate::setup::{self, Session};
use crate::style::{self, Glyph, Theme};

/// Run the interactive loop until the user quits or stdin closes.
///
/// `resume` continues a stored conversation; without it a new one is started.
pub async fn interactive(
    cli: &Cli,
    config: &Config,
    cwd: &Path,
    resume: Option<&str>,
) -> Result<()> {
    let mut state = setup::build(cli, config, cwd, resume).await?;
    let mut editor = DefaultEditor::new()
        .map_err(|err| Error::Config(format!("cannot start the line editor: {err}")))?;

    // The scheduler lives and dies with the session: a job fires while imp is
    // running, and nothing is left behind when it exits (D5).
    let _cron = start_cron(config, &state).await;

    // Ctrl-C mid-turn cancels the turn; the token is replaced so the next turn
    // starts fresh.
    let interrupt = Arc::new(Mutex::new(CancellationToken::new()));
    spawn_interrupt_swapper(interrupt.clone());

    let theme = style::stdout_theme(cli, config);
    if !cli.quiet {
        // The budget is worth one line at startup: it is what decides whether a
        // turn fits, and it is now often derived from the model rather than
        // configured, so a user who never set one should still see it.
        let budget = match state.context_budget {
            Some(tokens) => format!(" · context {tokens}"),
            None => String::new(),
        };
        println!(
            "{} {} · {} · {} · policy: {}{}",
            theme.accent("imp"),
            env!("CARGO_PKG_VERSION"),
            theme.bold(&state.options.model),
            theme.dim(&state.workspace_root.display().to_string()),
            theme.bold(decision_name(config.policy.default)),
            theme.dim(&budget),
        );
        println!(
            "{} {} · {}",
            theme.dim("session"),
            theme.info(&short_id(&state.session_id)),
            theme.dim("/help for commands, /quit to exit"),
        );
    }

    // Resuming prints the conversation, so the user sees the context they are
    // continuing rather than an empty prompt. `--no-history` and `--quiet` opt
    // out; quiet already suppresses the banner for the same reason.
    if resume.is_some() && !cli.no_history && !cli.quiet {
        print_history(&state.history, theme);
    }

    let mut usage = Usage::default();
    // Ctrl-C at the prompt asks once before it exits; the first press is easy to
    // hit by accident. A successful read re-arms the first press.
    let mut interrupts = InterruptGuard::default();

    loop {
        let line = match editor.readline("› ") {
            Ok(line) => {
                interrupts.on_line();
                line
            }
            Err(ReadlineError::Interrupted) => {
                if interrupts.on_interrupt() {
                    break;
                }
                eprintln!("press again to exit");
                continue;
            }
            Err(ReadlineError::Eof) => break,
            Err(err) => {
                eprintln!("imp: input error: {err}");
                break;
            }
        };

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let _ = editor.add_history_entry(trimmed);

        if let Some(command) = trimmed.strip_prefix('!') {
            shell_escape(command, &state.workspace_root).await;
            continue;
        }
        if let Some(command) = trimmed.strip_prefix('/') {
            match handle_slash(command, &mut state, &usage, cli, config).await {
                Ok(true) => break,
                Ok(false) => continue,
                Err(err) => {
                    eprintln!("imp: {err}");
                    continue;
                }
            }
        }

        let turn_start = state.history.len();
        // Retry the external servers before the turn they would serve, and give
        // a `system` message to anything that changed (§5.10).
        for notice in state.refresh_mcp().await {
            eprintln!("… {notice}");
            state.history.push(Message::system(notice));
        }
        state.history.push(Message::user(trimmed));
        // Where the reply will begin. Taken from the length rather than assumed
        // to be `turn_start + 1`, because a notice may have been pushed too.
        let reply_start = state.history.len();
        if let Err(err) = state.persist_since(turn_start).await {
            // Losing the turn is bad, but so is losing the user's prompt; say so
            // and keep going rather than exiting.
            eprintln!("imp: could not save this turn: {err}");
        }

        let (sender, receiver) = mpsc::unbounded_channel();
        let rendering = tokio::spawn(run::render_events(
            receiver,
            cli.json,
            run::style_for(cli, config),
            style::stderr_theme(cli, config),
            run::activity_enabled(cli, config),
        ));

        let cancel = interrupt.lock().await.clone();
        let agent = state.agent();
        let outcome = agent.run(&mut state.history, &sender, cancel).await;
        usage.absorb(outcome.usage);
        // Persist this turn's usage against the conversation, so `/cost` can
        // report the whole session rather than just this process (§7).
        if let Err(err) = state
            .store
            .record_usage(&state.session_id, outcome.usage)
            .await
        {
            eprintln!("imp: could not record token usage: {err}");
        }

        drop(sender);
        let _ = rendering.await;

        // Persist what the turn added, including a partial turn after Ctrl-C.
        // Compaction may have replaced messages *before* `reply_start`, so the
        // index has to lose what it removed or the turn would be written twice:
        // once from the shifted position, once from the old one.
        let reply_start = reply_start.saturating_sub(outcome.compacted);
        if let Err(err) = state.persist_since(reply_start).await {
            eprintln!("imp: could not save the assistant reply: {err}");
        }

        if outcome.stop != StopReason::Completed {
            eprintln!("… turn ended: {}", outcome.stop.as_str());
        }
    }

    // The external servers belong to the session, so they go with it.
    state.shutdown().await;
    Ok(())
}

/// Start the in-process scheduler for this session, reporting what it did.
///
/// Failure is not fatal: a scheduler that cannot start must not take the REPL
/// with it, since the user is still there to work. It is loud on stderr instead.
async fn start_cron(config: &Config, state: &Session) -> Option<crate::cron::CronService> {
    match crate::cron::start(config, &state.store, state.cron_runner.clone()).await {
        Ok(Some((service, startup))) => {
            if startup.reconciled > 0 {
                eprintln!(
                    "… cron: closed {} run(s) left in flight by a previous process",
                    startup.reconciled
                );
            }
            let caught = startup.caught_up;
            if caught != imp_cron::TickReport::default() {
                eprintln!(
                    "… cron catch-up: {} fired, {} queued, {} skipped, {} overlapped",
                    caught.fired, caught.queued, caught.skipped, caught.overlapped
                );
            }
            Some(service)
        }
        Ok(None) => None,
        Err(err) => {
            eprintln!("imp: the cron scheduler did not start: {err}");
            None
        }
    }
}

/// Handle a `/command`. Returns `true` when the REPL should exit.
async fn handle_slash(
    command: &str,
    state: &mut Session,
    usage: &Usage,
    cli: &Cli,
    config: &Config,
) -> Result<bool> {
    let theme = style::stdout_theme(cli, config);
    let mut parts = command.splitn(2, char::is_whitespace);
    let name = parts.next().unwrap_or_default();
    let argument = parts
        .next()
        .map(str::trim)
        .filter(|value| !value.is_empty());

    match name {
        "quit" | "exit" | "q" => return Ok(true),
        "help" | "?" => print_help(theme),
        "tools" => {
            let risks = state.tools.risks();
            if risks.is_empty() {
                println!("{}", theme.dim("No tools are enabled."));
            }
            let width = risks.iter().map(|(tool, _)| tool.len()).max().unwrap_or(0);
            for (tool, risk) in risks {
                println!(
                    "  {} {:<width$}  {}",
                    theme.glyph(Glyph::Selected),
                    theme.bold(tool),
                    theme.risk(risk),
                );
            }
        }
        "auto" => {
            let message = auto_command(argument, &state.auto)?;
            println!("{} {}", theme.success("auto:"), theme.bold(&message));
        }
        "new" => {
            state.reset().await?;
            println!(
                "{} {}",
                theme.success("new session"),
                theme.info(&short_id(&state.session_id))
            );
        }
        "sessions" => {
            let rows = state.store.list_sessions(20).await?;
            if rows.is_empty() {
                println!(
                    "{}",
                    theme.dim("No conversations yet. Ask something to start one.")
                );
            }
            for row in rows {
                println!("  {}", setup::describe_session(&row, theme));
            }
        }
        "resume" => {
            let needle = argument
                .ok_or_else(|| Error::Config("usage: /resume <id> — see /sessions".to_string()))?;
            let id = session::resolve(&state.store, needle).await?;
            let history = state.store.load_messages(&id).await?;
            if history.is_empty() {
                return Err(Error::Config(format!("session `{needle}` has no messages")));
            }
            let mut history = history;
            setup::apply_history_window(&mut history, state.history_window);
            // Re-adopt the prompt so a resumed session behaves like a fresh one.
            let needs_system = history
                .first()
                .map(|message| message.role != imp_core::message::Role::System)
                .unwrap_or(true);
            if needs_system && let Some(system) = state.history.first().cloned() {
                history.insert(0, system);
            }
            state.adopt(&id);
            let count = history.len();
            state.history = history;
            println!(
                "{} {} {}",
                theme.success("resumed"),
                theme.info(&id),
                theme.dim(&format!("({count} messages)")),
            );
        }
        "rename" => {
            let title =
                argument.ok_or_else(|| Error::Config("usage: /rename <title>".to_string()))?;
            if !state.store.rename_session(&state.session_id, title).await? {
                return Err(Error::Config(
                    "this session is no longer in the database".to_string(),
                ));
            }
            println!("{} {}", theme.success("renamed to"), theme.bold(title));
        }
        "clear" => {
            // Forget the conversation, keep the system prompt and the session id
            // so the transcript stays continuous and resumable.
            let system = state.history.first().cloned();
            state.history = system.into_iter().collect();
            println!("{}", theme.success("history cleared"));
        }
        "model" => match argument {
            Some(model) => {
                state.options.model = model.to_string();
                println!(
                    "{} {}",
                    theme.success("model set to"),
                    theme.bold(&state.options.model)
                );
            }
            None => {
                println!(
                    "{} {}",
                    theme.dim("current model:"),
                    theme.bold(&state.options.model)
                );
                match state.list_models().await {
                    Ok(models) if models.is_empty() => {
                        println!("{}", theme.dim("the provider advertised no models"));
                    }
                    Ok(models) => {
                        println!(
                            "{}",
                            theme.dim(&format!("available models ({}):", models.len()))
                        );
                        for model in &models {
                            if *model == state.options.model {
                                println!(
                                    "  {} {}",
                                    theme.glyph(Glyph::Selected),
                                    theme.bold(model)
                                );
                            } else {
                                println!("    {model}");
                            }
                        }
                    }
                    Err(err) => {
                        println!("{}", theme.warn(&format!("could not list models: {err}")));
                    }
                }
            }
        },
        "cost" => {
            let total = state.store.session_usage(&state.session_id).await?;
            println!(
                "{} prompt {} · completion {} · total {} tokens over {} turn(s)",
                theme.dim("session:"),
                theme.info(&total.prompt_tokens.to_string()),
                theme.info(&total.completion_tokens.to_string()),
                theme.bold(&total.total_tokens.to_string()),
                total.turns,
            );
            println!(
                "{} prompt {} · completion {} · total {} tokens",
                theme.dim("process:"),
                theme.info(&usage.prompt_tokens.to_string()),
                theme.info(&usage.completion_tokens.to_string()),
                theme.bold(&usage.total_tokens.to_string()),
            );
        }
        "context" => {
            let schemas = state.tools.schemas();
            let tools = imp_core::tokens::estimate_tools(&schemas);
            let system = state
                .history
                .first()
                .map(imp_core::tokens::estimate_message)
                .unwrap_or(0);
            let history = imp_core::tokens::estimate_messages(&state.history);
            let used = history + tools;

            let window = config
                .provider
                .context_window
                .or_else(|| state.context.as_ref().and_then(|info| info.context_window));
            match window {
                Some(window) => {
                    let source = match (&state.context, config.provider.context_window) {
                        (_, Some(_)) => "configured".to_string(),
                        (Some(info), None) => info.describe(),
                        (None, None) => "not discovered".to_string(),
                    };
                    println!(
                        "{} {} window  ({})",
                        theme.dim("context"),
                        theme.bold(&window.to_string()),
                        theme.dim(&source),
                    );
                }
                None => println!(
                    "{}",
                    theme.dim(
                        "context: the endpoint reported no window — \
                         set provider.context_window or agent.context_tokens"
                    )
                ),
            }

            println!(
                "          {} fixed     (system {} + tools {})",
                theme.info(&(system + tools).to_string()),
                system,
                tools
            );
            println!("          {} history", theme.info(&history.to_string()));

            match state.context_budget {
                Some(budget) => {
                    let percent = (used as f32 / budget as f32 * 100.0).round() as u32;
                    let threshold = (budget as f32 * state.options.compact_threshold) as usize;
                    println!(
                        "          {} used     {}% · compact at {}",
                        theme.bold(&used.to_string()),
                        percent,
                        threshold
                    );
                    println!(
                        "          {}",
                        theme.dim(&format!(
                            "budget {budget} · compact {} · keep {} turn(s)",
                            if state.options.compact { "on" } else { "off" },
                            state.options.compact_keep_turns,
                        ))
                    );
                }
                None => println!(
                    "          {} used     no budget: nothing trims and nothing compacts",
                    theme.bold(&used.to_string())
                ),
            }
        }
        "compact" => match argument {
            Some("off") => {
                state.options.compact = false;
                println!(
                    "{} {}",
                    theme.success("compaction off:"),
                    theme.dim("history is trimmed when it no longer fits")
                );
            }
            Some("on") => {
                state.options.compact = true;
                println!(
                    "{} {}",
                    theme.success("compaction on:"),
                    theme.dim(&format!(
                        "at {}% of the budget, keeping the last {} turn(s)",
                        (state.options.compact_threshold * 100.0).round() as u32,
                        state.options.compact_keep_turns,
                    ))
                );
            }
            Some(other) => {
                println!(
                    "{}",
                    theme.dim(&format!("compact: unknown option `{other}`; use on or off"))
                );
            }
            None => match state.compact_now(true).await? {
                Some((replaced, before, after)) => println!(
                    "{} {} messages → one summary ({} → {} tokens)",
                    theme.success("compacted:"),
                    replaced,
                    before,
                    after
                ),
                None => println!(
                    "{}",
                    theme.dim("nothing to compact: no older turn worth replacing")
                ),
            },
        },
        "session" => {
            println!("{}", theme.info(&state.session_id));
            println!(
                "{}",
                theme.dim("This is the conversation id substituted into any ${session} header.")
            );
        }
        "where" => println!("{}", theme.info(&state.store.path().display().to_string())),
        "cron" | "jobs" => {
            let jobs = state.store.jobs().list_jobs().await?;
            if jobs.is_empty() {
                println!(
                    "{}",
                    theme.dim("No jobs scheduled. Add one with `imp cron add` or ask the agent.")
                );
            }
            for job in jobs {
                println!("  {}", theme.bold(&imp_cron::describe(&job)));
            }
        }
        other => {
            let _ = cli;
            eprintln!(
                "{} {}",
                theme.warn(&format!("unknown command `/{other}`")),
                theme.dim("— try /help")
            );
        }
    }
    Ok(false)
}

/// Resolve `/auto [mode]` against the session switch.
///
/// With no argument it toggles the read-only-command shortcut. `on`/`read-only`
/// and `off` are explicit. `full` is reserved for a future System One mode and
/// is refused rather than silently ignored.
fn auto_command(argument: Option<&str>, auto: &AutoCommands) -> Result<String> {
    match argument {
        None => {
            let next = if auto.read_only() {
                AutoMode::Off
            } else {
                AutoMode::ReadOnly
            };
            auto.set_mode(next);
        }
        Some("on") | Some("read-only") | Some("readonly") => auto.set_mode(AutoMode::ReadOnly),
        Some("off") => auto.set_mode(AutoMode::Off),
        Some("full") => {
            return Err(Error::Config(
                "full auto is not implemented yet; `/auto` enables read-only commands".to_string(),
            ));
        }
        Some(other) => {
            return Err(Error::Config(format!(
                "unknown mode `{other}` — use /auto, /auto on, or /auto off"
            )));
        }
    }
    Ok(match auto.mode() {
        AutoMode::ReadOnly => "read-only commands run without approval".to_string(),
        AutoMode::Off => "read-only commands need approval again".to_string(),
        AutoMode::Full => "full (reserved)".to_string(),
    })
}

/// A grouped, coloured `/help`.
fn print_help(theme: Theme) {
    let head = |title: &str| println!("{}", theme.accent(title));
    let row = |name: &str, description: &str| {
        println!(
            "  {} {}",
            theme.info(&format!("{name:<18}")),
            theme.dim(description)
        );
    };

    head("Conversation");
    row("/new", "start a fresh conversation");
    row("/clear", "forget the messages, keep the session id");
    row("/sessions", "list stored conversations");
    row("/resume <id>", "continue a stored conversation");
    row("/rename <title>", "set this conversation's title");
    head("Model");
    row("/model", "list the provider's models");
    row("/model <name>", "switch the model for this session");
    head("Session");
    row("/tools", "enabled tools, risk class and approval");
    row("/cron", "scheduled jobs (alias /jobs)");
    row("/cost", "token usage for this session");
    row("/context", "the window, the budget and the fixed cost");
    row(
        "/compact [on|off]",
        "summarize older turns now, or toggle it",
    );
    row("/session", "the id sent to the provider as ${session}");
    row("/where", "the database path");
    head("Approval");
    row("/auto [on|off]", "run read-only commands without approval");
    head("Other");
    row("/help", "this message");
    row("/quit", "exit (or press Ctrl-C twice)");
    row(
        "!<command>",
        "run a shell command directly, bypassing the model",
    );
}

/// Print a resumed conversation, so continuing shows its context.
///
/// It is shaped like the live REPL: the user's prompts carry the `›` prompt and
/// the assistant's replies are printed bare, so a resumed session reads as one
/// continuous scrollback rather than a transcript. An assistant turn's tool calls
/// are shown as one indented line each; tool *results* are the model's context
/// rather than the conversation and are left out. The system prompt is never
/// shown.
fn print_history(history: &[Message], theme: Theme) {
    let messages: Vec<&Message> = history
        .iter()
        .filter(|message| message.role != Role::System && message.role != Role::Tool)
        .collect();
    if messages.is_empty() {
        return;
    }
    println!(
        "{}",
        theme.dim(&format!(
            "— {} message(s) from this conversation —",
            messages.len()
        ))
    );
    let glyph = theme.glyph(Glyph::Prompt);
    let prompt = theme.accent(if glyph.is_empty() { ">" } else { glyph });
    for message in messages {
        match message.role {
            Role::User => {
                if let Some(text) = &message.content {
                    println!("{prompt} {text}");
                }
            }
            Role::Assistant => {
                if let Some(text) = &message.content {
                    println!("{text}");
                }
                if let Some(calls) = &message.tool_calls {
                    for call in calls {
                        println!(
                            "  {} {} {}",
                            theme.dim(theme.glyph(Glyph::Tool)),
                            theme.bold(&call.function.name),
                            theme.dim(&call.function.arguments)
                        );
                    }
                }
            }
            Role::Tool | Role::System => continue,
        }
    }
}

/// Run a local shell command, bypassing the model entirely.
async fn shell_escape(command: &str, root: &Path) {
    if command.trim().is_empty() {
        return;
    }
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
    let status = tokio::process::Command::new(shell)
        .arg("-c")
        .arg(command)
        .current_dir(root)
        .status()
        .await;

    match status {
        Ok(status) if !status.success() => {
            eprintln!("(exit {})", status.code().unwrap_or(-1));
        }
        Ok(_) => {}
        Err(err) => eprintln!("imp: cannot run `{command}`: {err}"),
    }
}

/// Replace the cancellation token after each interrupt so turns are independent.
fn spawn_interrupt_swapper(slot: Arc<Mutex<CancellationToken>>) {
    tokio::spawn(async move {
        while tokio::signal::ctrl_c().await.is_ok() {
            let mut guard = slot.lock().await;
            guard.cancel();
            *guard = CancellationToken::new();
        }
    });
}

fn decision_name(decision: Decision) -> &'static str {
    match decision {
        Decision::Auto => "auto",
        Decision::Ask => "ask",
        Decision::Deny => "deny",
    }
}

/// First few characters of a session id, for a compact banner.
fn short_id(session_id: &str) -> String {
    session_id.chars().take(8).collect()
}

/// Tracks the "press Ctrl-C again to exit" gesture.
///
/// The first interrupt arms the exit and prints a hint; a second, consecutive
/// interrupt exits. Any line the user actually submits re-arms the first press,
/// so the gesture is "twice in a row" and not a sticky session-wide state. The
/// decision is a pure function so it can be tested without a pty.
#[derive(Debug, Default)]
struct InterruptGuard {
    armed: bool,
}

impl InterruptGuard {
    /// A line was read: the next interrupt is a fresh first press.
    fn on_line(&mut self) {
        self.armed = false;
    }

    /// A Ctrl-C arrived. Returns `true` when the REPL should exit.
    fn on_interrupt(&mut self) -> bool {
        if self.armed {
            return true;
        }
        self.armed = true;
        false
    }
}

#[cfg(test)]
mod tests {
    use super::{InterruptGuard, auto_command};
    use imp_core::read_only::{AutoCommands, AutoMode};

    #[test]
    fn one_interrupt_arms_and_the_next_exits() {
        let mut guard = InterruptGuard::default();

        assert!(!guard.on_interrupt(), "the first press only warns");
        assert!(guard.on_interrupt(), "the second consecutive press exits");
    }

    #[test]
    fn reading_a_line_re_arms_the_first_press() {
        let mut guard = InterruptGuard::default();

        assert!(!guard.on_interrupt());
        guard.on_line();
        assert!(
            !guard.on_interrupt(),
            "after a submitted line, the next press warns again"
        );
        assert!(guard.on_interrupt());
    }

    #[test]
    fn auto_toggles_and_sets_explicitly() {
        let auto = AutoCommands::new(false, Vec::new());

        auto_command(None, &auto).unwrap();
        assert_eq!(auto.mode(), AutoMode::ReadOnly);

        auto_command(None, &auto).unwrap();
        assert_eq!(auto.mode(), AutoMode::Off);

        auto_command(Some("read-only"), &auto).unwrap();
        assert_eq!(auto.mode(), AutoMode::ReadOnly);

        auto_command(Some("off"), &auto).unwrap();
        assert_eq!(auto.mode(), AutoMode::Off);
    }

    #[test]
    fn full_auto_is_refused_for_now() {
        let auto = AutoCommands::new(false, Vec::new());
        let err = auto_command(Some("full"), &auto).unwrap_err();
        assert!(err.to_string().contains("not implemented"), "was: {err}");
        assert_eq!(auto.mode(), AutoMode::Off, "a refused mode changes nothing");
    }

    #[test]
    fn an_unknown_auto_mode_is_refused() {
        let auto = AutoCommands::new(false, Vec::new());
        assert!(auto_command(Some("maybe"), &auto).is_err());
    }
}
