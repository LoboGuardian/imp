//! `imp mcp serve`: publishing the agent to another model (SDD §5.9, FR-22,
//! FR-24, T5, D8, R5).
//!
//! The server half of MCP. An MCP host — another model, another harness — drives
//! imp through a small surface: one tool that runs an agent turn
//! (`agent_ask`), three that introspect what is there, the `cron_*` tools, and
//! two that are *listed but refused* unless the operator turned them on
//! (`agent_run_command`, `agent_write_file`).
//!
//! Three properties are the point, and all three are enforced by code:
//!
//! - **The default surface is read-only** (D8). `agent_ask` runs a turn against
//!   imp's own model backend with the read-only subset of the built-in tools,
//!   and shell execution and file writes are opt-in flags. The flags are printed
//!   at startup, so an operator cannot enable remote code execution without
//!   seeing that they did (T5).
//! - **A disabled capability is denied, not merely absent.** `agent_run_command`
//!   and `agent_write_file` are always in the tool list; without their flag,
//!   every call is refused by the same [`PolicyEngine`] that guards the rest of
//!   imp, and the refusal is written to the audit trail. Hiding a tool would
//!   leave the host guessing; a denial it can read is a decision it can work with.
//! - **The server never starts a REPL** (R5). Both own stdin/stdout, so
//!   `mcp serve` is a terminal mode: it runs the protocol and exits, and there is
//!   no path from here to [`crate::repl`].
//!
//! The gate is deliberately non-interactive. There is no terminal to prompt on —
//! stdin is the protocol pipe — so an enabled family substitutes the
//! non-interactive fallback (D21) and everything else fails closed.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::extract::State;
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, GetPromptRequestParams,
    GetPromptResponse, GetPromptResult, Implementation, JsonObject, ListPromptsResult,
    ListResourceTemplatesResult, ListResourcesResult, ListToolsResult, PaginatedRequestParams,
    Prompt, PromptArgument, PromptMessage, ReadResourceRequestParams, ReadResourceResponse,
    ReadResourceResult, Resource, ResourceContents, ResourceTemplate, Role as PromptRole,
    ServerCapabilities, ServerConfig, Tool as RmcpTool,
};
use rmcp::service::{RequestContext, serve_server};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{ErrorData, RoleServer, ServerHandler};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use imp_core::agent::{Agent, AgentEvent, AgentOptions};
use imp_core::clock::SystemClock;
use imp_core::config::{Config, Decision, McpServerSection};
use imp_core::error::{Error, Result};
use imp_core::message::{Message, Role as MessageRole};
use imp_core::new_session_id;
use imp_core::policy::{PolicyEngine, RecordingGate, ToolGate, ToolPolicy};
use imp_core::provider::Provider;
use imp_core::tool::{Risk, Tool as ImpTool, ToolCtx, ToolOutput, ToolRegistry};
use imp_provider::OpenAiProvider;
use imp_store::{NewSession, Store};
use imp_tools::{
    CronAdd, CronList, CronRemove, CronTools, RunCommand, WriteFile, default_registry,
};

use crate::cli::Cli;
use crate::setup::{apply_history_window, cron_context, system_prompt, tool_config};
use crate::summarize::ProviderSummarizer;

/// Builds the provider for one conversation.
///
/// It takes the session id because the provider expands `${session}` into its
/// configured headers, and the id has to be the one the conversation is stored
/// under — a second provider built for the same conversation must look identical
/// to the gateway, or prompt-cache routing breaks (§5.2.1). A test injects its
/// own factory to run the whole server without a network.
pub type ProviderFactory = Arc<dyn Fn(&str) -> Arc<dyn Provider> + Send + Sync>;

/// What the operator enabled (T5). Printed at startup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Exposure {
    /// `agent_run_command` is callable.
    pub exec: bool,
    /// `agent_write_file` is callable.
    pub write: bool,
    /// The `cron_*` tools are published at all.
    pub cron_write: bool,
}

impl Exposure {
    /// Read the flags from `[mcp.server]`.
    pub fn from(section: &McpServerSection) -> Self {
        Self {
            exec: section.expose_exec,
            write: section.expose_write,
            cron_write: section.expose_cron_write,
        }
    }

    /// The startup banner, one line per capability (T5).
    ///
    /// It is printed to **stderr**, because stdout is the protocol. An enabled
    /// capability is shouted rather than listed: the whole mitigation for T5 is
    /// that nobody turns on shell-over-MCP without noticing.
    pub fn banner(&self, transport: &str) -> Vec<String> {
        let mut lines = vec![
            format!("imp mcp serve · {transport}"),
            format!(
                "  expose_exec       = {}  {}",
                self.exec,
                if self.exec {
                    "⚠ ENABLED: a connected model can run shell commands"
                } else {
                    "(agent_run_command is listed, but refused by policy)"
                }
            ),
            format!(
                "  expose_write      = {}  {}",
                self.write,
                if self.write {
                    "⚠ ENABLED: a connected model can write files"
                } else {
                    "(agent_write_file is listed, but refused by policy)"
                }
            ),
            format!(
                "  expose_cron_write = {}  {}",
                self.cron_write,
                if self.cron_write {
                    "(cron_add, cron_list, cron_remove)"
                } else {
                    "(the cron tools are not published)"
                }
            ),
        ];
        if !self.exec && !self.write {
            lines.push("  surface: read-only".to_string());
        }
        lines
    }
}

/// The pieces one server shares with the rest of imp: the loop, the store and
/// the policy engine (§5.9).
///
/// It is the same assembly a session gets, minus the conversation: the store is
/// the session database, the tools are the built-in registry's read-only subset,
/// and the gate is a [`PolicyEngine`] the exposed tools and the inner agent both
/// pass through.
struct Runtime {
    config: Config,
    store: Arc<Store>,
    workspace_root: PathBuf,
    /// The read-only tools `agent_ask`'s inner agent may call.
    tools: Arc<ToolRegistry>,
    /// The one gate every decision goes through.
    gate: Arc<dyn ToolGate>,
    /// Builds the provider for a conversation.
    provider: ProviderFactory,
    exposure: Exposure,
    history_window: usize,
}

impl Runtime {
    /// Assemble the runtime. `provider` overrides the configured backend, which
    /// is how a test runs the whole server without a network.
    async fn build(
        config: &Config,
        cwd: &Path,
        db: Option<PathBuf>,
        provider: Option<ProviderFactory>,
    ) -> Result<Arc<Self>> {
        let workspace_root = config.workspace_root(cwd)?;
        let database = db.unwrap_or_else(|| config.database_path());
        let store = Arc::new(Store::open(&database).await?);
        let exposure = Exposure::from(&config.mcp.server);

        // The inner agent's tools are the *read-only* subset of the built-in
        // registry. Filtering by the risk class rather than listing names keeps
        // the boundary where it is enforced everywhere else: a tool that is not
        // an observation is not offered to the host's model.
        let full = default_registry(&tool_config(config), store.clone(), cron_context(config));
        let read_only: Vec<Arc<dyn ImpTool>> = full
            .risks()
            .into_iter()
            .filter(|(_, risk)| risk.is_observation())
            .filter_map(|(name, _)| full.get(name))
            .collect();
        let tools = Arc::new(ToolRegistry::from_tools(read_only));

        let gate: Arc<dyn ToolGate> = Arc::new(RecordingGate::new(
            Arc::new(server_gate(config, &store, &workspace_root, exposure)),
            store.approvals(),
        ));

        let provider = match provider {
            Some(factory) => factory,
            None => provider_factory(config)?,
        };

        Ok(Arc::new(Self {
            config: config.clone(),
            store,
            workspace_root,
            tools,
            gate,
            provider,
            exposure,
            history_window: config.agent.history_window,
        }))
    }

    /// Run one agent turn and return its answer.
    async fn ask(&self, args: Value) -> Result<ToolOutput> {
        let prompt = required(&args, "prompt", "agent_ask")?;
        let model = args
            .get("model")
            .and_then(Value::as_str)
            .map(str::to_string);
        let max_iterations = args
            .get("max_iterations")
            .and_then(Value::as_u64)
            .map(|value| value.max(1) as u32);
        let max_tokens = args
            .get("max_tokens")
            .and_then(Value::as_u64)
            .map(|value| value.clamp(1, u32::MAX as u64) as u32);
        let allow_tools: Option<Vec<String>> = match args.get("allow_tools") {
            Some(Value::Array(items)) => Some(
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect(),
            ),
            _ => None,
        };
        let requested = args
            .get("session_id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty());

        let (session_id, mut history, fresh) = self.conversation(requested).await?;

        // Index 0 is always the system prompt; a reloaded transcript keeps its
        // own, and a fresh one gets the one built from the read-only tool set.
        if history
            .first()
            .map(|message| message.role != MessageRole::System)
            .unwrap_or(true)
        {
            history.insert(
                0,
                Message::system(system_prompt(
                    &self.config,
                    &self.workspace_root,
                    &self.tools,
                )),
            );
        }
        apply_history_window(&mut history, self.history_window);
        // A conversation that was just created is persisted whole, prompt
        // included; a resumed one only gains this turn.
        let start = if fresh { 0 } else { history.len() };
        history.push(Message::user(prompt));

        let tools = match &allow_tools {
            Some(names) => Arc::new(self.subset(names)),
            None => self.tools.clone(),
        };
        let options = AgentOptions {
            model: model.unwrap_or_else(|| self.config.provider.model.clone()),
            max_iterations: max_iterations.unwrap_or(self.config.agent.max_iterations),
            temperature: self.config.provider.temperature,
            max_tokens,
            parallel_tool_calls: Some(self.config.provider.parallel_tool_calls),
            include_usage: self.config.provider.supports_usage_in_stream,
            max_tool_calls_per_turn: self.config.agent.max_tool_calls_per_turn,
            context_tokens: crate::setup::effective_budget(&self.config, None),
            tool_result_chars: self.config.agent.tool_result_chars,
            compact: self.config.agent.compact,
            compact_threshold: self.config.agent.compact_threshold,
            compact_keep_turns: self.config.agent.compact_keep_turns,
            summary_role: crate::setup::summary_role(&self.config),
            nudge_on_empty: self.config.agent.nudge_on_empty,
            repair_arguments: self.config.agent.repair_arguments,
            workspace_root: self.workspace_root.clone(),
        };
        let provider = (self.provider)(&session_id);
        let agent = Agent::new(provider.clone(), tools, options)
            .with_gate(self.gate.clone())
            .with_session(session_id.clone())
            .with_summarizer(Arc::new(ProviderSummarizer::new(
                provider,
                crate::setup::summary_model(&self.config),
            )));

        let (sender, mut events) = mpsc::unbounded_channel();
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
        if let Some(prompt) = history
            .iter()
            .find(|message| message.role == MessageRole::User)
            .and_then(|message| message.content.clone())
        {
            // A title is a convenience; failing to derive one must not fail a turn.
            let _ = self
                .store
                .title_from_first_prompt(&session_id, &prompt)
                .await;
        }

        Ok(ToolOutput::json(json!({
            "session_id": session_id,
            "stop": outcome.stop.as_str(),
            "text": text,
            "iterations": outcome.iterations,
            "usage": {
                "prompt_tokens": outcome.usage.prompt_tokens,
                "completion_tokens": outcome.usage.completion_tokens,
                "total_tokens": outcome.usage.total_tokens,
            },
        })))
    }

    /// Resolve (or create) the conversation `session_id` names.
    async fn conversation(&self, requested: Option<&str>) -> Result<(String, Vec<Message>, bool)> {
        let Some(id) = requested else {
            return Ok((self.new_conversation().await?, Vec::new(), true));
        };
        match self.store.session(id).await? {
            Some(row) => {
                let history = self.store.load_messages(&row.id).await?;
                let fresh = history.is_empty();
                Ok((row.id, history, fresh))
            }
            // A host may name its own conversation. Creating it under that id is
            // what lets a caller keep one across calls without a round trip.
            None => {
                let id = self.create_session(id).await?;
                Ok((id, Vec::new(), true))
            }
        }
    }

    async fn new_conversation(&self) -> Result<String> {
        let id = new_session_id();
        self.create_session(&id).await
    }

    async fn create_session(&self, id: &str) -> Result<String> {
        self.store
            .create_session(NewSession {
                id: id.to_string(),
                cwd: self.workspace_root.display().to_string(),
                model: Some(self.config.provider.model.clone()),
                provider: Some(self.config.provider.base_url.clone()),
            })
            .await?;
        Ok(id.to_string())
    }

    /// Restrict the inner agent to a named subset of the read-only tools.
    ///
    /// A name that is not in the read-only set is dropped, not an error: the
    /// surface is fail-closed, exactly as `tool_allow` is for an external server.
    fn subset(&self, names: &[String]) -> ToolRegistry {
        let tools = names
            .iter()
            .filter_map(|name| self.tools.get(name))
            .collect();
        ToolRegistry::from_tools(tools)
    }

    /// Every stored conversation, newest first.
    async fn list_sessions(&self) -> Result<ToolOutput> {
        let sessions = self.store.list_sessions(50).await?;
        let items: Vec<Value> = sessions.iter().map(session_json).collect();
        Ok(ToolOutput::json(json!({ "sessions": items })))
    }

    /// One conversation's transcript.
    async fn get_session(&self, args: Value) -> Result<ToolOutput> {
        let id = required(&args, "session_id", "agent_get_session")?;
        let row = self.store.session(id).await?.ok_or_else(|| Error::Tool {
            tool: "agent_get_session".to_string(),
            message: format!("no session `{id}` is stored"),
        })?;
        let messages = self.store.load_messages(id).await?;
        let items: Vec<Value> = messages.iter().map(message_json).collect();
        Ok(ToolOutput::json(json!({
            "session": session_json(&row),
            "messages": items,
        })))
    }

    /// The read-only tools the inner agent may call, with their risk classes.
    fn list_tools(&self) -> ToolOutput {
        let items: Vec<Value> = self
            .tools
            .risks()
            .into_iter()
            .map(|(name, risk)| json!({ "name": name, "risk": risk.as_str(), "approval": "auto" }))
            .collect();
        ToolOutput::json(json!({ "surface": "read-only", "tools": items }))
    }

    /// The `imp://…` resources, as JSON text.
    async fn read_uri(&self, uri: &str) -> Result<String> {
        match uri {
            "imp://sessions" => {
                let sessions = self.store.list_sessions(50).await?;
                let items: Vec<Value> = sessions.iter().map(session_json).collect();
                Ok(json!({ "sessions": items }).to_string())
            }
            "imp://jobs" => {
                let jobs = self.store.jobs().list_jobs().await?;
                let items: Vec<Value> = jobs.iter().map(job_json).collect();
                Ok(json!({ "jobs": items }).to_string())
            }
            "imp://config-redacted" => Ok(redacted_config(&self.config)),
            other => match other.strip_prefix("imp://sessions/") {
                Some(id) if !id.is_empty() => {
                    let row = self.store.session(id).await?.ok_or_else(|| Error::Tool {
                        tool: "imp://sessions".to_string(),
                        message: format!("no session `{id}` is stored"),
                    })?;
                    let messages = self.store.load_messages(id).await?;
                    let items: Vec<Value> = messages.iter().map(message_json).collect();
                    Ok(json!({ "session": session_json(&row), "messages": items }).to_string())
                }
                _ => Err(Error::Config(format!("unknown resource `{other}`"))),
            },
        }
    }
}

/// The gate every exposed tool and every inner-agent call goes through.
///
/// It is non-interactive on purpose: stdin is the protocol pipe, so there is no
/// terminal to answer a prompt. A capability the operator enabled gets a family
/// policy that substitutes the non-interactive fallback (D21); a capability they
/// did not gets a hard deny, so the refusal is a policy decision rather than a
/// missing tool (FR-24, T5).
fn server_gate(
    config: &Config,
    store: &Arc<Store>,
    scope: &Path,
    exposure: Exposure,
) -> PolicyEngine {
    let allow = config
        .policy
        .allow
        .iter()
        .map(|rule| (rule.tool.clone(), rule.pattern.clone()))
        .collect();
    let mut deny: Vec<(String, String)> = config
        .policy
        .deny
        .iter()
        .map(|rule| (rule.tool.clone(), rule.pattern.clone()))
        .collect();

    // `agent_run_command` is `run_command`, so it is denied under that name: an
    // operator's existing `run_command` rules keep protecting the MCP surface,
    // and the classifier still sees the command.
    for (enabled, tool) in [
        (exposure.exec, "run_command"),
        (exposure.write, "write_file"),
    ] {
        if !enabled {
            deny.push((tool.to_string(), "*".to_string()));
        }
    }
    if !exposure.cron_write {
        for tool in ["cron_add", "cron_remove"] {
            deny.push((tool.to_string(), "*".to_string()));
        }
    }

    let mut policies = Vec::new();
    if exposure.exec {
        policies.push(ToolPolicy::new("run_command", Decision::Auto));
    }
    if exposure.write {
        policies.push(ToolPolicy::new("write_file", Decision::Auto));
    }
    if exposure.cron_write {
        policies.push(ToolPolicy::new("cron_", Decision::Auto));
    }

    PolicyEngine::new(
        allow,
        deny,
        config.policy.default,
        config.policy.noninteractive,
        scope.display().to_string(),
        false,
    )
    .with_store(store.approvals())
    .with_tool_policies(policies)
}

/// The provider the server hands a conversation, built from `[provider]`.
fn provider_factory(config: &Config) -> Result<ProviderFactory> {
    let api_key = config.api_key()?;
    let base_url = config.provider.base_url.clone();
    let max_retries = config.provider.max_retries;
    let timeout = Duration::from_secs(config.provider.request_timeout_secs);
    let usage = config.provider.supports_usage_in_stream;
    let strict = config.provider.strict_tool_arguments;
    let headers = config.request_headers();

    Ok(Arc::new(move |session_id: &str| -> Arc<dyn Provider> {
        Arc::new(
            OpenAiProvider::new(&base_url, api_key.clone().unwrap_or_default())
                .with_max_retries(max_retries)
                .with_request_timeout(Some(timeout))
                .with_usage_in_stream(usage)
                .with_strict_tool_arguments(strict)
                .with_headers(headers.clone())
                .with_session_id(session_id.to_string()),
        )
    }))
}

/// The tools published over MCP, in surface order (§5.9).
fn build_surface(runtime: &Arc<Runtime>) -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    registry.register(AgentAsk::new(runtime.clone()));
    registry.register(AgentListSessions::new(runtime.clone()));
    registry.register(AgentGetSession::new(runtime.clone()));
    registry.register(AgentListTools::new(runtime.clone()));
    // The cron tools are the built-ins, over the same store: a job added over
    // MCP is in the database the scheduler ticks over.
    if runtime.exposure.cron_write {
        let cron = CronTools {
            store: runtime.store.jobs(),
            clock: Arc::new(SystemClock),
            timezone: runtime.config.cron.timezone.clone(),
        };
        registry.register(CronAdd::new(cron.clone()));
        registry.register(CronList::new(cron.clone()));
        registry.register(CronRemove::new(cron));
    }
    // Always listed, refused by policy unless the flag is set — see `Exposure`.
    let config = &runtime.config;
    registry.register(Exposed::new(
        Arc::new(RunCommand::new(
            config.exec.shell.clone(),
            Duration::from_secs(config.exec.default_timeout_secs),
            Duration::from_secs(config.exec.max_timeout_secs),
            config.exec.output_cap_bytes,
        )),
        "agent_run_command",
        "Run a shell command in imp's workspace. Refused by policy unless \
         [mcp.server].expose_exec is true.",
    ));
    registry.register(Exposed::new(
        Arc::new(WriteFile::new(config.workspace.max_file_bytes)),
        "agent_write_file",
        "Write a file in imp's workspace. Refused by policy unless \
         [mcp.server].expose_write is true.",
    ));
    registry
}

/// The name the policy engine decides on for an exposed tool.
///
/// The surface is named `agent_*` (§5.9), but `agent_run_command` *is*
/// `run_command`: deciding it under its real name is what makes an operator's
/// existing rules for `run_command` protect the MCP surface too, and what lets
/// the command classifier see the command it is judging.
fn policy_name(exposed: &str) -> &str {
    match exposed {
        "agent_run_command" => "run_command",
        "agent_write_file" => "write_file",
        other => other,
    }
}

/// A built-in tool published under its MCP name.
///
/// The delegation is the point: `agent_run_command` is `run_command` — the same
/// path guard, the same output caps, the same process-group kill — and the only
/// thing this adds is the name the surface calls it.
struct Exposed {
    inner: Arc<dyn ImpTool>,
    name: &'static str,
    description: &'static str,
}

impl Exposed {
    fn new(inner: Arc<dyn ImpTool>, name: &'static str, description: &'static str) -> Self {
        Self {
            inner,
            name,
            description,
        }
    }
}

#[async_trait]
impl ImpTool for Exposed {
    fn name(&self) -> &'static str {
        self.name
    }

    fn description(&self) -> &'static str {
        self.description
    }

    fn schema(&self) -> Value {
        self.inner.schema()
    }

    fn risk(&self) -> Risk {
        self.inner.risk()
    }

    fn approval_subject(&self, args: &Value) -> Option<String> {
        self.inner.approval_subject(args)
    }

    fn timeout(&self) -> Duration {
        self.inner.timeout()
    }

    async fn invoke(&self, ctx: ToolCtx, args: Value) -> Result<ToolOutput> {
        self.inner.invoke(ctx, args).await
    }
}

/// `agent_ask`: run one agent turn (§5.9).
struct AgentAsk {
    runtime: Arc<Runtime>,
}

impl AgentAsk {
    fn new(runtime: Arc<Runtime>) -> Self {
        Self { runtime }
    }
}

#[async_trait]
impl ImpTool for AgentAsk {
    fn name(&self) -> &'static str {
        "agent_ask"
    }
    fn description(&self) -> &'static str {
        "Run one turn of imp's agent with its own model backend and return the answer. \
         Params: prompt (required), session_id? (continue a conversation), model?, \
         max_tokens?, max_iterations?, allow_tools? (a subset of the read-only tools)."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "prompt": { "type": "string", "description": "What to ask the agent." },
                "session_id": {
                    "type": "string",
                    "description": "Continue this conversation. Omit to start a new one."
                },
                "model": { "type": "string", "description": "Override the model for this turn." },
                "max_tokens": {
                    "type": "integer",
                    "description": "Cap the tokens this turn may generate."
                },
                "max_iterations": {
                    "type": "integer",
                    "description": "Cap the provider round-trips for this turn."
                },
                "allow_tools": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Restrict the inner agent to these read-only tools."
                }
            },
            "required": ["prompt"]
        })
    }

    /// `agent_ask` only observes: it runs a turn whose tool set is the read-only
    /// subset, so the outer class matches the inner surface.
    fn risk(&self) -> Risk {
        Risk::ReadOnly
    }

    /// A turn is several provider round-trips, so the budget is the per-request
    /// timeout times the iteration cap rather than the tool default.
    fn timeout(&self) -> Duration {
        let per_request = self.runtime.config.provider.request_timeout_secs;
        let iterations = self.runtime.config.agent.max_iterations as u64;
        Duration::from_secs(per_request.saturating_mul(iterations).clamp(60, 3600))
    }

    async fn invoke(&self, _ctx: ToolCtx, args: Value) -> Result<ToolOutput> {
        self.runtime.ask(args).await
    }
}

/// `agent_list_sessions`: enumerate stored conversations (§5.9).
struct AgentListSessions {
    runtime: Arc<Runtime>,
}

impl AgentListSessions {
    fn new(runtime: Arc<Runtime>) -> Self {
        Self { runtime }
    }
}

#[async_trait]
impl ImpTool for AgentListSessions {
    fn name(&self) -> &'static str {
        "agent_list_sessions"
    }

    fn description(&self) -> &'static str {
        "List imp's stored conversations, newest first."
    }

    fn schema(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }

    fn risk(&self) -> Risk {
        Risk::ReadOnly
    }

    async fn invoke(&self, _ctx: ToolCtx, _args: Value) -> Result<ToolOutput> {
        self.runtime.list_sessions().await
    }
}

/// `agent_get_session`: one transcript (§5.9).
struct AgentGetSession {
    runtime: Arc<Runtime>,
}

impl AgentGetSession {
    fn new(runtime: Arc<Runtime>) -> Self {
        Self { runtime }
    }
}

#[async_trait]
impl ImpTool for AgentGetSession {
    fn name(&self) -> &'static str {
        "agent_get_session"
    }

    fn description(&self) -> &'static str {
        "Fetch one conversation's transcript. Params: session_id (required)."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "session_id": { "type": "string", "description": "The conversation to fetch." }
            },
            "required": ["session_id"]
        })
    }

    fn risk(&self) -> Risk {
        Risk::ReadOnly
    }

    async fn invoke(&self, _ctx: ToolCtx, args: Value) -> Result<ToolOutput> {
        self.runtime.get_session(args).await
    }
}

/// `agent_list_tools`: what the inner agent may call (§5.9).
struct AgentListTools {
    runtime: Arc<Runtime>,
}

impl AgentListTools {
    fn new(runtime: Arc<Runtime>) -> Self {
        Self { runtime }
    }
}

#[async_trait]
impl ImpTool for AgentListTools {
    fn name(&self) -> &'static str {
        "agent_list_tools"
    }

    fn description(&self) -> &'static str {
        "Introspect the read-only tools imp's agent may call, with their risk classes."
    }

    fn schema(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }

    fn risk(&self) -> Risk {
        Risk::ReadOnly
    }

    async fn invoke(&self, _ctx: ToolCtx, _args: Value) -> Result<ToolOutput> {
        Ok(self.runtime.list_tools())
    }
}

/// The MCP server: the protocol surface over the runtime.
struct ImpServer {
    runtime: Arc<Runtime>,
    surface: Arc<ToolRegistry>,
}

impl ImpServer {
    fn new(runtime: Arc<Runtime>) -> Self {
        let surface = Arc::new(build_surface(&runtime));
        Self { runtime, surface }
    }

    /// Gate one exposed call, then run it.
    ///
    /// This is `Agent::dispatch` without a model in front of it: the tool is
    /// resolved, the gate decides under the tool's *policy* name, and the call
    /// runs under the same timeout contract as any other tool.
    async fn dispatch(&self, name: &str, args: Value) -> Result<ToolOutput> {
        let Some(tool) = self.surface.get(name) else {
            let available = self
                .surface
                .risks()
                .iter()
                .map(|(name, _)| name.to_string())
                .collect();
            return Err(Error::UnknownTool {
                name: name.to_string(),
                available,
            });
        };
        let subject = tool.approval_subject(&args);
        self.runtime
            .gate
            .check(policy_name(name), tool.risk(), &args, subject.as_deref())
            .await?;

        let ctx = ToolCtx {
            workspace_root: self.runtime.workspace_root.clone(),
            cancel: CancellationToken::new(),
        };
        let budget = tool.timeout();
        match tokio::time::timeout(budget, tool.invoke(ctx, args)).await {
            Ok(result) => result,
            Err(_) => Err(Error::Tool {
                tool: name.to_string(),
                message: format!("timed out after {budget:?}"),
            }),
        }
    }

    /// The exposed tools as the wire describes them.
    fn tools(&self) -> Vec<RmcpTool> {
        self.surface
            .schemas()
            .into_iter()
            .map(|schema| {
                RmcpTool::new(
                    schema.name,
                    schema.description,
                    as_object(schema.parameters),
                )
            })
            .collect()
    }

    /// A successful tool result, as MCP content.
    fn result(output: ToolOutput) -> CallToolResponse {
        CallToolResponse::from(CallToolResult::success(vec![ContentBlock::text(
            output.content,
        )]))
    }

    /// A tool failure the caller can read. A protocol error would render
    /// opaquely, and the host needs the reason — especially for a denial.
    fn failure(error: &Error) -> CallToolResponse {
        CallToolResponse::from(CallToolResult::error(vec![ContentBlock::text(
            error.to_string(),
        )]))
    }
}

impl ServerHandler for ImpServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_resources()
                .enable_prompts()
                .build(),
        )
        .with_server_info(Implementation::new("imp", env!("CARGO_PKG_VERSION")))
        .with_instructions(
            "imp runs its own agent against its own model backend. Start with `agent_ask`; \
             use the `imp_agent` prompt if you only have prompts. The default surface is \
             read-only: `agent_run_command` and `agent_write_file` are listed but refused \
             unless the operator enabled them.",
        )
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult::with_all_items(self.tools()))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let args = match request.arguments {
            Some(object) => Value::Object(object),
            None => Value::Null,
        };
        Ok(match self.dispatch(&request.name, args).await {
            Ok(output) => Self::result(output),
            Err(error) => Self::failure(&error),
        })
    }

    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, ErrorData> {
        Ok(ListResourcesResult::with_all_items(vec![
            Resource::new("imp://sessions", "sessions")
                .with_description("Stored conversations, newest first.")
                .with_mime_type("application/json"),
            Resource::new("imp://jobs", "jobs")
                .with_description("Scheduled cron jobs.")
                .with_mime_type("application/json"),
            Resource::new("imp://config-redacted", "config-redacted")
                .with_description("The effective configuration, with secrets removed.")
                .with_mime_type("application/json"),
        ]))
    }

    async fn list_resource_templates(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourceTemplatesResult, ErrorData> {
        Ok(ListResourceTemplatesResult::with_all_items(vec![
            ResourceTemplate::new("imp://sessions/{id}", "session")
                .with_description("One conversation's transcript.")
                .with_mime_type("application/json"),
        ]))
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, ErrorData> {
        let uri = request.uri;
        match self.runtime.read_uri(&uri).await {
            Ok(body) => Ok(ReadResourceResponse::from(ReadResourceResult::new(vec![
                ResourceContents::text(body, uri).with_mime_type("application/json"),
            ]))),
            Err(Error::Config(message)) => Err(ErrorData::resource_not_found(
                message,
                Some(json!({ "uri": uri })),
            )),
            Err(error) => Err(ErrorData::internal_error(error.to_string(), None)),
        }
    }

    async fn list_prompts(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListPromptsResult, ErrorData> {
        Ok(ListPromptsResult::with_all_items(vec![Prompt::new(
            "imp_agent",
            Some("Frame a task for imp's agent, for hosts that only support prompts."),
            Some(vec![
                PromptArgument::new("task")
                    .with_description("What you want imp to do.")
                    .with_required(true),
            ]),
        )]))
    }

    async fn get_prompt(
        &self,
        request: GetPromptRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<GetPromptResponse, ErrorData> {
        if request.name != "imp_agent" {
            return Err(ErrorData::invalid_params(
                format!("no prompt named `{}`", request.name),
                None,
            ));
        }
        let task = request
            .arguments
            .as_ref()
            .and_then(|args| args.get("task"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| ErrorData::invalid_params("`task` is required", None))?;

        let text = format!(
            "Use the `agent_ask` tool to run the task below with imp, then report what it \
             returned.\n\nTask: {task}"
        );
        Ok(GetPromptResponse::from(GetPromptResult::new(vec![
            PromptMessage::new_text(PromptRole::User, text),
        ])))
    }
}

/// `imp mcp serve` (§5.12).
///
/// It never returns to the REPL: over stdio the server owns stdin/stdout, and
/// over HTTP it owns its socket until the process is stopped. Neither path can
/// reach `repl::interactive` (R5). It also never starts the scheduler — a job
/// added here is stored, and runs the next time a process with a scheduler opens
/// the database.
pub async fn run(
    cli: &Cli,
    config: &Config,
    cwd: &Path,
    stdio: bool,
) -> Result<std::process::ExitCode> {
    if !config.mcp.server.enabled {
        return Err(Error::Config(
            "mcp.server.enabled is false; refusing to serve".to_string(),
        ));
    }
    // `--stdio` forces the stdio transport whatever the config says; otherwise
    // the configured transport decides. Anything but the two known names is a
    // startup error rather than a server that quietly does something else.
    let transport = if stdio {
        "stdio".to_string()
    } else {
        config.mcp.server.transport.trim().to_string()
    };

    if transport == "stdio" {
        serve_stdio(config, cwd, cli.db.clone()).await
    } else if transport.eq_ignore_ascii_case("http") {
        serve_http(config, cwd, cli.db.clone()).await
    } else {
        Err(Error::Config(format!(
            "mcp.server.transport must be \"stdio\" or \"http\", was `{transport}`"
        )))
    }
}

/// The stdio server (M6): the protocol on stdin/stdout, until the peer closes.
async fn serve_stdio(
    config: &Config,
    cwd: &Path,
    db: Option<PathBuf>,
) -> Result<std::process::ExitCode> {
    let runtime = Runtime::build(config, cwd, db, None).await?;
    for line in runtime.exposure.banner("stdio") {
        eprintln!("{line}");
    }

    let server = ImpServer::new(runtime);
    let service = serve_server(server, (tokio::io::stdin(), tokio::io::stdout()))
        .await
        .map_err(|err| Error::Config(format!("mcp serve: the handshake failed: {err}")))?;
    service
        .waiting()
        .await
        .map_err(|err| Error::Config(format!("mcp serve: {err}")))?;
    Ok(std::process::ExitCode::SUCCESS)
}

/// The Streamable HTTP server (M10.1): the same surface on a socket.
///
/// The bind policy already ran at config load ([`McpServerSection::bind_socket`]),
/// so a wildcard, a public address, or a non-loopback bind without a token never
/// reaches this point. The token, when configured, is resolved here and required
/// on every request — on loopback too, because a token that is only checked on a
/// tailnet is a token that can be forgotten locally.
async fn serve_http(
    config: &Config,
    cwd: &Path,
    db: Option<PathBuf>,
) -> Result<std::process::ExitCode> {
    let runtime = Runtime::build(config, cwd, db, None).await?;
    for line in runtime.exposure.banner("http") {
        eprintln!("{line}");
    }

    let token = config.mcp.server.bearer_token()?;
    if token.is_some() {
        eprintln!("  auth              required (Authorization: Bearer …)");
    } else {
        eprintln!("  auth              none (loopback only)");
    }

    let addr = config.mcp.server.bind_socket()?;
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|err| Error::Config(format!("mcp serve: cannot bind {addr}: {err}")))?;
    let local = listener.local_addr().unwrap_or(addr);
    eprintln!("imp listening on http://{local}/mcp");

    let router = http_router(runtime, token, local.ip().is_loopback());
    axum::serve(listener, router)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
        .map_err(|err| Error::Config(format!("mcp serve: the HTTP server stopped: {err}")))?;
    Ok(std::process::ExitCode::SUCCESS)
}

/// The router `transport = "http"` serves: the MCP service at `/mcp`, optionally
/// behind the bearer-token gate.
///
/// Split out from [`serve_http`] so a test can bind it to an ephemeral port and
/// drive it with a real client, which is the only way to prove the surface and
/// the gate together.
fn http_router(runtime: Arc<Runtime>, token: Option<String>, loopback: bool) -> axum::Router {
    let factory = {
        let runtime = runtime.clone();
        move || Ok::<_, std::io::Error>(ImpServer::new(runtime.clone()))
    };

    let mut server_config = StreamableHttpServerConfig::default();
    if !loopback {
        // The `Host` allowlist defaults to loopback names, and a MagicDNS name
        // cannot be derived from an `IP:port` bind. Off a loopback bind the
        // bearer token — not the request's `Host` or its source address — is the
        // authentication boundary (D26), so host validation is switched off
        // rather than guessed at.
        server_config = server_config.disable_allowed_hosts();
    }

    let service = StreamableHttpService::new(
        factory,
        Arc::new(LocalSessionManager::default()),
        server_config,
    );

    let mut router = axum::Router::new().nest_service("/mcp", service);
    if let Some(expected) = token {
        router = router.layer(axum::middleware::from_fn_with_state(
            Arc::new(expected),
            require_bearer,
        ));
    }
    router
}

/// Reject a request that does not carry the configured bearer token.
///
/// It runs **before** the MCP service, so a wrong token never reaches a handler:
/// no tool is listed, no call is attempted, and an unauthenticated caller learns
/// nothing about the surface beyond "not authorized". The comparison is
/// constant-time so a token cannot be recovered a byte at a time.
async fn require_bearer(
    State(expected): State<Arc<String>>,
    request: axum::extract::Request,
    next: Next,
) -> Response {
    let ok = request
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .is_some_and(|presented| constant_time_eq(presented.trim(), expected.as_str()));

    if ok {
        next.run(request).await
    } else {
        (StatusCode::UNAUTHORIZED, "missing or invalid bearer token").into_response()
    }
}

/// Compare two secrets without letting the comparison's duration depend on where
/// they first differ.
fn constant_time_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
}

// ---------------------------------------------------------------- helpers

/// A required, non-empty string argument.
fn required<'a>(args: &'a Value, name: &str, tool: &str) -> Result<&'a str> {
    args.get(name)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| Error::ToolArgs {
            tool: tool.to_string(),
            message: format!("`{name}` is required"),
        })
}

/// A JSON schema as the object MCP expects.
fn as_object(value: Value) -> JsonObject {
    match value {
        Value::Object(object) => object,
        _ => JsonObject::new(),
    }
}

fn role_name(role: MessageRole) -> &'static str {
    match role {
        MessageRole::System => "system",
        MessageRole::User => "user",
        MessageRole::Assistant => "assistant",
        MessageRole::Tool => "tool",
    }
}

fn session_json(row: &imp_store::SessionRow) -> Value {
    json!({
        "id": row.id,
        "title": row.title,
        "cwd": row.cwd,
        "model": row.model,
        "provider": row.provider,
        "created_at": row.created_at,
        "updated_at": row.updated_at,
    })
}

fn message_json(message: &Message) -> Value {
    json!({
        "role": role_name(message.role),
        "content": message.content,
        "tool_calls": message.tool_calls.as_ref().map(|calls| {
            calls
                .iter()
                .map(|call| json!({
                    "id": call.id,
                    "name": call.function.name,
                    "arguments": call.function.arguments,
                }))
                .collect::<Vec<_>>()
        }),
        "tool_call_id": message.tool_call_id,
    })
}

fn job_json(job: &imp_core::job::Job) -> Value {
    json!({
        "id": job.id,
        "name": job.name,
        "schedule": job.schedule,
        "timezone": job.timezone,
        "session_mode": job.session_mode.as_str(),
        "enabled": job.enabled,
        "runs_count": job.runs_count,
        "max_runs": job.max_runs,
        "next_run_at": job.next_run_at,
        "last_run_at": job.last_run_at,
        "last_status": job.last_status,
        "prompt": job.prompt,
        "cwd": job.cwd,
    })
}

/// The effective config as JSON, with anything secret-shaped removed.
///
/// The repo's invariant is that a config never *contains* a secret — the key
/// lives in a `0600` credentials file and the config records its *name*. That
/// holds for everything except `[provider].headers`, which an operator can write
/// by hand and which may carry an `Authorization` value. Those are redacted
/// here, by name, so `imp://config-redacted` cannot become an exfiltration
/// path no matter what someone put in the file.
fn redacted_config(config: &Config) -> String {
    let mut value = serde_json::to_value(config).unwrap_or(Value::Null);
    if let Some(headers) = value
        .pointer_mut("/provider/headers")
        .and_then(Value::as_object_mut)
    {
        for (name, header) in headers.iter_mut() {
            if is_sensitive(name) {
                *header = Value::String("<redacted>".to_string());
            }
        }
    }
    serde_json::to_string_pretty(&value).unwrap_or_default()
}

/// Whether a header name looks like it carries a credential.
fn is_sensitive(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    [
        "authorization",
        "auth",
        "token",
        "key",
        "secret",
        "cookie",
        "password",
        "credential",
    ]
    .iter()
    .any(|needle| name.contains(needle))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use futures::stream::BoxStream;
    use imp_core::config::{AllowRule, DenyRule};
    use imp_core::provider::{ChatEvent, ChatRequest, FinishReason};
    use rmcp::ServiceExt;

    /// A provider that replays scripted answers, so a turn runs with no network.
    struct Scripted(Mutex<VecDeque<Vec<ChatEvent>>>);

    impl Scripted {
        fn new(scripts: Vec<Vec<ChatEvent>>) -> Arc<Self> {
            Arc::new(Self(Mutex::new(scripts.into())))
        }

        fn once(text: &str) -> Arc<Self> {
            Self::new(vec![vec![
                ChatEvent::TextDelta(text.to_string()),
                ChatEvent::Done {
                    finish_reason: FinishReason::Stop,
                },
            ]])
        }
    }

    impl Provider for Scripted {
        fn stream(
            &self,
            _request: ChatRequest,
            _cancel: CancellationToken,
        ) -> BoxStream<'static, Result<ChatEvent>> {
            let events = self.0.lock().unwrap().pop_front().unwrap_or_else(|| {
                vec![ChatEvent::Done {
                    finish_reason: FinishReason::Stop,
                }]
            });
            Box::pin(futures::stream::iter(events.into_iter().map(Ok)))
        }
    }

    /// A runtime over a throwaway store, a temp workspace and a scripted model.
    async fn runtime(dir: &Path, exposure: Exposure, provider: Arc<Scripted>) -> Arc<Runtime> {
        let mut config = Config::default();
        // No credentials: the fake provider never reads one, and this keeps the
        // test off the developer's real credentials file.
        config.provider.api_key_env = String::new();
        config.provider.api_key_file = String::new();
        config.provider.base_url = "http://127.0.0.1:1/v1".to_string();
        config.provider.model = "scripted".to_string();
        config.workspace.roots = vec![dir.display().to_string()];
        config.mcp.server.expose_exec = exposure.exec;
        config.mcp.server.expose_write = exposure.write;
        config.mcp.server.expose_cron_write = exposure.cron_write;

        let factory: ProviderFactory =
            Arc::new(move |_id: &str| provider.clone() as Arc<dyn Provider>);
        Runtime::build(&config, dir, Some(dir.join("imp.db")), Some(factory))
            .await
            .expect("the runtime builds")
    }

    fn surface_json(runtime: &Arc<Runtime>) -> Vec<Value> {
        ImpServer::new(runtime.clone())
            .tools()
            .into_iter()
            .map(|tool| {
                json!({
                    "name": tool.name,
                    "description": tool.description,
                    "parameters": Value::Object((*tool.input_schema).clone()),
                })
            })
            .collect()
    }

    fn args(value: Value) -> JsonObject {
        as_object(value)
    }

    // ------------------------------------------------------- the surface

    /// The golden surface: the default tool list, name for name. This is the
    /// contract §5.9 states, and it is the file a reviewer reads.
    #[tokio::test]
    async fn the_default_surface_is_a_golden() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = runtime(
            dir.path(),
            Exposure::from(&McpServerSection::default()),
            Scripted::once("x"),
        )
        .await;

        insta::assert_json_snapshot!(surface_json(&runtime));
    }

    /// `expose_cron_write = false` removes the cron tools; the exec and write
    /// tools stay listed, because a denial is what the host should read.
    #[tokio::test]
    async fn the_cron_tools_are_absent_without_their_flag() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = runtime(
            dir.path(),
            Exposure {
                exec: false,
                write: false,
                cron_write: false,
            },
            Scripted::once("x"),
        )
        .await;

        let names: Vec<String> = surface_json(&runtime)
            .into_iter()
            .filter_map(|tool| tool["name"].as_str().map(str::to_string))
            .collect();

        assert!(
            !names.iter().any(|name| name.starts_with("cron_")),
            "cron tools must be absent, was: {names:?}"
        );
        assert!(names.contains(&"agent_run_command".to_string()));
        assert!(names.contains(&"agent_write_file".to_string()));
    }

    /// T5/FR-24: without the flags, the write and exec capabilities are denied
    /// *by the policy engine* — not merely missing. The tool is listed, the call
    /// reaches the gate, and the gate refuses it.
    #[tokio::test]
    async fn write_and_exec_are_denied_by_policy_not_absent() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = runtime(
            dir.path(),
            Exposure {
                exec: false,
                write: false,
                cron_write: true,
            },
            Scripted::once("x"),
        )
        .await;
        let server = ImpServer::new(runtime.clone());

        // Both are on the surface...
        assert!(server.surface.get("agent_run_command").is_some());
        assert!(server.surface.get("agent_write_file").is_some());

        // ...and both are refused by the engine, under the name an operator's
        // own rules would use.
        let err = server
            .dispatch(
                "agent_run_command",
                Value::Object(args(json!({ "command": "echo hi" }))),
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("deny rule"), "was: {err}");

        let err = server
            .dispatch(
                "agent_write_file",
                Value::Object(args(json!({ "path": "a.txt", "content": "x" }))),
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("deny rule"), "was: {err}");

        // The refusal is real: nothing was written.
        assert!(!dir.path().join("a.txt").exists());
    }

    /// An operator's own `run_command` deny rules keep protecting the MCP
    /// surface, because `agent_run_command` is decided under that name.
    #[tokio::test]
    async fn an_operators_run_command_rules_apply_to_the_exposed_shell() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.provider.api_key_env = String::new();
        config.provider.api_key_file = String::new();
        config.workspace.roots = vec![dir.path().display().to_string()];
        config.mcp.server.expose_exec = true;
        config.policy.deny.push(DenyRule {
            tool: "run_command".to_string(),
            pattern: "*sudo*".to_string(),
        });
        config.policy.allow.push(AllowRule {
            tool: "run_command".to_string(),
            pattern: "*".to_string(),
            scope: "always".to_string(),
        });

        let provider = Scripted::once("x");
        let factory: ProviderFactory =
            Arc::new(move |_id: &str| provider.clone() as Arc<dyn Provider>);
        let runtime = Runtime::build(
            &config,
            dir.path(),
            Some(dir.path().join("imp.db")),
            Some(factory),
        )
        .await
        .unwrap();
        let server = ImpServer::new(runtime.clone());

        let err = server
            .dispatch(
                "agent_run_command",
                Value::Object(args(json!({ "command": "sudo ls" }))),
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("deny rule"), "was: {err}");
    }

    /// With `expose_exec`, the shell tool runs for real — the flag is what makes
    /// it a capability rather than a label.
    #[tokio::test]
    async fn expose_exec_makes_the_shell_tool_callable() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = runtime(
            dir.path(),
            Exposure {
                exec: true,
                write: false,
                cron_write: true,
            },
            Scripted::once("x"),
        )
        .await;
        let server = ImpServer::new(runtime.clone());

        let output = server
            .dispatch(
                "agent_run_command",
                Value::Object(args(json!({ "command": "echo exposed" }))),
            )
            .await
            .expect("the operator enabled it");
        assert!(
            output.content.contains("exposed"),
            "was: {}",
            output.content
        );
    }

    /// The startup banner states every capability, and shouts about the enabled
    /// ones (T5).
    #[test]
    fn the_banner_states_the_capabilities() {
        let quiet = Exposure {
            exec: false,
            write: false,
            cron_write: true,
        }
        .banner("stdio")
        .join("\n");
        assert!(quiet.contains("expose_exec       = false"));
        assert!(quiet.contains("read-only"));

        let loud = Exposure {
            exec: true,
            write: true,
            cron_write: false,
        }
        .banner("stdio")
        .join("\n");
        assert!(loud.contains("ENABLED"), "was: {loud}");
        assert!(loud.contains("shell commands"), "was: {loud}");
        assert!(loud.contains("write files"), "was: {loud}");
        assert!(!loud.contains("read-only"));
    }

    // ------------------------------------------------------ config redaction

    /// `imp://config-redacted` cannot carry a secret, whatever a hand-written
    /// config put in a header.
    #[test]
    fn config_redacted_removes_secrets() {
        let mut config = Config::default();
        config.provider.api_key_env = "MY_BACKEND_KEY".to_string();
        config.provider.headers.insert(
            "Authorization".to_string(),
            "Bearer super-secret-value".to_string(),
        );
        config
            .provider
            .headers
            .insert("x-opencode-session".to_string(), "${session}".to_string());

        let redacted = redacted_config(&config);

        assert!(
            !redacted.contains("super-secret-value"),
            "the header value leaked: {redacted}"
        );
        assert!(redacted.contains("<redacted>"));
        assert!(
            redacted.contains("MY_BACKEND_KEY"),
            "the env var *name* is not a secret and stays"
        );
        assert!(
            redacted.contains("${session}"),
            "a non-secret header must survive"
        );
    }

    // ------------------------------------------------------------ agent_ask

    /// The exit criterion: another model drives `agent_ask` end to end over a
    /// real MCP connection, and gets imp's answer back.
    #[tokio::test]
    async fn another_model_drives_agent_ask_end_to_end() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = runtime(
            dir.path(),
            Exposure::from(&McpServerSection::default()),
            Scripted::once("the fake model answered"),
        )
        .await;
        let server = ImpServer::new(runtime.clone());

        let (server_read, client_write) = tokio::io::duplex(64 * 1024);
        let (client_read, server_write) = tokio::io::duplex(64 * 1024);
        // The server is spawned, not awaited: `serve_server` completes the
        // handshake before returning, and it cannot handshake with a client that
        // has not been built yet. Awaiting it here would deadlock.
        let _server = tokio::spawn(serve_server(server, (server_read, server_write)));
        let client = ().serve((client_read, client_write)).await.expect("the client handshakes");

        let listed = client.peer().list_tools(None).await.expect("tools/list");
        let names: Vec<String> = listed
            .tools
            .iter()
            .map(|tool| tool.name.to_string())
            .collect();
        assert!(names.contains(&"agent_ask".to_string()), "was: {names:?}");

        let result = client
            .peer()
            .call_tool(
                CallToolRequestParams::new("agent_ask")
                    .with_arguments(args(json!({ "prompt": "say something" }))),
            )
            .await
            .expect("tools/call");

        assert_eq!(result.is_error, Some(false));
        let text = result
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text(block) => Some(block.text.clone()),
                _ => None,
            })
            .collect::<String>();
        let body: Value = serde_json::from_str(&text).expect("the tool result is JSON");
        assert_eq!(body["text"], "the fake model answered");
        assert_eq!(body["stop"], "completed");
        let session_id = body["session_id"].as_str().expect("a session id");

        // The turn is in the store, under the id the caller was handed.
        let stored = runtime.store.session(session_id).await.unwrap();
        assert!(stored.is_some(), "the conversation was persisted");

        client.cancel().await.ok();
    }

    /// The depth cap, pinned (M10.2, §5.13). The inner surface an `agent_ask`
    /// turn runs — the one a peer runs *when it answers a brief* — is the
    /// read-only subset of the built-ins: no `peer__*` tool is in it, so a peer
    /// cannot pass the brief on to a third model. If someone ever widens this
    /// surface, this test is the one that should fail.
    #[tokio::test]
    async fn the_inner_surface_a_peer_runs_has_no_delegation_tool() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = runtime(
            dir.path(),
            Exposure::from(&McpServerSection::default()),
            Scripted::once("x"),
        )
        .await;

        let risks = runtime.tools.risks();
        assert!(!risks.is_empty(), "the inner surface is not empty");
        for (name, risk) in risks {
            assert!(
                risk.is_observation(),
                "`{name}` is {risk:?}, so it must not be handed to a delegated turn"
            );
            assert!(
                !name.starts_with("peer__"),
                "`{name}` would let a peer delegate again, breaking the depth cap"
            );
        }
    }

    /// A denial crosses the protocol as a readable tool error, which is what the
    /// host needs to react to (FR-24).
    #[tokio::test]
    async fn a_denied_tool_reaches_the_caller_as_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = runtime(
            dir.path(),
            Exposure::from(&McpServerSection::default()),
            Scripted::once("x"),
        )
        .await;
        let server = ImpServer::new(runtime.clone());

        let (server_read, client_write) = tokio::io::duplex(64 * 1024);
        let (client_read, server_write) = tokio::io::duplex(64 * 1024);
        // The server is spawned, not awaited: `serve_server` completes the
        // handshake before returning, and it cannot handshake with a client that
        // has not been built yet. Awaiting it here would deadlock.
        let _server = tokio::spawn(serve_server(server, (server_read, server_write)));
        let client = ().serve((client_read, client_write)).await.expect("the client handshakes");

        let result = client
            .peer()
            .call_tool(
                CallToolRequestParams::new("agent_run_command")
                    .with_arguments(args(json!({ "command": "echo hi" }))),
            )
            .await
            .expect("tools/call");

        assert_eq!(result.is_error, Some(true));
        let text = result
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text(block) => Some(block.text.clone()),
                _ => None,
            })
            .collect::<String>();
        assert!(text.contains("deny rule"), "was: {text}");

        client.cancel().await.ok();
    }

    /// A turn that needs a tool runs it through the read-only subset and feeds
    /// the result back — the loop is imp's, not a stub.
    #[tokio::test]
    async fn agent_ask_runs_a_read_only_tool_through_the_loop() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("note.txt"), "the file said hello").unwrap();

        let provider = Scripted::new(vec![
            vec![
                ChatEvent::ToolCallDelta {
                    index: 0,
                    id: Some("call_1".to_string()),
                    name: Some("read_file".to_string()),
                    arguments: json!({ "path": "note.txt" }).to_string(),
                },
                ChatEvent::Done {
                    finish_reason: FinishReason::ToolCalls,
                },
            ],
            vec![
                ChatEvent::TextDelta("read it".to_string()),
                ChatEvent::Done {
                    finish_reason: FinishReason::Stop,
                },
            ],
        ]);
        let runtime = runtime(
            dir.path(),
            Exposure::from(&McpServerSection::default()),
            provider,
        )
        .await;
        let server = ImpServer::new(runtime.clone());

        let output = server
            .dispatch(
                "agent_ask",
                Value::Object(args(json!({ "prompt": "read note.txt" }))),
            )
            .await
            .expect("the turn runs");
        let body: Value = serde_json::from_str(&output.content).unwrap();
        assert_eq!(body["text"], "read it");
        assert_eq!(body["iterations"], 2);

        // The transcript holds the tool result, which means the loop ran the
        // read-only tool and fed it back.
        let session_id = body["session_id"].as_str().unwrap();
        let messages = runtime.store.load_messages(session_id).await.unwrap();
        assert!(
            messages.iter().any(|message| message
                .content
                .as_deref()
                .is_some_and(|text| text.contains("the file said hello"))),
            "the tool result is in the transcript"
        );
    }

    /// The resources and the prompt are part of the surface (FR-22), and they
    /// are reachable over the protocol, not just from the handler.
    #[tokio::test]
    async fn the_resources_and_the_prompt_are_published() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = runtime(
            dir.path(),
            Exposure::from(&McpServerSection::default()),
            Scripted::once("x"),
        )
        .await;
        let server = ImpServer::new(runtime.clone());

        let (server_read, client_write) = tokio::io::duplex(64 * 1024);
        let (client_read, server_write) = tokio::io::duplex(64 * 1024);
        // The server is spawned, not awaited: `serve_server` completes the
        // handshake before returning, and it cannot handshake with a client that
        // has not been built yet. Awaiting it here would deadlock.
        let _server = tokio::spawn(serve_server(server, (server_read, server_write)));
        let client = ().serve((client_read, client_write)).await.expect("the client handshakes");

        let resources = client
            .peer()
            .list_resources(None)
            .await
            .expect("resources/list");
        let uris: Vec<String> = resources.resources.iter().map(|r| r.uri.clone()).collect();
        assert!(
            uris.contains(&"imp://sessions".to_string()),
            "was: {uris:?}"
        );
        assert!(uris.contains(&"imp://jobs".to_string()));
        assert!(uris.contains(&"imp://config-redacted".to_string()));

        let templates = client
            .peer()
            .list_resource_templates(None)
            .await
            .expect("resources/templates/list");
        assert_eq!(
            templates.resource_templates[0].uri_template,
            "imp://sessions/{id}"
        );

        let prompts = client
            .peer()
            .list_prompts(None)
            .await
            .expect("prompts/list");
        assert_eq!(prompts.prompts[0].name, "imp_agent");

        let fetched = client
            .peer()
            .get_prompt(
                GetPromptRequestParams::new("imp_agent")
                    .with_arguments(args(json!({ "task": "summarise the repo" }))),
            )
            .await
            .expect("prompts/get");
        let framed = match &fetched.messages[0].content {
            ContentBlock::Text(block) => block.text.clone(),
            other => panic!("expected text, got {other:?}"),
        };
        assert!(framed.contains("agent_ask"), "was: {framed}");
        assert!(framed.contains("summarise the repo"), "was: {framed}");

        let body = client
            .peer()
            .read_resource(ReadResourceRequestParams::new("imp://config-redacted"))
            .await
            .expect("resources/read");
        let text = match &body.contents[0] {
            ResourceContents::TextResourceContents { text, .. } => text.clone(),
            other => panic!("expected text, got {other:?}"),
        };
        assert!(text.contains("expose_exec"), "was: {text}");

        client.cancel().await.ok();
    }

    // ------------------------------------------------- streamable http (M10.1)

    /// Bind the HTTP router to an ephemeral loopback port and serve it in the
    /// background. Returns the address, and the task the test aborts.
    async fn spawn_http(
        runtime: Arc<Runtime>,
        token: Option<String>,
    ) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("an ephemeral loopback port");
        let addr = listener.local_addr().expect("a bound address");
        let router = http_router(runtime, token, true);
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        (addr, server)
    }

    /// The status line of a raw HTTP request, so a test can read what the
    /// transport actually answered — the client library hides a `401` behind a
    /// handshake error.
    async fn raw_status(addr: std::net::SocketAddr, token: Option<&str>) -> u16 {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let request = async {
            let mut stream = tokio::net::TcpStream::connect(addr).await?;
            let mut head = format!(
                "POST /mcp HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\n\
                 Accept: application/json, text/event-stream\r\nContent-Length: 2\r\n\
                 Connection: close\r\n"
            );
            if let Some(token) = token {
                head.push_str(&format!("Authorization: Bearer {token}\r\n"));
            }
            head.push_str("\r\n{}");
            stream.write_all(head.as_bytes()).await?;
            let mut response = String::new();
            stream.read_to_string(&mut response).await?;
            Ok::<_, std::io::Error>(response)
        };

        let response = tokio::time::timeout(std::time::Duration::from_secs(5), request)
            .await
            .expect("the request answered instead of hanging")
            .expect("the request completed");
        response
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|code| code.parse().ok())
            .expect("a status code")
    }

    /// (a) A real client reaches the HTTP surface on an ephemeral loopback
    /// port, discovers the tools, and runs one — and the approval gate is still
    /// what decides. "It listens" is not the assertion; a call that crossed the
    /// gate is.
    #[tokio::test]
    async fn a_client_reaches_the_http_surface_and_the_gate_still_decides() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = runtime(
            dir.path(),
            Exposure::from(&McpServerSection::default()),
            Scripted::once("the http model answered"),
        )
        .await;

        let (addr, server) = spawn_http(runtime, None).await;
        let url = format!("http://{addr}/mcp");

        let client = imp_mcp::McpClient::connect_http("peer", &url, None)
            .await
            .expect("the client handshakes over HTTP");

        let tools = client.list_tools().await.expect("tools/list");
        let names: Vec<String> = tools.iter().map(|tool| tool.name.clone()).collect();
        assert!(names.contains(&"agent_ask".to_string()), "was: {names:?}");
        // The disabled capability is listed, because a hidden tool would leave
        // the host guessing (D22).
        assert!(
            names.contains(&"agent_run_command".to_string()),
            "was: {names:?}"
        );

        let answer = client
            .call("agent_ask", json!({ "prompt": "say something" }), 64 * 1024)
            .await
            .expect("tools/call");
        assert!(!answer.is_error, "{}", answer.content);
        assert!(
            answer.content.contains("the http model answered"),
            "was: {}",
            answer.content
        );

        // The gate did not move: `agent_run_command` is refused by policy, and
        // the refusal crosses HTTP as a readable tool error.
        let denied = client
            .call(
                "agent_run_command",
                json!({ "command": "echo hi" }),
                64 * 1024,
            )
            .await
            .expect("tools/call");
        assert!(denied.is_error, "was: {}", denied.content);
        assert!(
            denied.content.contains("deny rule"),
            "was: {}",
            denied.content
        );

        client.close().await;
        server.abort();
    }

    /// (c) A wrong token is refused before the MCP service sees the request, so
    /// no tool is ever published to that caller. Both halves are checked: the
    /// raw status the layer answers, and the client-side failure.
    #[tokio::test]
    async fn a_wrong_http_token_is_rejected_before_any_tool_is_published() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = runtime(
            dir.path(),
            Exposure::from(&McpServerSection::default()),
            Scripted::once("x"),
        )
        .await;

        let (addr, server) = spawn_http(runtime, Some("right-token".to_string())).await;
        let url = format!("http://{addr}/mcp");

        // The right token works, so the 401s below are the token, not the URL.
        let client = imp_mcp::McpClient::connect_http("peer", &url, Some("right-token"))
            .await
            .expect("the right token handshakes");
        assert!(
            client
                .list_tools()
                .await
                .unwrap()
                .iter()
                .any(|tool| tool.name == "agent_ask"),
            "the authorized client sees the surface"
        );
        client.close().await;

        // A wrong token cannot even handshake, so it never lists a tool.
        let wrong = imp_mcp::McpClient::connect_http("peer", &url, Some("wrong-token")).await;
        assert!(wrong.is_err(), "a wrong token must not connect");
        assert!(
            imp_mcp::McpClient::connect_http("peer", &url, None)
                .await
                .is_err(),
            "a missing token must not connect"
        );

        // The layer itself answers 401, before the service: the rejection is
        // structural, not a handler's choice.
        assert_eq!(raw_status(addr, Some("wrong-token")).await, 401);
        assert_eq!(raw_status(addr, None).await, 401);

        server.abort();
    }

    /// The other half of `serve_http`'s token handling: an absent token source
    /// means no `Authorization` header is checked, which is what a loopback
    /// server with no token configured relies on.
    #[tokio::test]
    async fn a_tokenless_loopback_server_answers_without_a_header() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = runtime(
            dir.path(),
            Exposure::from(&McpServerSection::default()),
            Scripted::once("x"),
        )
        .await;

        let (addr, server) = spawn_http(runtime, None).await;

        // An anonymous request reaches the service, which rejects the empty body
        // as a protocol error rather than as an auth failure: not a 401.
        assert_ne!(raw_status(addr, None).await, 401);

        server.abort();
    }

    #[test]
    fn a_token_comparison_does_not_short_circuit() {
        assert!(constant_time_eq("s3cret", "s3cret"));
        assert!(!constant_time_eq("s3cret", "s3cre7"));
        assert!(!constant_time_eq("short", "longer"));
        assert!(constant_time_eq("", ""));
    }
}
