//! Session assembly: store, provider, tools, transcript, and the system prompt.

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;

use imp_core::agent::{Agent, AgentEvent, AgentOptions, StopReason};
use imp_core::clock::SystemClock;
use imp_core::compact::{Summarizer, SummaryRole};
use imp_core::config::{Config, ContextProbe, Decision};
use imp_core::error::{Error, Result};
use imp_core::guard::GuardThresholds;
use imp_core::job::{Job, SessionMode};
use imp_core::message::{Message, Role};
use imp_core::new_session_id;
use imp_core::policy::{PolicyEngine, RecordingGate, ToolGate};
use imp_core::provider::Provider;
use imp_core::read_only::AutoCommands;
use imp_core::tool::ToolRegistry;
use imp_cron::RunReport;
use imp_guard::HttpSystemOneGuard;
use imp_mcp::{McpServers, OnStart, PeerAsk, peer_policies, peer_tools};
use imp_provider::{ModelInfo, OpenAiProvider};
use imp_store::{NewSession, SessionRow, Store};
use imp_tools::{CronContext, ToolConfig, default_registry};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::approval;
use crate::cli::Cli;
use crate::style::Theme;
use crate::summarize::ProviderSummarizer;

/// Everything one interactive or one-shot run needs.
pub struct Session {
    /// Model backend, shared with every [`Agent`] built from this session.
    pub provider: Arc<dyn Provider>,
    /// Tools offered to the model.
    pub tools: Arc<ToolRegistry>,
    /// Loop settings. `model` is mutable so `/model` can retarget a session.
    pub options: AgentOptions,
    /// Conversation so far. Index 0 is always the system prompt.
    pub history: Vec<Message>,
    /// Canonical workspace root.
    pub workspace_root: PathBuf,
    /// Identifier of the current conversation. Doubles as the `${session}`
    /// value sent to gateways, so resuming keeps the same id.
    pub session_id: String,
    /// Transcript store.
    pub store: Arc<Store>,
    /// How many trailing messages to keep when a transcript is reloaded.
    pub history_window: usize,
    /// Runs a scheduled job's prompt. Wired to the real agent, always
    /// non-interactively; the scheduler never builds an agent itself.
    pub cron_runner: Arc<dyn imp_cron::JobRunner>,
    /// What the backend reported about the model, for `/context` and `doctor`.
    pub context: Option<ModelInfo>,
    /// Token budget the loop actually enforces, after discovery.
    pub context_budget: Option<usize>,
    /// Model that writes compaction summaries; the session's unless configured.
    summary_model: String,
    /// Retained so the provider can be rebuilt when the conversation changes.
    spec: ProviderSpec,
    /// The approval gate, shared so session-scoped allows accumulate.
    gate: Arc<dyn ToolGate>,
    /// The read-only-command switch `/auto` flips. Shared with the gate, so a
    /// REPL command changes what the next `check` sees.
    pub auto: Arc<AutoCommands>,
    /// The external servers this session consumes, shared so a turn can retry
    /// one that was down and the catalogue follows.
    mcp: Arc<McpServers>,
    /// The peer delegation tools, kept so their connections close on the way
    /// out. A stdio peer is a child process; an HTTP one is a socket.
    peers: Vec<Arc<PeerAsk>>,
}

/// The parameters a provider was built from, kept to rebuild it later.
#[derive(Clone)]
struct ProviderSpec {
    base_url: String,
    api_key: Option<String>,
    max_retries: u32,
    request_timeout: Duration,
    usage_in_stream: bool,
    strict_tool_arguments: bool,
    headers: Vec<(String, String)>,
    stream: bool,
    stream_idle_timeout: Duration,
    sanitize_schemas: bool,
    omit_parallel_tool_calls: bool,
    omit_tool_choice: bool,
    empty_assistant_content: bool,
    extra_body: serde_json::Map<String, serde_json::Value>,
}

impl ProviderSpec {
    fn build(&self, session_id: &str) -> OpenAiProvider {
        OpenAiProvider::new(&self.base_url, self.api_key.clone().unwrap_or_default())
            .with_max_retries(self.max_retries)
            .with_request_timeout(Some(self.request_timeout))
            .with_usage_in_stream(self.usage_in_stream)
            .with_strict_tool_arguments(self.strict_tool_arguments)
            .with_headers(self.headers.clone())
            .with_session_id(session_id.to_string())
            .with_stream(self.stream)
            .with_stream_idle_timeout(self.stream_idle_timeout)
            .with_sanitize_schemas(self.sanitize_schemas)
            .with_omit_parallel_tool_calls(self.omit_parallel_tool_calls)
            .with_omit_tool_choice(self.omit_tool_choice)
            .with_empty_assistant_content(self.empty_assistant_content)
            .with_extra_body(self.extra_body.clone())
    }
}

impl Session {
    /// Build an [`Agent`] over this session's shared pieces.
    pub fn agent(&self) -> Agent {
        Agent::new(
            self.provider.clone(),
            self.tools.clone(),
            self.options.clone(),
        )
        .with_gate(self.gate.clone())
        .with_session(self.session_id.clone())
        .with_summarizer(self.summarizer())
    }

    /// The summarizer compaction uses, built from the *current* provider.
    ///
    /// Rebuilt per agent rather than stored, so `adopt` — which changes the
    /// conversation and therefore the `${session}` header — cannot leave a
    /// summarizer talking through the previous conversation's client.
    fn summarizer(&self) -> Arc<dyn Summarizer> {
        Arc::new(ProviderSummarizer::new(
            self.provider.clone(),
            self.summary_model.clone(),
        ))
    }

    /// Point this session at a different conversation.
    ///
    /// The provider is rebuilt so the `${session}` header matches the new
    /// conversation; reusing the old one would make two conversations look
    /// identical to the gateway and defeat prompt-cache routing.
    pub fn adopt(&mut self, session_id: &str) {
        self.provider = Arc::new(self.spec.build(session_id));
        self.session_id = session_id.to_string();
    }

    /// Start a fresh conversation in the same session object.
    pub async fn reset(&mut self) -> Result<()> {
        let system = self.history.first().cloned();
        self.adopt(&new_session_id());
        self.store
            .create_session(NewSession {
                id: self.session_id.clone(),
                cwd: self.workspace_root.display().to_string(),
                model: Some(self.options.model.clone()),
                provider: Some(self.spec.base_url.clone()),
            })
            .await?;
        self.history = system.into_iter().collect();
        self.persist_all().await
    }

    /// Persist everything from `start` onwards, and title on first prompt.
    pub async fn persist_since(&self, start: usize) -> Result<()> {
        if start >= self.history.len() {
            return Ok(());
        }
        self.store
            .append_messages(&self.session_id, &self.history[start..])
            .await?;
        if let Some(prompt) = self.first_user_text() {
            self.store
                .title_from_first_prompt(&self.session_id, &prompt)
                .await?;
        }
        Ok(())
    }

    /// Persist the whole transcript, for a freshly created session.
    pub async fn persist_all(&self) -> Result<()> {
        self.store
            .append_messages(&self.session_id, &self.history)
            .await
    }

    /// Contact every configured MCP server, and return a `system` notice for
    /// each one whose state changed.
    ///
    /// Called at the start of every turn, which is both how a server that came
    /// up reaches the catalogue and how one that fell over leaves it (§5.10).
    /// A turn that changes nothing produces no notice, so the transcript does
    /// not repeat itself.
    pub async fn refresh_mcp(&self) -> Vec<String> {
        // A configured-but-lazy server is contacted here too: the first turn is
        // the "first use" its config deferred to, and this is where the tools
        // it publishes join the catalogue.
        self.mcp.refresh(OnStart::All).await
    }

    /// Close the external servers and any peer connection on the way out.
    pub async fn shutdown(&self) {
        self.mcp.shutdown().await;
        for peer in &self.peers {
            peer.close().await;
        }
    }

    /// The models the session's backend advertises, via `GET {base_url}/models`.
    ///
    /// `/model` uses this to show what the current provider offers. It rebuilds
    /// the same client the session already talks through — from the retained
    /// [`ProviderSpec`] — so it needs no second provider abstraction and cannot
    /// drift from the request path's base URL, key or headers.
    pub async fn list_models(&self) -> Result<Vec<String>> {
        OpenAiProvider::new(
            &self.spec.base_url,
            self.spec.api_key.clone().unwrap_or_default(),
        )
        .with_request_timeout(Some(self.spec.request_timeout))
        .with_max_retries(1)
        .with_headers(self.spec.headers.clone())
        .with_session_id(self.session_id.clone())
        .list_models()
        .await
    }

    /// Replace the oldest turns with a summary, on demand.
    ///
    /// Returns `(messages replaced, tokens before, tokens after)` when something
    /// was compacted, and `None` when there was nothing to replace. `force` is
    /// `/compact`: it ignores the threshold, because the user asked, but the
    /// same boundaries apply — the prompt survives and no turn is split.
    ///
    /// Only the in-memory history changes. The stored transcript keeps every
    /// message (R3), so a resume sees the whole conversation and compacts again
    /// if it needs to.
    pub async fn compact_now(&mut self, force: bool) -> Result<Option<(usize, usize, usize)>> {
        let keep = self.options.compact_keep_turns;
        let plan = match (self.context_budget, force) {
            (Some(budget), false) => {
                let tools = imp_core::tokens::estimate_tools(&self.tools.schemas());
                imp_core::compact::plan(&self.history, budget.saturating_sub(tools), keep)
            }
            (_, true) => imp_core::compact::plan_forced(&self.history, keep),
            (None, false) => None,
        };
        let Some(plan) = plan else {
            return Ok(None);
        };

        let before = imp_core::tokens::estimate_messages(&self.history);
        let replaced = plan.len();
        let replaced_tokens =
            imp_core::tokens::estimate_messages(&self.history[plan.from..plan.to]);
        let summary = self
            .summarizer()
            .summarize(&self.history[plan.from..plan.to], CancellationToken::new())
            .await?;
        let message = imp_core::compact::summary_message(self.options.summary_role, &summary);
        // Asking for a summary that costs more than what it replaces would make
        // the context worse, so a forced compaction refuses it too.
        if imp_core::tokens::estimate_message(&message) >= replaced_tokens {
            return Ok(None);
        }
        self.history.splice(plan.from..plan.to, [message]);
        let after = imp_core::tokens::estimate_messages(&self.history);
        Ok(Some((replaced, before, after)))
    }

    /// The text of the first user message, used to derive a title.
    fn first_user_text(&self) -> Option<String> {
        self.history
            .iter()
            .find(|message| message.role == Role::User)
            .and_then(|message| message.content.clone())
    }
}

/// Assemble a session, optionally resuming a stored conversation.
pub async fn build(
    cli: &Cli,
    config: &Config,
    cwd: &Path,
    resume: Option<&str>,
) -> Result<Session> {
    // An empty `api_key_env` means the backend needs no credentials.
    let api_key = config.api_key()?;
    let spec = ProviderSpec {
        base_url: config.provider.base_url.clone(),
        api_key,
        max_retries: config.provider.max_retries,
        request_timeout: Duration::from_secs(config.provider.request_timeout_secs),
        usage_in_stream: config.provider.supports_usage_in_stream,
        strict_tool_arguments: config.provider.strict_tool_arguments,
        headers: config.request_headers(),
        stream: config.provider.stream,
        stream_idle_timeout: Duration::from_secs(config.provider.stream_idle_timeout_secs),
        sanitize_schemas: config.provider.quirks.sanitize_schemas,
        omit_parallel_tool_calls: config.provider.quirks.omit_parallel_tool_calls,
        omit_tool_choice: config.provider.quirks.omit_tool_choice,
        empty_assistant_content: config.provider.quirks.empty_assistant_content,
        extra_body: config.provider.extra_body.clone(),
    };

    let workspace_root = config.workspace_root(cwd)?;

    // The store is opened before the registry because the memory tools need a
    // handle on it, and the system prompt advertises the registered tools, so
    // the order is store -> tools -> prompt.
    let database = cli.db.clone().unwrap_or_else(|| config.database_path());
    let store = Arc::new(Store::open(&database).await?);

    // External servers are contacted before the prompt is built so an eager one
    // is advertised in it, and before the gate so their approval policies are
    // part of the engine rather than bolted on afterwards.
    let mcp = McpServers::new(&config.mcp.client, config.exec.output_cap_bytes);

    let mut registry = default_registry(&tool_config(config), store.clone(), cron_context(config));
    registry.attach_catalog(mcp.clone());
    // One delegation tool per peer, `peer__<name>_ask` (M10.2). Registered
    // rather than catalogued: its name and schema come from config, so it is on
    // offer from the moment the session exists. The handles are kept so their
    // connections close with the session.
    let peers: Vec<Arc<PeerAsk>> = peer_tools(&config.peers)
        .into_iter()
        .map(Arc::new)
        .collect();
    for peer in &peers {
        registry.register_arc(peer.clone());
    }
    // The `[tools]` selection trims what the model is offered. It is applied to
    // the finished registry — catalogs included — and never touches the gate:
    // `tools::ToolSelection` only removes entries, so it cannot widen a
    // permission the policy engine would refuse.
    registry.select(config.tools.selection());
    let tools = Arc::new(registry);

    let mcp_notices = mcp.refresh(OnStart::Eager).await;
    let system = Message::system(system_prompt(config, &workspace_root, &tools));

    let (session_id, mut history) = match resume {
        Some(id) => {
            let row = store
                .session(id)
                .await?
                .ok_or_else(|| Error::Config(format!("no session `{id}` in the database")))?;
            let mut history = store.load_messages(id).await?;
            if history.is_empty() {
                history.push(system);
            }
            (row.id, history)
        }
        None => {
            let id = new_session_id();
            store
                .create_session(NewSession {
                    id: id.clone(),
                    cwd: workspace_root.display().to_string(),
                    model: Some(config.provider.model.clone()),
                    provider: Some(config.provider.base_url.clone()),
                })
                .await?;
            (id, vec![system])
        }
    };

    apply_history_window(&mut history, config.agent.history_window);
    let history_window = config.agent.history_window;

    // A server that could not be contacted is explained as a `system` message,
    // after the prompt so index 0 stays the prompt (§5.10).
    for notice in mcp_notices {
        history.push(Message::system(notice));
    }

    let provider = Arc::new(spec.build(&session_id));
    tracing::debug!(
        session_id = %session_id,
        base_url = %config.provider.base_url,
        history = history.len(),
        "session ready"
    );

    // What the model can actually hold. An explicit `context_window` skips the
    // probe; otherwise the endpoint is asked, and an endpoint that answers
    // nothing leaves the budget unset — which is exactly the behaviour of the
    // version that had no probe at all.
    let context = match config.provider.context_probe {
        ContextProbe::Off => None,
        ContextProbe::Auto if config.provider.context_window.is_some() => None,
        ContextProbe::Auto => provider.probe_context(&config.provider.model).await,
    };
    let context_budget = effective_budget(config, context.as_ref());

    let options = AgentOptions {
        model: config.provider.model.clone(),
        max_iterations: config.agent.max_iterations,
        temperature: config.provider.temperature,
        max_tokens: config.agent.max_tokens,
        parallel_tool_calls: Some(config.provider.parallel_tool_calls),
        include_usage: config.provider.supports_usage_in_stream,
        max_tool_calls_per_turn: config.agent.max_tool_calls_per_turn,
        context_tokens: context_budget,
        tool_result_chars: config.agent.tool_result_chars,
        compact: config.agent.compact,
        compact_threshold: config.agent.compact_threshold,
        compact_keep_turns: config.agent.compact_keep_turns,
        summary_role: summary_role(config),
        nudge_on_empty: config.agent.nudge_on_empty,
        repair_arguments: config.agent.repair_arguments,
        workspace_root: workspace_root.clone(),
    };

    // The families a per-server (or per-peer) policy names. Both go into the same
    // list, so the longest matching prefix still decides (D21).
    let mut families = mcp.policy_families();
    families.extend(peer_policies(&config.peers));

    // Whether anyone can answer a prompt. `--yes` forces the permissive path;
    // a non-TTY still cannot be prompted, so it falls back to `noninteractive`.
    let tty = std::io::IsTerminal::is_terminal(&std::io::stdin());
    // The read-only-command shortcut is off by default; `/auto` flips this at
    // runtime, so the same handle goes to the gate and the session.
    let auto = AutoCommands::new(
        config.policy.read_only.enabled,
        config.policy.read_only.extra.clone(),
    );
    let gate = recording(
        build_gate(
            config,
            &store,
            &workspace_root.display().to_string(),
            tty,
            cli.yes,
            cli.deny,
            families.clone(),
            crate::style::stderr_theme(cli, config),
            auto.clone(),
        ),
        &store,
    );

    // Cron runs through a *different* gate, on purpose: a job prompt has no
    // terminal, so it must take the non-interactive decision and can never be
    // approved. See `build_cron_gate`.
    let cron_runner: Arc<dyn imp_cron::JobRunner> = Arc::new(JobAgentRunner {
        spec: spec.clone(),
        tools: tools.clone(),
        gate: recording(
            build_cron_gate(
                config,
                &store,
                &workspace_root.display().to_string(),
                families,
            ),
            &store,
        ),
        config: config.clone(),
        store: store.clone(),
        history_window,
        mcp: mcp.clone(),
    });

    let session = Session {
        provider,
        tools,
        options,
        history,
        workspace_root,
        session_id,
        store,
        history_window,
        cron_runner,
        context,
        context_budget,
        summary_model: summary_model(config),
        spec,
        gate,
        auto,
        mcp,
        peers,
    };
    if resume.is_none() {
        session.persist_all().await?;
    }
    Ok(session)
}

/// Wrap an engine so every decision it makes is written to the audit trail.
///
/// §7 asks for a row per tool decision, and the engine writes one only where a
/// guard verdict was involved. The decorator is applied here rather than inside
/// the engine so the rule order stays untouched — see [`RecordingGate`].
fn recording(engine: Arc<PolicyEngine>, store: &Arc<Store>) -> Arc<dyn ToolGate> {
    RecordingGate::arc(engine, store.approvals())
}

/// The `[workspace]`, `[exec]` and `[http_fetch]` values that shape the tool set.
pub(crate) fn tool_config(config: &Config) -> ToolConfig {
    ToolConfig {
        max_file_bytes: config.workspace.max_file_bytes,
        shell: config.exec.shell.clone(),
        default_timeout: Duration::from_secs(config.exec.default_timeout_secs),
        max_timeout: Duration::from_secs(config.exec.max_timeout_secs),
        output_cap_bytes: config.exec.output_cap_bytes,
        http_fetch: config.http_fetch.clone(),
    }
}

/// Generation room reserved when a budget is derived from a discovered window.
///
/// A model can overflow *while producing* its answer, not only on the prompt: a
/// reasoning model spends its window before it emits a token of text. Reserving
/// only for the prompt leaves that failure open, which is how a turn can die
/// with an empty reply on a cold local model.
const DEFAULT_GENERATION_RESERVE: usize = 512;

/// The token budget the loop should enforce.
///
/// Precedence: an explicit `agent.context_tokens` wins, then the window
/// discovered from the endpoint, then `provider.context_window`. The reserve is
/// generation room plus a 5% margin for the tokenizer's own framing, which the
/// character heuristic does not model.
pub(crate) fn effective_budget(config: &Config, context: Option<&ModelInfo>) -> Option<usize> {
    if let Some(tokens) = config.agent.context_tokens {
        return Some(tokens);
    }
    let window = config
        .provider
        .context_window
        .or_else(|| context.and_then(|info| info.context_window))?;
    let generation = config
        .agent
        .max_tokens
        .map(|tokens| tokens as usize)
        .unwrap_or(0)
        .max(DEFAULT_GENERATION_RESERVE);
    Some(
        window
            .saturating_sub(generation)
            .saturating_sub(window / 20)
            .max(1),
    )
}

/// Model that writes compaction summaries.
pub(crate) fn summary_model(config: &Config) -> String {
    config
        .agent
        .compact_model
        .clone()
        .unwrap_or_else(|| config.provider.model.clone())
}

/// Role a summary travels as, honouring the strict-template quirk.
pub(crate) fn summary_role(config: &Config) -> SummaryRole {
    if config.provider.quirks.summary_as_user {
        SummaryRole::User
    } else {
        SummaryRole::System
    }
}
/// What the `cron_*` tools need from the process: a clock, and the default zone.
pub(crate) fn cron_context(config: &Config) -> CronContext {
    CronContext {
        clock: Arc::new(SystemClock),
        timezone: config.cron.timezone.clone(),
    }
}

/// The approval gate, wired from the policy config and the store.
///
/// `--yes` replaces the default decision rather than the whole engine, so deny
/// rules and the command classifier keep applying: it means "do not ask me",
/// not "do whatever you like".
#[allow(clippy::too_many_arguments)]
fn build_gate(
    config: &Config,
    store: &Arc<Store>,
    scope: &str,
    tty: bool,
    yes: bool,
    deny: bool,
    policies: Vec<imp_core::policy::ToolPolicy>,
    theme: Theme,
    auto: Arc<AutoCommands>,
) -> Arc<PolicyEngine> {
    let allow = config
        .policy
        .allow
        .iter()
        .map(|rule| (rule.tool.clone(), rule.pattern.clone()))
        .collect();
    let deny_rules = config
        .policy
        .deny
        .iter()
        .map(|rule| (rule.tool.clone(), rule.pattern.clone()))
        .collect();

    let default = if deny {
        Decision::Deny
    } else if yes {
        Decision::Auto
    } else {
        config.policy.default
    };
    // A flag cannot conjure a prompt, so a non-TTY stays fail-closed unless
    // the user explicitly passed --yes.
    let noninteractive = if yes {
        Decision::Auto
    } else if deny {
        Decision::Deny
    } else {
        config.policy.noninteractive
    };

    let engine = PolicyEngine::new(
        allow,
        deny_rules,
        default,
        noninteractive,
        scope.to_string(),
        tty,
    )
    .with_ui(approval::ui_for(tty, theme))
    .with_store(store.approvals())
    .with_tool_policies(policies)
    .with_auto_commands(auto);

    // The optional System One guard (SDD §5.6, D16). Disabled by default; when
    // enabled it can only shorten a prompt into a silent allow, and it is never
    // consulted off a terminal because there is no prompt to resolve there.
    let engine = if config.guard.enabled {
        engine.with_guard(
            Arc::new(HttpSystemOneGuard::new(&config.guard)),
            GuardThresholds::from(&config.guard),
        )
    } else {
        engine
    };

    Arc::new(engine)
}

/// The gate a scheduled job's prompt runs behind.
///
/// It is built with `interactive = false` and **no approval UI at all**, which
/// is the whole point: §5.7 says a job prompt cannot prompt for approval, so a
/// tool that would `ask` is decided by `policy.noninteractive` instead —
/// `deny` by default. A job therefore cannot promote an `ask` tool to `auto`,
/// and a job cannot reach the System One guard either, because the guard only
/// ever resolves a *prompt* and there is no prompt here.
///
/// Deny rules and allow rules still apply first, so an allowlisted command runs
/// and a denied one is still denied: this narrows nothing and widens nothing
/// except the absence of a human.
///
/// The read-only-command shortcut is built from the config, never from the
/// session handle `/auto` flips, so an interactive toggle never reaches a job.
pub fn build_cron_gate(
    config: &Config,
    store: &Arc<Store>,
    scope: &str,
    policies: Vec<imp_core::policy::ToolPolicy>,
) -> Arc<PolicyEngine> {
    let allow = config
        .policy
        .allow
        .iter()
        .map(|rule| (rule.tool.clone(), rule.pattern.clone()))
        .collect();
    let deny = config
        .policy
        .deny
        .iter()
        .map(|rule| (rule.tool.clone(), rule.pattern.clone()))
        .collect();

    Arc::new(
        PolicyEngine::new(
            allow,
            deny,
            config.policy.default,
            config.policy.noninteractive,
            scope.to_string(),
            false,
        )
        .with_store(store.approvals())
        .with_tool_policies(policies)
        .with_auto_commands(AutoCommands::new(
            config.policy.read_only.enabled,
            config.policy.read_only.extra.clone(),
        )),
    )
}

/// Runs a job's prompt as one real agent turn.
///
/// This is the `imp-cron` [`JobRunner`](imp_cron::JobRunner) seam filled
/// in with the agent loop. It owns the session bookkeeping a job needs — a fresh
/// session per run for `new`, the job's own session for `reuse` — because that
/// is knowledge about conversations, not about scheduling.
struct JobAgentRunner {
    spec: ProviderSpec,
    tools: Arc<ToolRegistry>,
    gate: Arc<dyn ToolGate>,
    config: Config,
    store: Arc<Store>,
    history_window: usize,
    /// Shared with the session, so a job sees the servers a turn would and can
    /// revive one that was down.
    mcp: Arc<McpServers>,
}

impl JobAgentRunner {
    /// Open (or reopen) the conversation this run appends to.
    async fn conversation(&self, job: &Job) -> Result<(String, Vec<Message>)> {
        if job.session_mode == SessionMode::Reuse
            && let Some(id) = &job.session_id
        {
            let exists = self.store.session(id).await?.is_some();
            if exists {
                let history = self.store.load_messages(id).await?;
                return Ok((id.clone(), history));
            }
            // The session was deleted under the job; fall through and start one.
        }
        let id = new_session_id();
        self.store
            .create_session(NewSession {
                id: id.clone(),
                cwd: job.cwd.clone(),
                model: Some(self.config.provider.model.clone()),
                provider: Some(self.spec.base_url.clone()),
            })
            .await?;
        Ok((id, Vec::new()))
    }

    async fn execute(&self, job: &Job) -> Result<RunReport> {
        // A job runs unattended, so a server that was down when the session
        // started gets another chance here rather than being lost for good.
        for notice in self.mcp.refresh(OnStart::All).await {
            tracing::info!(job = %job.label(), "{notice}");
        }

        let workspace = PathBuf::from(&job.cwd);
        let system = Message::system(system_prompt(&self.config, &workspace, &self.tools));
        let (session_id, mut history) = self.conversation(job).await?;
        let fresh = history.is_empty();

        // A reloaded transcript starts with its own system prompt; a fresh one
        // starts empty. Either way index 0 is the prompt, never history.
        if history
            .first()
            .map(|message| message.role != Role::System)
            .unwrap_or(true)
        {
            history.insert(0, system);
        }
        apply_history_window(&mut history, self.history_window);

        let tag = job.label();
        // A session that was just created is persisted whole, prompt included,
        // the way `setup::build` does it. A reused one only gains this turn.
        let start = if fresh { 0 } else { history.len() };
        history.push(Message::user(format!("[cron:{tag}] {}", job.prompt)));

        let options = AgentOptions {
            model: self.config.provider.model.clone(),
            max_iterations: self.config.agent.max_iterations,
            temperature: self.config.provider.temperature,
            max_tokens: self.config.agent.max_tokens,
            parallel_tool_calls: Some(self.config.provider.parallel_tool_calls),
            include_usage: self.config.provider.supports_usage_in_stream,
            max_tool_calls_per_turn: self.config.agent.max_tool_calls_per_turn,
            context_tokens: effective_budget(&self.config, None),
            tool_result_chars: self.config.agent.tool_result_chars,
            compact: self.config.agent.compact,
            compact_threshold: self.config.agent.compact_threshold,
            compact_keep_turns: self.config.agent.compact_keep_turns,
            summary_role: summary_role(&self.config),
            nudge_on_empty: self.config.agent.nudge_on_empty,
            repair_arguments: self.config.agent.repair_arguments,
            workspace_root: workspace,
        };
        let provider: Arc<dyn Provider> = Arc::new(self.spec.build(&session_id));
        let agent = Agent::new(provider.clone(), self.tools.clone(), options)
            .with_gate(self.gate.clone())
            .with_session(session_id.clone())
            .with_summarizer(Arc::new(ProviderSummarizer::new(
                provider,
                summary_model(&self.config),
            )));

        let (sender, mut events) = mpsc::unbounded_channel();
        // Drain to a buffer rather than the renderer: a job's output must not
        // fight the REPL for the terminal, and stdout belongs to the user's
        // conversation.
        let collected = tokio::spawn(async move {
            let mut text = String::new();
            while let Some(event) = events.recv().await {
                if let AgentEvent::TextDelta(delta) = event {
                    text.push_str(&delta);
                }
            }
            text
        });

        let outcome = agent
            .run(&mut history, &sender, CancellationToken::new())
            .await;
        drop(sender);
        let text = collected.await.unwrap_or_default();

        self.store
            .append_messages(
                &session_id,
                &history[start.saturating_sub(outcome.compacted)..],
            )
            .await?;

        let summary = format!(
            "{} · {}",
            outcome.stop.as_str(),
            one_line(&text, 160).unwrap_or_else(|| "(no output)".to_string())
        );
        let report = if outcome.stop == StopReason::Completed {
            RunReport::ok(summary)
        } else {
            RunReport::failed(summary)
        };
        Ok(report
            .with_session(Some(session_id.clone()))
            .with_output_ref(Some(session_id)))
    }
}

#[async_trait]
impl imp_cron::JobRunner for JobAgentRunner {
    async fn run(&self, job: &Job, _run_id: &str) -> RunReport {
        tracing::info!(job = %job.label(), session_mode = job.session_mode.as_str(), "cron run starting");
        match self.execute(job).await {
            Ok(report) => {
                tracing::info!(job = %job.label(), ok = report.ok, summary = %report.summary, "cron run finished");
                report
            }
            Err(err) => {
                tracing::warn!(job = %job.label(), error = %err, "cron run failed");
                RunReport::failed(format!("{err}"))
            }
        }
    }
}

/// The first line of `text`, clipped, or `None` when there is nothing to show.
fn one_line(text: &str, width: usize) -> Option<String> {
    let line = text.lines().find(|line| !line.trim().is_empty())?.trim();
    if line.chars().count() <= width {
        return Some(line.to_string());
    }
    Some(line.chars().take(width - 1).collect::<String>() + "…")
}

/// The system prompt: persona plus a digest of what the agent is allowed to do.
///
/// Stating the boundary in the prompt reduces wasted tool calls against denied
/// paths, but the enforcement lives in the tools and the approval engine — the
/// prompt is a hint, never a control.
pub fn system_prompt(config: &Config, root: &Path, tools: &ToolRegistry) -> String {
    let persona = match &config.agent.system_prompt_file {
        Some(path) => match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(err) => {
                tracing::warn!(path, error = %err, "cannot read system_prompt_file; using the built-in persona");
                builtin_persona()
            }
        },
        None => builtin_persona(),
    };

    let mode = if std::io::stdin().is_terminal() {
        "interactive (you may ask for approval)"
    } else {
        "non-interactive (approval is unavailable; actions that need consent will be refused)"
    };

    let mut prompt = persona;
    prompt.push_str(&format!(
        "\n\nWorkspace root: {} (read-write)",
        root.display()
    ));
    prompt.push_str(&format!("\nMode: {mode}"));

    prompt.push_str("\n\nAvailable tools:");
    for (name, risk) in tools.risks() {
        prompt.push_str(&format!("\n- {name} ({})", risk.as_str()));
    }

    if !config.policy.deny.is_empty() {
        prompt.push_str("\n\nAlways refused:");
        for rule in &config.policy.deny {
            prompt.push_str(&format!("\n- {} matching `{}`", rule.tool, rule.pattern));
        }
    }

    // M10.3: a small local model imitates "never/always" far better than a
    // paragraph of prose. Opt-in, so a large model is not handed a list it does
    // not need and a custom persona keeps its own voice.
    if config.agent.small_model {
        prompt.push_str(SMALL_MODEL_RULES);
    }

    prompt
}

/// Imperative rules appended for a small local model (`agent.small_model`).
///
/// Each is a "never"/"always" a 2–4B model can imitate, and each maps to real
/// loop behaviour: one tool per turn keeps the transcript short enough to parse,
/// not re-reading a file avoids the repeat the loop otherwise absorbs, and
/// stopping when it can answer is what ends the turn at all.
const SMALL_MODEL_RULES: &str = "\n\nRules (follow exactly):\
\n1. Call at most one tool per turn.\
\n2. Never read a file you have already read in this conversation.\
\n3. Never repeat a tool call with the same arguments.\
\n4. Stop as soon as you can answer, then reply in plain text with no tool call.\
\n5. Keep tool arguments short; never paste a whole file into an argument.\
\n6. If a tool fails, change your approach instead of retrying the same call.";

fn builtin_persona() -> String {
    "You are imp, a minimalist Unix-native agent. Work in small, verifiable steps, \
     prefer tools over guesses, and report what you actually observed. \
     When a task needs a capability you do not have, say so plainly."
        .to_string()
}

/// Drop old turns, keeping the system prompt and whole user→assistant groups.
///
/// Cutting on a message boundary alone can leave an assistant `tool_calls`
/// message whose results were dropped, or a stray `tool` result, and the provider
/// rejects that transcript outright. So the cut is moved forward to the next
/// `user` message.
pub fn apply_history_window(history: &mut Vec<Message>, window: usize) {
    if window == 0 || history.len() <= 1 {
        return;
    }
    // Only the conversation is windowed: index 0 is the system prompt, which is
    // configuration rather than history and is always sent.
    let body = history.len() - 1;
    if body <= window {
        return;
    }

    let target = 1 + (body - window);
    let mut cut = (1..history.len())
        .find(|index| *index >= target && history[*index].role == Role::User)
        .unwrap_or(target);

    // Never keep a tool result whose assistant call was cut away.
    while cut < history.len() && history[cut].role == Role::Tool {
        cut += 1;
    }
    if cut >= history.len() {
        return;
    }
    history.drain(1..cut);
}

/// Every `tool_calls` entry in `history` is answered by a matching result.
///
/// This is the property that actually matters: a transcript that keeps an
/// assistant tool call but drops its result is rejected outright by the
/// provider, and it is easy to produce by trimming on a message boundary.
#[cfg(test)]
fn tool_calls_are_paired(history: &[Message]) -> bool {
    for (index, message) in history.iter().enumerate() {
        let Some(calls) = &message.tool_calls else {
            continue;
        };
        for call in calls {
            let answered = history[index + 1..].iter().any(|later| {
                later.role == Role::Tool && later.tool_call_id.as_deref() == Some(call.id.as_str())
            });
            if !answered {
                return false;
            }
        }
    }
    true
}

/// Render a session for `/sessions`.
pub fn describe_session(row: &SessionRow, theme: Theme) -> String {
    let title = row
        .title
        .clone()
        .unwrap_or_else(|| "(untitled)".to_string());
    let model = row.model.clone().unwrap_or_else(|| "-".to_string());
    format!(
        "{}  {}  {}  {}",
        theme.info(&row.id[..8.min(row.id.len())]),
        theme.dim(&row.updated_at),
        theme.dim(&model),
        theme.bold(&title)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use imp_core::policy::ToolGate;
    use imp_core::tool::Risk;

    fn config() -> Config {
        Config::default()
    }

    /// The prompt only reads tool names and descriptions, so a registry over a
    /// throwaway in-memory store is enough. Async because opening the store is.
    async fn tools() -> ToolRegistry {
        let store = Arc::new(Store::open_in_memory().await.expect("in-memory store"));
        default_registry(
            &tool_config(&Config::default()),
            store,
            CronContext::default(),
        )
    }

    fn conversation() -> Vec<Message> {
        vec![
            Message::system("sys"),
            Message::user("one"),
            Message::assistant("a1"),
            Message::user("two"),
            Message::assistant("a2"),
            Message::user("three"),
            Message::assistant("a3"),
        ]
    }

    #[tokio::test]
    async fn prompt_states_the_workspace_and_every_tool() {
        let tools = tools().await;
        let prompt = system_prompt(&config(), Path::new("/tmp/ws"), &tools);

        assert!(prompt.contains("/tmp/ws"));
        assert!(prompt.contains("read_file"));
        assert!(
            prompt.contains("remember") && prompt.contains("recall"),
            "the memory tools are registered, so the prompt must advertise them"
        );
        assert!(
            prompt.contains("http_fetch"),
            "http_fetch is registered, so the prompt must advertise it"
        );
    }

    #[tokio::test]
    async fn prompt_surfaces_deny_rules() {
        let mut config = config();
        config.policy.deny.push(imp_core::config::DenyRule {
            tool: "run_command".to_string(),
            pattern: "*sudo*".to_string(),
        });

        let prompt = system_prompt(&config, Path::new("/tmp/ws"), &tools().await);

        assert!(prompt.contains("Always refused"));
        assert!(prompt.contains("*sudo*"));
    }

    #[tokio::test]
    async fn a_missing_prompt_file_falls_back_instead_of_failing() {
        let mut config = config();
        config.agent.system_prompt_file = Some("/nonexistent/persona.md".to_string());

        let prompt = system_prompt(&config, Path::new("/tmp/ws"), &tools().await);

        assert!(prompt.contains("imp"));
    }

    /// M10.3: a small-model prompt carries short numbered rules; a normal one
    /// is left exactly as it was.
    #[tokio::test]
    async fn the_small_model_rules_are_appended_only_when_asked() {
        let tools = tools().await;
        let plain = system_prompt(&config(), Path::new("/tmp/ws"), &tools);
        assert!(
            !plain.contains("Rules (follow exactly)"),
            "the rules must be opt-in: {plain}"
        );

        let mut config = config();
        config.agent.small_model = true;
        let prompted = system_prompt(&config, Path::new("/tmp/ws"), &tools);

        assert!(prompted.contains("Rules (follow exactly)"));
        assert!(prompted.contains("1. Call at most one tool per turn."));
        assert!(prompted.contains("Never read a file you have already read"));
        assert!(prompted.contains("Stop as soon as you can answer"));
        assert!(
            prompted.ends_with("retrying the same call."),
            "the rules go last so they are the freshest thing in context"
        );
    }

    /// The `[tools]` selection reaches the prompt: a hidden tool is not
    /// advertised, so a weak model never sees the name to choose it.
    #[tokio::test]
    async fn the_prompt_advertises_only_the_selected_tools() {
        let mut tools = tools().await;
        tools.select(imp_core::ToolSelection {
            only: vec!["read_file".to_string()],
            hide: Vec::new(),
        });

        let prompt = system_prompt(&config(), Path::new("/tmp/ws"), &tools);

        assert!(prompt.contains("- read_file"));
        assert!(!prompt.contains("- run_command"), "was: {prompt}");
        assert!(!prompt.contains("- http_fetch"), "was: {prompt}");
    }

    #[test]
    fn a_short_history_is_left_alone() {
        let mut history = conversation();
        let before = history.len();

        apply_history_window(&mut history, 100);

        assert_eq!(history.len(), before);
    }

    #[test]
    fn the_system_prompt_survives_trimming() {
        let mut history = conversation();

        apply_history_window(&mut history, 3);

        assert_eq!(history[0].content.as_deref(), Some("sys"));
        assert_eq!(history.len(), 3);
    }

    #[test]
    fn trimming_never_splits_a_turn() {
        let mut history = conversation();

        apply_history_window(&mut history, 4);

        assert_eq!(history[0].role, imp_core::message::Role::System);
        assert!(tool_calls_are_paired(&history));
        assert_eq!(history[1].role, imp_core::message::Role::User);
    }

    #[test]
    fn a_tool_result_is_never_orphaned() {
        use imp_core::message::{FunctionCall, ToolCall};
        let history = vec![
            Message::system("sys"),
            Message::user("one"),
            Message::assistant_with_tool_calls(
                None,
                vec![ToolCall {
                    id: "c1".to_string(),
                    kind: "function".to_string(),
                    function: FunctionCall {
                        name: "read_file".to_string(),
                        arguments: "{}".to_string(),
                    },
                }],
            ),
            Message::tool_result("c1", "contents"),
            Message::assistant("done"),
        ];

        for window in 1..8 {
            let mut candidate = history.clone();
            apply_history_window(&mut candidate, window);
            assert_eq!(
                candidate[0].content.as_deref(),
                Some("sys"),
                "window {window} dropped the system prompt"
            );
            assert!(
                tool_calls_are_paired(&candidate),
                "window {window} left a tool call unanswered: {:?}",
                candidate.iter().map(|m| m.role).collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn a_window_of_zero_keeps_everything() {
        let mut history = conversation();
        let before = history.len();

        apply_history_window(&mut history, 0);

        assert_eq!(history.len(), before, "0 means 'do not trim', not 'empty'");
    }

    // ------------------------------------------------------------- cron gate

    /// §5.7: a job prompt cannot prompt for approval, so a tool the policy would
    /// `ask` about is decided by `policy.noninteractive` instead. A job cannot
    /// promote such a tool to `auto`.
    #[tokio::test]
    async fn a_cron_gate_cannot_promote_an_ask_tool_to_auto() {
        let store = Arc::new(Store::open_in_memory().await.expect("in-memory store"));
        let config = Config::default();
        assert_eq!(config.policy.default, Decision::Ask);
        assert_eq!(config.policy.noninteractive, Decision::Deny);
        let gate = build_cron_gate(&config, &store, "/workspace", Vec::new());

        // `cron_add` is `Write`: with a terminal this would prompt.
        let err = gate
            .check(
                "cron_add",
                Risk::Write,
                &serde_json::json!({ "name": "weekly" }),
                Some("weekly"),
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("non-interactive"), "was: {err}");

        // The same for a command, which is what a job would actually want.
        let err = gate
            .check(
                "run_command",
                Risk::Execute,
                &serde_json::json!({ "command": "ls -la" }),
                None,
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("non-interactive"), "was: {err}");

        // Reading is not a change, so a job can still gather context (D15).
        gate.check("read_file", Risk::ReadOnly, &serde_json::json!({}), None)
            .await
            .expect("a read-only tool must survive the absence of a terminal");
    }

    /// The cron gate narrows nothing: a deny rule still refuses and an allow
    /// rule still permits, exactly as they do in an interactive session.
    #[tokio::test]
    async fn a_cron_gate_still_honours_allow_and_deny_rules() {
        let store = Arc::new(Store::open_in_memory().await.expect("in-memory store"));
        let mut config = Config::default();
        config.policy.allow.push(imp_core::config::AllowRule {
            tool: "run_command".to_string(),
            pattern: "git status".to_string(),
            scope: "session".to_string(),
        });
        config.policy.deny.push(imp_core::config::DenyRule {
            tool: "run_command".to_string(),
            pattern: "*sudo*".to_string(),
        });
        let gate = build_cron_gate(&config, &store, "/workspace", Vec::new());

        gate.check(
            "run_command",
            Risk::Execute,
            &serde_json::json!({ "command": "git status" }),
            None,
        )
        .await
        .expect("an allowlisted command runs without a terminal");

        let err = gate
            .check(
                "run_command",
                Risk::Execute,
                &serde_json::json!({ "command": "sudo ls" }),
                None,
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("deny rule"), "was: {err}");
    }

    /// The scheduler's own tools are registered, so a model can schedule a job.
    #[tokio::test]
    async fn the_cron_tools_are_registered_and_advertised() {
        let store = Arc::new(Store::open_in_memory().await.expect("in-memory store"));
        let tools = default_registry(
            &tool_config(&Config::default()),
            store.clone(),
            CronContext::default(),
        );
        assert!(tools.get("cron_add").is_some());
        assert!(tools.get("cron_list").is_some());
        assert!(tools.get("cron_remove").is_some());
        // The tools the scheduler itself registers must be advertised too.
        let prompt = system_prompt(&Config::default(), Path::new("/tmp/ws"), &tools);
        assert!(prompt.contains("cron_add"), "was: {prompt}");
    }
}
