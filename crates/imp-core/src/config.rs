//! Layered configuration.
//!
//! Precedence, highest first: CLI flags → `IMP_*` environment variables →
//! project `imp.toml` → user config file → built-in defaults.
//!
//! The CLI layer applies flag overrides on top of [`Config::load`]; this module
//! owns everything below flags. Overlaying is done on `toml::Value` trees, so a
//! project file only needs to restate the keys it actually changes.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// Application directory name used for config, state, and logs.
const APP: &str = "imp";

/// The application directory this project used before it was renamed.
///
/// This is the only place in the tree where the old name survives, and it is
/// deliberate: [`migrate_legacy_state`] renames `<config>/minion` and
/// `<state>/minion` onto their `imp` counterparts on the first run, so an
/// existing config, its `0600` credentials file and the session database are
/// moved rather than silently abandoned. See SDD D46.
const LEGACY_APP: &str = "minion";

/// The session database file under the old application directory.
///
/// Renamed in place by [`migrate_db_files`], sidecars included.
const LEGACY_DB_FILE: &str = "minion.db";

/// The project configuration file read from a workspace.
const PROJECT_CONFIG: &str = "imp.toml";

/// The project configuration file under its pre-rename name.
///
/// Still read when `imp.toml` is absent: a rename that quietly drops a
/// project's settings is the failure the fallback exists to avoid (SDD D46).
const LEGACY_PROJECT_CONFIG: &str = "minion.toml";

/// Config fields that are considered dangerous when loaded from an untrusted
/// project directory, because they enable code execution or secret exfiltration.
const DANGEROUS_CONFIG_FIELDS: &[&str] = &[
    "mcp.client.servers",
    "peers",
    "provider.api_key_file",
    "provider.api_key_env",
    "provider.base_url",
    "provider.token_file",
    "mcp.server.token_file",
    "mcp.server.token_env",
    "agent.system_prompt_file",
];

/// Check whether the config was loaded from a project directory and warn about
/// dangerous fields that could enable code execution or secret exfiltration.
fn warn_about_project_config(path: &Path) {
    // Only warn if loading from an actual project file (not the default path)
    if !path.exists() {
        return;
    }
    eprintln!(
        "imp: WARNING: loading configuration from {}",
        path.display()
    );
    eprintln!("imp: This file may contain fields that execute code or access secrets.");
    eprintln!("imp: Review the file before trusting it, especially:");
    for field in DANGEROUS_CONFIG_FIELDS {
        eprintln!("imp:   - [{}]", field);
    }
}

/// Request-body keys `[provider.extra_body]` may not override.
///
/// These are built by the agent loop or the provider client; letting a config
/// shadow one would change the request's meaning rather than its sampling.
const PROTECTED_BODY_KEYS: &[&str] = &[
    "model",
    "messages",
    "stream",
    "tools",
    "tool_choice",
    "stream_options",
];

/// The values `--lean` fills in when the user has not chosen their own.
///
/// A CLI convenience, not a config layer: the flag overrides the file, exactly
/// as every other global flag does. It is the small-model profile from
/// SDD D45 in one switch, and each value stays independently settable.
const LEAN_CONTEXT_TOKENS: usize = 8192;
const LEAN_TOOL_RESULT_CHARS: usize = 6000;
const LEAN_MAX_TOKENS: u32 = 1024;
const LEAN_MAX_TOOL_CALLS: u32 = 12;
const LEAN_STREAM_IDLE_SECS: u64 = 300;

/// Fully resolved configuration for one run.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Model endpoint settings.
    pub provider: ProviderConfig,
    /// Agent loop settings.
    pub agent: AgentSettings,
    /// Filesystem boundary settings.
    pub workspace: WorkspaceConfig,
    /// Process execution settings.
    pub exec: ExecConfig,
    /// Which tools are offered to the model.
    pub tools: ToolsConfig,
    /// Approval policy.
    pub policy: PolicyConfig,
    /// Outbound HTTP limits and the SSRF guard for `http_fetch`.
    pub http_fetch: HttpFetchConfig,
    /// The optional System One guard for the approval gate.
    pub guard: GuardConfig,
    /// In-process cron scheduler settings.
    pub cron: CronConfig,
    /// Consuming external MCP servers (SDD §5.10).
    pub mcp: McpConfig,
    /// Remote peers a small model delegates a brief to (SDD §5.13, M10.2).
    ///
    /// Each entry is *one* tool — `peer__<name>_ask` — a flattened, gate-visible
    /// delegation to another imp's `agent_ask` (D29). Keyed by the name that
    /// tool carries.
    pub peers: BTreeMap<String, Peer>,
    /// Self-update channel (SDD §9).
    pub update: UpdateConfig,
    /// Logging settings.
    pub logging: LoggingConfig,
    /// Terminal presentation (`[ui]`). Cosmetic only.
    pub ui: UiConfig,
}

/// Settings for the OpenAI-compatible endpoint.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProviderConfig {
    /// Base URL *without* `/chat/completions`.
    pub base_url: String,
    /// Name of the environment variable holding the API key. Never the key itself.
    ///
    /// Empty means "this backend needs no credentials".
    pub api_key_env: String,
    /// Path to the credentials file holding the API key.
    ///
    /// Defaults to the standard location next to the user config, so secrets
    /// never live in a config file. Empty disables the lookup.
    pub api_key_file: String,
    /// Model identifier.
    pub model: String,
    /// Sampling temperature.
    pub temperature: Option<f32>,
    /// Stream responses.
    pub stream: bool,
    /// Total budget for one HTTP request.
    pub request_timeout_secs: u64,
    /// Retry attempts for pre-stream failures.
    pub max_retries: u32,
    /// Whether the provider reports usage on the final stream chunk.
    pub supports_usage_in_stream: bool,
    /// Whether to request multiple tool calls per assistant turn.
    pub parallel_tool_calls: bool,
    /// Idle budget between stream chunks. Slow local prefill can exceed the old
    /// hardcoded 60s before the first token, so it is configurable.
    pub stream_idle_timeout_secs: u64,
    /// Extra keys merged into the request body, for backend-specific sampling
    /// (`top_p`, `repeat_penalty`, …). Core keys cannot be overridden.
    pub extra_body: serde_json::Map<String, serde_json::Value>,
    /// Backend compatibility switches.
    pub quirks: ProviderQuirks,
    /// Ask the backend to constrain tool arguments to their JSON schema.
    ///
    /// A provider quirk, off by default. When the backend understands it
    /// (OpenAI-style `strict` function calling, Ollama's structured outputs),
    /// each advertised function carries `"strict": true`, which makes the model
    /// emit arguments that validate rather than prose the loop then has to
    /// reject. A backend that does not understand the field sees the current
    /// request, because the flag is simply absent. It is not a validation of
    /// our own: a backend that ignores it changes nothing.
    pub strict_tool_arguments: bool,
    /// Extra headers sent on every request.
    ///
    /// Values may contain `${session}`, which expands to the stable identifier
    /// of the current conversation. Gateways that pin a conversation to one
    /// upstream need this: OpenCode Go, for example, requires
    /// `x-opencode-session` so it can route and cache prompts consistently.
    pub headers: BTreeMap<String, String>,
}

impl Default for ProviderConfig {
    fn default() -> Self {
        Self {
            base_url: "https://api.openai.com/v1".to_string(),
            api_key_env: "OPENAI_API_KEY".to_string(),
            api_key_file: default_credentials_path()
                .map(|path| path.display().to_string())
                .unwrap_or_default(),
            model: "gpt-4.1-mini".to_string(),
            temperature: Some(0.2),
            stream: true,
            request_timeout_secs: 120,
            max_retries: 3,
            supports_usage_in_stream: true,
            parallel_tool_calls: true,
            stream_idle_timeout_secs: 120,
            extra_body: serde_json::Map::new(),
            quirks: ProviderQuirks::default(),
            strict_tool_arguments: false,
            headers: BTreeMap::new(),
        }
    }
}

/// Compatibility switches for strict or non-conforming backends.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProviderQuirks {
    /// Strip non-essential schema keywords and collapse nullable unions.
    pub sanitize_schemas: bool,
    /// Omit `parallel_tool_calls` even when it is set.
    pub omit_parallel_tool_calls: bool,
    /// Omit `tool_choice` entirely.
    pub omit_tool_choice: bool,
    /// Serialize `"content": null` on assistant tool-call messages instead of
    /// omitting the field, for templates that require it.
    pub empty_assistant_content: bool,
}

impl Default for ProviderQuirks {
    fn default() -> Self {
        Self {
            sanitize_schemas: true,
            omit_parallel_tool_calls: false,
            omit_tool_choice: false,
            empty_assistant_content: false,
        }
    }
}

/// Settings for the agent loop.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentSettings {
    /// Optional file whose contents replace the built-in system prompt.
    pub system_prompt_file: Option<String>,
    /// Maximum provider round-trips per turn.
    pub max_iterations: u32,
    /// Maximum tool calls the model may make within one turn. `0` is unlimited.
    ///
    /// A weak local model loops; this is the wall that stops one turn from
    /// burning the session. When it is reached the turn ends with
    /// [`StopReason::ToolBudget`](crate::agent::StopReason::ToolBudget) and every
    /// tool call already taken from the model is answered, so the stored
    /// transcript stays valid.
    pub max_tool_calls_per_turn: u32,
    /// Treat the model as a small local one: append short numbered rules to the
    /// system prompt. A 2–4B model imitates "never/always" far better than a
    /// vague instruction, so the rules are imperative and finite.
    pub small_model: bool,
    /// Advisory token ceiling for a turn.
    pub max_tokens_per_turn: u64,
    /// How many trailing messages to send.
    pub history_window: usize,
    /// Summarize dropped history instead of discarding it silently.
    pub summarize_on_truncate: bool,
    /// Token budget for the whole request. `None` disables token trimming.
    ///
    /// Covers the system prompt, history and reserved room for the tool
    /// schemas. This is what keeps a small-context model from overflowing.
    pub context_tokens: Option<usize>,
    /// Largest number of characters of any one tool result sent to the model.
    ///
    /// `None` leaves results uncapped.
    pub tool_result_chars: Option<usize>,
    /// Upper bound on tokens generated per provider response. `None` sends none.
    pub max_tokens: Option<u32>,
    /// Give an empty model reply one bounded second chance.
    pub nudge_on_empty: bool,
    /// Repair fenced or trailing-comma tool arguments before giving up.
    pub repair_arguments: bool,
}

impl Default for AgentSettings {
    fn default() -> Self {
        Self {
            system_prompt_file: None,
            max_iterations: 25,
            max_tool_calls_per_turn: 0,
            small_model: false,
            max_tokens_per_turn: 200_000,
            history_window: 40,
            summarize_on_truncate: true,
            context_tokens: None,
            tool_result_chars: None,
            max_tokens: None,
            nudge_on_empty: false,
            repair_arguments: true,
        }
    }
}

/// Which tools the local agent is offered (M10.3).
///
/// Both lists name tools exactly, and both are **narrowing only**: they change
/// what the model is shown, never what the approval gate decides. A hidden tool
/// is unresolvable as well as unadvertised, so a model that calls one is refused
/// as if the name did not exist. There is no way for `hide` to turn a denied
/// call into an allowed one. See `tool::ToolSelection`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ToolsConfig {
    /// When non-empty, only these tools are offered. Empty offers everything.
    pub only: Vec<String>,
    /// Tools removed from the surface, even if they also appear in `only`.
    pub hide: Vec<String>,
}

impl ToolsConfig {
    /// The registry filter these settings describe.
    pub fn selection(&self) -> crate::tool::ToolSelection {
        crate::tool::ToolSelection {
            only: self.only.clone(),
            hide: self.hide.clone(),
        }
    }
}

/// Filesystem boundary the tools may not cross.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WorkspaceConfig {
    /// Read-write roots. Relative entries resolve against the process cwd.
    pub roots: Vec<String>,
    /// Roots that may be read but never written.
    pub read_only_roots: Vec<String>,
    /// Largest file a single read or write may touch.
    pub max_file_bytes: u64,
    /// Whether path resolution may follow symlinks out of the root.
    pub follow_symlinks: bool,
}

impl Default for WorkspaceConfig {
    fn default() -> Self {
        Self {
            roots: vec![".".to_string()],
            read_only_roots: Vec::new(),
            max_file_bytes: 2_097_152,
            follow_symlinks: false,
        }
    }
}

/// Process execution limits.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ExecConfig {
    /// Default command timeout.
    pub default_timeout_secs: u64,
    /// Hard ceiling the model may not raise.
    pub max_timeout_secs: u64,
    /// Bytes of stdout/stderr retained per stream.
    pub output_cap_bytes: u64,
    /// Shell used for `run_command`.
    pub shell: String,
    /// Use `-lc` instead of `-c` so login profiles are sourced.
    pub login_shell: bool,
}

impl Default for ExecConfig {
    fn default() -> Self {
        Self {
            default_timeout_secs: 120,
            max_timeout_secs: 3600,
            output_cap_bytes: 262_144,
            shell: "/bin/sh".to_string(),
            login_shell: false,
        }
    }
}

/// Approval policy.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PolicyConfig {
    /// Outcome when no other rule matches.
    pub default: Decision,
    /// Outcome when stdin is not a TTY. Defaults to deny: fail closed.
    ///
    /// Applies to tools that change something. `ReadOnly` tools are allowed
    /// regardless, per SDD §5.6 and D14; `Network` counts as a change.
    pub noninteractive: Decision,
    /// Patterns that pre-approve a tool call.
    pub allow: Vec<AllowRule>,
    /// Patterns that always refuse a tool call. Always beats `allow`.
    pub deny: Vec<DenyRule>,
    /// Automatic approval for recognised read-only shell commands.
    pub read_only: ReadOnlyConfig,
}

impl Default for PolicyConfig {
    fn default() -> Self {
        Self {
            default: Decision::Ask,
            noninteractive: Decision::Deny,
            allow: Vec::new(),
            deny: Vec::new(),
            read_only: ReadOnlyConfig::default(),
        }
    }
}

/// Automatic approval for read-only shell commands (SDD §5.6, D48).
///
/// `run_command` is `Risk::Execute`, so without this every `ls` or `git status`
/// prompts. When enabled, a command the classifier in [`crate::read_only`]
/// recognises as read-only runs without a prompt. Off by default; the `/auto`
/// REPL command flips [`enabled`](Self::enabled) for the process.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ReadOnlyConfig {
    /// Start a session with the read-only-command shortcut on.
    pub enabled: bool,
    /// Extra bare verbs to treat as read-only, beyond the built-in set.
    ///
    /// Each entry is a command name (`fd`, `delta`), never a path or a pattern;
    /// a command with extra verbs is still refused if it uses shell syntax.
    pub extra: Vec<String>,
}

/// What policy decides about an invocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Decision {
    /// Run without asking.
    Auto,
    /// Prompt the user.
    #[default]
    Ask,
    /// Refuse.
    Deny,
}

/// A pattern that pre-approves matching invocations.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AllowRule {
    /// Tool this rule applies to.
    pub tool: String,
    /// Command or argument prefix to match. Glob syntax, anchored at the start.
    pub pattern: String,
    /// `once`-like scopes are session-only; `always` persists.
    #[serde(default = "default_scope")]
    pub scope: String,
}

fn default_scope() -> String {
    "session".to_string()
}

/// A pattern that always refuses matching invocations.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DenyRule {
    /// Tool this rule applies to.
    pub tool: String,
    /// Glob pattern to match.
    pub pattern: String,
}

/// Limits and the SSRF guard for the `http_fetch` tool (SDD §5.5, threat T3).
///
/// Two separate lists gate an outbound request, and they mean different things:
/// this `allowed_domains` is the *guard* — a host that does not match it cannot
/// be fetched at all, whatever the approval policy says. Approval is the
/// ordinary `[policy.allow]` mechanism, matched against the URL's host because
/// `http_fetch` names the host as its approval subject. Keeping the two apart is
/// what lets `allowed_domains` be a security boundary rather than a convenience.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct HttpFetchConfig {
    /// Hosts the tool may reach, as exact names or globs (`*.example.com`).
    ///
    /// A plain `http://` URL is granted only by an exact, non-glob entry — a
    /// wildcard raises reachability, not the scheme (SDD §5.5, §11.1). Empty
    /// means nothing is fetchable: the guard fails closed.
    pub allowed_domains: Vec<String>,
    /// Refuse a host that resolves to a private, loopback, link-local,
    /// unique-local or otherwise non-public address.
    pub block_private_ips: bool,
    /// Largest response body kept, in bytes.
    pub max_bytes: u64,
    /// Wall-clock budget for one request, in seconds.
    pub timeout_secs: u64,
}

impl Default for HttpFetchConfig {
    fn default() -> Self {
        Self {
            allowed_domains: Vec::new(),
            block_private_ips: true,
            max_bytes: 1_048_576,
            timeout_secs: 20,
        }
    }
}

/// The optional System One guard for the approval gate (SDD §5.6, D16).
///
/// When enabled, a flagged `run_command` in an eligible category may be
/// resolved by a `/v1/systemone` model instead of prompting. The guard is
/// **disabled by default**: it is a network call that carries command text to a
/// third party, so it stays opt-in until it has been seen behaving. It can only
/// narrow prompts, never widen permissions (FR-43), and every failure falls
/// back to the existing prompt (FR-45).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GuardConfig {
    /// Whether the guard is consulted at all. Off by default.
    pub enabled: bool,
    /// Server root of the System One model, *without* the `/v1` suffix.
    ///
    /// The request is sent to `{base_url}/v1/systemone`.
    pub base_url: String,
    /// Highest risk that resolves silently. A verdict at or below this allows.
    pub allow_threshold: f32,
    /// Risk above which the verdict is a confident refusal.
    ///
    /// The band between the two thresholds also prompts; the distinction is
    /// what the audit trail records (FR-47).
    pub deny_threshold: f32,
    /// Wall-clock budget for one verdict, in seconds.
    pub timeout_secs: u64,
}

impl Default for GuardConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            base_url: String::new(),
            allow_threshold: 0.25,
            deny_threshold: 0.75,
            timeout_secs: 10,
        }
    }
}

/// In-process cron scheduler settings (SDD §5.3, §5.7).
///
/// The scheduler is a task inside the process, not an OS crontab entry (D5), so
/// these values describe one running `imp`. That also means jobs do not run
/// while imp is closed — a documented limitation, R6.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CronConfig {
    /// Whether the tick loop starts at all.
    pub enabled: bool,
    /// IANA timezone a job gets when it does not name one.
    pub timezone: String,
    /// What to do about occurrences that came due while imp was not running.
    pub missed_run_policy: MissedRunPolicy,
    /// How many job runs may be in flight at once. The rest are queued.
    pub max_concurrent_jobs: usize,
    /// Hard ceiling on the runs a single `run_all` catch-up may start, so a job
    /// missed for a year cannot turn into a thousand runs at startup.
    pub missed_run_cap: usize,
}

impl Default for CronConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            timezone: "UTC".to_string(),
            missed_run_policy: MissedRunPolicy::RunOnce,
            max_concurrent_jobs: 2,
            missed_run_cap: 20,
        }
    }
}

/// What to do with occurrences that were missed while the process was not running.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MissedRunPolicy {
    /// Drop them: recompute `next_run_at` and record one `skipped` run.
    Skip,
    /// Run the job once now, then resume the schedule.
    #[default]
    RunOnce,
    /// Run every missed occurrence, bounded by `missed_run_cap`.
    RunAll,
}

/// Outbound MCP client settings (SDD §5.10).
///
/// Each entry under `[mcp.client.servers.<name>]` is an external MCP server that
/// imp spawns over stdio and consumes the tools of. The table key namespaces
/// everything about it: its tools reach the model as `mcp__<name>__<tool>`, and
/// the approval policy for those tools is derived from that same name.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct McpConfig {
    /// The client half: external servers imp calls.
    pub client: McpClientConfig,
    /// The server half: exposing imp over MCP (§5.9).
    pub server: McpServerSection,
}

/// `[mcp.server]`: the surface `imp mcp serve` publishes (§5.9).
///
/// The default is the read-only surface (D8, T5): `agent_ask` and the
/// introspection tools, plus the cron tools. Shell execution and file writes are
/// opt-in, because both are a remote-code-execution surface once an MCP host is
/// driving: `expose_exec`/`expose_write` stay `false` until an operator says
/// otherwise, and the flags are printed at startup so nobody enables them by
/// accident.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct McpServerSection {
    /// Whether `imp mcp serve` is allowed to run at all.
    pub enabled: bool,
    /// Transport. `stdio` (the default) owns stdin/stdout; `http` serves the
    /// same surface over Streamable HTTP on `bind`. R5 makes the server and the
    /// REPL mutually exclusive because both own stdin/stdout — with `http` the
    /// server instead owns a socket, which is why the transport is a deliberate
    /// config value and not a flag.
    pub transport: String,
    /// Address `transport = "http"` listens on.
    ///
    /// Defaults to loopback. A non-loopback bind is fail-closed: it is refused
    /// at load time unless a token is configured, because on a tailnet the token
    /// — not the source address — is what authenticates a caller (see
    /// [`Self::bind_socket`]).
    pub bind: String,
    /// Name of the environment variable holding the bearer token a client must
    /// present. Only the *name* lives here, never the value.
    pub token_env: String,
    /// Path to the credentials file (`api_key = "…"`, mode 0600) holding the
    /// bearer token. Only the *path* lives here.
    pub token_file: String,
    /// Publish `agent_run_command` as a callable tool. `false` still lists it,
    /// but every call is refused by the policy engine.
    pub expose_exec: bool,
    /// Publish `agent_write_file` as a callable tool. Same "listed but denied"
    /// contract as [`expose_exec`](Self::expose_exec).
    pub expose_write: bool,
    /// Publish the `cron_*` tools. They are absent from the surface when this is
    /// `false`, not merely denied.
    pub expose_cron_write: bool,
}

impl Default for McpServerSection {
    fn default() -> Self {
        Self {
            enabled: true,
            transport: "stdio".to_string(),
            bind: "127.0.0.1:8788".to_string(),
            token_env: String::new(),
            token_file: String::new(),
            expose_exec: false,
            expose_write: false,
            // §5.1's example has this on: scheduling is a `Write`, but the
            // scheduler runs under the non-interactive cron gate, so a job it
            // creates can still do very little on its own.
            expose_cron_write: true,
        }
    }
}

impl McpServerSection {
    /// Whether `transport = "http"`.
    pub fn is_http(&self) -> bool {
        self.transport.trim().eq_ignore_ascii_case("http")
    }

    /// Whether a bearer token source is configured (name or path).
    ///
    /// This is what the bind policy keys on: it is the *configuration*, not the
    /// resolved value, that has to be present for a non-loopback bind to be
    /// allowed at load time. A named source that turns out to be empty is a
    /// separate, later error ([`Self::bearer_token`]).
    pub fn has_token_source(&self) -> bool {
        !self.token_env.trim().is_empty() || !self.token_file.trim().is_empty()
    }

    /// The token clients must present, resolved from `token_env` then
    /// `token_file`, using the same resolution the provider uses.
    pub fn bearer_token(&self) -> Result<Option<String>> {
        resolve_secret(
            &self.token_env,
            &self.token_file,
            "MCP bearer token",
            " (set `mcp.server.token_env` or `mcp.server.token_file`)",
        )
    }

    /// The address `transport = "http"` listens on, after the bind policy.
    ///
    /// The rules, and why they exist:
    ///
    /// - **`0.0.0.0` and `::` are always refused.** They are not an address; they
    ///   are every interface, the public one included. There is no tailnet bind
    ///   that wants them.
    /// - **A globally routable address is always refused.** imp's wide-area
    ///   transport is the tailnet; a bind that reaches the public internet is a
    ///   different decision, and not one this milestone makes.
    /// - **Anything else non-loopback requires a token.** Loopback is a local
    ///   process, so it needs nothing. A tailnet, LAN or link-local address can
    ///   still be reached by another device, and there the only thing imp has
    ///   is the bearer token — the source IP proves nothing, because any process
    ///   on a tailnet node can open that socket. Without a configured token the
    ///   server refuses to start rather than serving unauthenticated.
    pub fn bind_socket(&self) -> Result<std::net::SocketAddr> {
        let text = self.bind.trim();
        let addr: std::net::SocketAddr = text.parse().map_err(|err| {
            Error::Config(format!(
                "mcp.server.bind must be an `IP:port` address, was `{text}`: {err}"
            ))
        })?;
        let ip = addr.ip();

        if ip.is_unspecified() {
            return Err(Error::Config(format!(
                "mcp.server.bind = `{text}` is a wildcard and would listen on the public \
                 interface; bind the tailnet address (or 127.0.0.1 for local use) instead"
            )));
        }
        if is_public_address(ip) {
            return Err(Error::Config(format!(
                "mcp.server.bind = `{text}` is a globally routable address; imp serves over \
                 the tailnet, not the public interface"
            )));
        }
        if !ip.is_loopback() && !self.has_token_source() {
            return Err(Error::Config(format!(
                "mcp.server.bind = `{text}` is not loopback, and no token is configured; \
                 set `mcp.server.token_env` or `mcp.server.token_file` (fail-closed)"
            )));
        }
        Ok(addr)
    }
}

/// Whether `ip` reaches the public internet, as opposed to a loopback, private,
/// link-local, CGNAT/tailnet or unique-local address.
///
/// The list is the one `block_private_ips` in `http_fetch` uses; it answers the
/// same question ("is this address a place the wider network lives?"), so the
/// two agree on where the boundary is.
fn is_public_address(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => {
            let octets = v4.octets();
            !(v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || is_cgnat_v4(v4)
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_documentation()
                || v4.is_multicast()
                // 198.18.0.0/15, the benchmarking range.
                || (octets[0] == 198 && (octets[1] == 18 || octets[1] == 19)))
        }
        std::net::IpAddr::V6(v6) => {
            !(v6.is_loopback()
                || v6.is_unique_local()
                || v6.is_unicast_link_local()
                || v6.is_unspecified()
                || v6.is_multicast()
                || is_tailnet_v6(v6))
        }
    }
}

/// `100.64.0.0/10`, the range Tailscale draws from (and RFC 6598 CGNAT).
fn is_cgnat_v4(ip: std::net::Ipv4Addr) -> bool {
    let octets = ip.octets();
    octets[0] == 100 && (64..128).contains(&octets[1])
}

/// Tailscale's IPv6 prefix, `fd7a:115c:a1e0::/48`.
fn is_tailnet_v6(ip: std::net::Ipv6Addr) -> bool {
    let segments = ip.segments();
    segments[0] == 0xfd7a && segments[1] == 0x115c && segments[2] == 0xa1e0
}

/// `[mcp.client]`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct McpClientConfig {
    /// External servers, keyed by the namespace their tools are published under.
    ///
    /// The key must be non-empty and must not contain `__`: the flattened name
    /// `mcp__<server>__<tool>` has to identify one tool on one server, and a key
    /// carrying the separator would make two servers collide on a single name.
    /// `Config::validate` rejects such a key rather than letting the ambiguity
    /// reach the catalogue.
    pub servers: BTreeMap<String, McpServerConfig>,
}

/// One external server, `[mcp.client.servers.<name>]`.
///
/// A server is reached one of two ways, and they are mutually exclusive: a
/// `command` imp spawns over stdio, or a `url` it speaks Streamable HTTP to
/// (the transport that lets two imp instances on the same tailnet talk).
/// `Config::validate` accepts exactly one of the two.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct McpServerConfig {
    /// Program to spawn. Executed directly, never through a shell, so an
    /// argument cannot become a command.
    pub command: String,
    /// Arguments passed to `command`, verbatim.
    pub args: Vec<String>,
    /// The server's Streamable-HTTP endpoint, e.g.
    /// `http://peer.tailnet.ts.net:8788/mcp`. Mutually exclusive with
    /// `command`/`args`.
    pub url: String,
    /// Name of the environment variable holding the bearer token sent as
    /// `Authorization: Bearer`. Only the *name* lives here.
    pub token_env: String,
    /// Path to the credentials file (`api_key = "…"`, mode 0600) holding the
    /// bearer token, resolved the same way `provider.api_key_file` is. Only the
    /// *path* lives here.
    pub token_file: String,
    /// Contact this server at the start of the first turn instead of while the
    /// session is assembled. A session that is opened and closed without a turn
    /// then never spawns it.
    pub lazy: bool,
    /// Which of the server's tools the model may see and call.
    ///
    /// Glob syntax ([`crate::glob::glob_match`]), matched against the tool's own
    /// name, anchored at both ends. **An empty list allows nothing**: a server is
    /// fail-closed until a pattern names something, and `["*"]` is how an
    /// operator says "every tool this server has". A tool that is not listed is
    /// absent from the catalogue and cannot be invoked — an unknown tool is an
    /// unknown tool whoever asks.
    pub tool_allow: Vec<String>,
    /// Approval decision for this server's tools, substituting the global
    /// `policy.default` (and `policy.noninteractive`) for them.
    ///
    /// `None` leaves the server under the global policy. Deny rules, allow rules
    /// and the command classifier still run first, so this moves only the
    /// default a call would otherwise inherit: a trusted local server can be
    /// `auto` while a server reached over the network stays `ask`. See D21.
    pub approval: Option<Decision>,
}

impl McpServerConfig {
    /// Whether this server is reached over HTTP rather than spawned over stdio.
    pub fn is_http(&self) -> bool {
        !self.url.trim().is_empty()
    }

    /// The bearer token to send, resolved from `token_env` then `token_file`.
    ///
    /// `Ok(None)` means "no auth configured" — the client simply sends no
    /// `Authorization` header. A source that is named but yields nothing is an
    /// error rather than a silent anonymous request, which is the fail-closed
    /// half: a misconfigured token must not look like a server that rejects you.
    pub fn bearer_token(&self) -> Result<Option<String>> {
        resolve_secret(
            &self.token_env,
            &self.token_file,
            "MCP bearer token",
            " (set `token_env` or `token_file` on the server, or unset both to send no token)",
        )
    }

    /// Whether `tool` — the server's own name for it, not the flattened one —
    /// may reach the model.
    pub fn allows(&self, tool: &str) -> bool {
        self.tool_allow
            .iter()
            .any(|pattern| crate::glob::glob_match(pattern, tool))
    }

    /// The prefix every flattened tool of this server carries.
    ///
    /// It is also the family key the approval policy is keyed on, so the one
    /// function is what keeps `mcp__a__b` from being read as server `a` tool
    /// `b` in one place and something else in another.
    pub fn tool_prefix(server: &str) -> String {
        format!("mcp__{server}__")
    }

    /// The name `tool` is published under.
    pub fn flattened(server: &str, tool: &str) -> String {
        format!("{}{tool}", Self::tool_prefix(server))
    }
}

/// One remote peer, `[peers.<name>]` (SDD §5.13, M10.2).
///
/// A peer is *another imp's* `mcp serve` endpoint, reached over MCP like any
/// other server (D25) — no new protocol. What differs is how it reaches the
/// model: instead of every tool the peer publishes, a peer contributes exactly
/// one, `peer__<name>_ask`, which carries a *brief* to the peer's `agent_ask`
/// and brings back its answer. The name is per peer so the approval engine keys
/// on it directly; a generic `delegate(target = "…")` would move the decision
/// off the tool name and into an argument, which is the trap `mcp_call` is
/// deferred for (D20, D29).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Peer {
    /// Program to spawn for a peer on the same machine. Executed directly,
    /// never through a shell. Mutually exclusive with [`Self::url`].
    pub command: String,
    /// Arguments passed to `command`, verbatim.
    pub args: Vec<String>,
    /// The peer's Streamable-HTTP endpoint, e.g. `http://big.tailnet.ts.net:8788/mcp`.
    /// This is the shape the milestone is about: the brief leaves the machine.
    pub url: String,
    /// Name of the environment variable holding the bearer token. Only the name.
    pub token_env: String,
    /// Path to the credentials file (`api_key = "…"`, mode 0600) holding the
    /// token. Only the path.
    pub token_file: String,
    /// Completion budget forwarded to the peer's `agent_ask` when a call does
    /// not name one. `None` leaves the peer's own default.
    pub max_tokens: Option<u32>,
    /// Largest answer kept from the peer, in bytes. A longer answer is cut and
    /// marked. Around 8 KB by default: the local model has a small context, so
    /// a long answer to a short brief is the failure mode to avoid (§5.13).
    pub result_cap_bytes: u64,
    /// Approval decision for `peer__<name>_ask`, substituting the global
    /// `policy.default` (and `policy.noninteractive`) for that one tool.
    ///
    /// `None` leaves it under the global policy — which is `deny` in a
    /// non-interactive run, so a cron job cannot escalate a brief off the
    /// machine unless the operator says so. Deny and allow rules still run
    /// first. See D21.
    pub approval: Option<Decision>,
}

impl Default for Peer {
    fn default() -> Self {
        Self {
            command: String::new(),
            args: Vec::new(),
            url: String::new(),
            token_env: String::new(),
            token_file: String::new(),
            max_tokens: None,
            result_cap_bytes: 8192,
            approval: None,
        }
    }
}

impl Peer {
    /// Whether this peer is reached over HTTP rather than spawned over stdio.
    pub fn is_http(&self) -> bool {
        !self.url.trim().is_empty()
    }

    /// The bearer token to send, resolved from `token_env` then `token_file`.
    ///
    /// `Ok(None)` means "no auth configured". A source that is named but yields
    /// nothing is an error rather than a silent anonymous call, the same
    /// fail-closed rule the MCP client applies.
    pub fn bearer_token(&self) -> Result<Option<String>> {
        resolve_secret(
            &self.token_env,
            &self.token_file,
            "peer bearer token",
            " (set `token_env` or `token_file` on the peer, or unset both to send no token)",
        )
    }

    /// The one tool this peer is exposed as: `peer__<name>_ask`.
    ///
    /// It is also the family key the approval policy is keyed on, so the name a
    /// call resolves to and the name a policy matches are the same string.
    pub fn tool_name(name: &str) -> String {
        format!("peer__{name}_ask")
    }
}

/// `[update]`: where `imp update` looks for a newer release (SDD §9).
///
/// The channel is a GitHub-compatible releases API serving prebuilt binaries.
/// The defaults point at the public repository, which needs no token; `api_url`
/// exists so a mirror (or a test fixture on `127.0.0.1`) can be used instead.
/// The scheme is enforced at use: `https`, or plain `http` only to a literal
/// loopback host.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct UpdateConfig {
    /// API root, *without* `/repos/...`. GitHub is `https://api.github.com`.
    pub api_url: String,
    /// Repository in `owner/name` form.
    pub repo: String,
    /// Prefix of the published asset names. The platform suffix is appended as
    /// `-<os>-<arch>`, e.g. `imp-linux-arm64`.
    pub asset_prefix: String,
}

impl Default for UpdateConfig {
    fn default() -> Self {
        Self {
            api_url: "https://api.github.com".to_string(),
            repo: "Az107/imp".to_string(),
            asset_prefix: "imp".to_string(),
        }
    }
}

/// Log destination and verbosity.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LoggingConfig {
    /// Tracing filter directive.
    pub level: String,
    /// Human-readable or JSON lines.
    pub format: LogFormat,
    /// Optional log file. `None` logs to stderr only.
    pub file: Option<String>,
    /// Scrub values of API-key environment variables from logs.
    pub redact_env: bool,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            level: "info".to_string(),
            format: LogFormat::Text,
            file: None,
            redact_env: true,
        }
    }
}

/// Terminal presentation settings (`[ui]`).
///
/// Cosmetic only: nothing here changes what the agent is allowed to do, and
/// `--json` ignores all of it. The defaults preserve the plain behaviour a pipe
/// or a screen reader expects — colour follows the terminal, glyphs are the
/// Unicode set, and motion is off.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct UiConfig {
    /// When to emit ANSI colour: `auto` (a terminal), `always`, or `never`.
    pub color: ColorChoice,
    /// Which glyph set to draw: `auto` (Unicode), `ascii`, or `none`.
    pub icons: IconChoice,
    /// Draw a transient activity line while a turn is working. On by default,
    /// but only ever drawn on a terminal and never under `--json`/`--quiet`.
    pub spinner: bool,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            color: ColorChoice::Auto,
            icons: IconChoice::Auto,
            spinner: true,
        }
    }
}

/// When ANSI colour is allowed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ColorChoice {
    /// Colour when the stream is a terminal, and never when it is a pipe.
    #[default]
    Auto,
    /// Always colour, even through a pipe (for `less -R` and friends).
    Always,
    /// Never colour. Layout is kept; `NO_COLOR` and `--no-color` do the same.
    Never,
}

/// Which glyph set the decorative marks use.
///
/// Kept separate from colour so a terminal that renders Unicode but refuses
/// colour can still show structure, and a screen reader or a legacy encoding
/// can degrade to ASCII or to nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum IconChoice {
    /// The Unicode dingbats the SDD's examples draw.
    #[default]
    Auto,
    /// ASCII fallbacks for terminals or fonts without the glyphs.
    Ascii,
    /// No decorative marks at all; wording carries the meaning.
    None,
}

/// Log rendering format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogFormat {
    /// Human-readable lines.
    #[default]
    Text,
    /// Newline-delimited JSON.
    Json,
}

impl Config {
    /// Load the layered configuration for a process started in `cwd`.
    ///
    /// `explicit` is the `--config` path, which wins over everything except
    /// environment variables and CLI flags.
    pub fn load(explicit: Option<&Path>, cwd: &Path) -> Result<Self> {
        Self::load_with(user_config_path().as_deref(), explicit, cwd)
    }

    /// Load with every config path supplied, for tests and embedders.
    ///
    /// Exists so tests can opt out of the developer's real user config instead
    /// of silently inheriting whatever `imp init` last wrote to it.
    pub fn load_with(user: Option<&Path>, explicit: Option<&Path>, cwd: &Path) -> Result<Self> {
        let mut merged = toml::Value::Table(toml::Table::new());

        if let Some(path) = user {
            merge_file(&mut merged, path)?;
        }
        merge_file(&mut merged, &project_config_path(cwd))?;
        if let Some(path) = explicit {
            merge_file(&mut merged, path)?;
        }

        let mut config: Config = match merged.as_table() {
            Some(table) if table.is_empty() => Config::default(),
            _ => merged
                .try_into()
                .map_err(|err| Error::Config(format!("invalid configuration: {err}")))?,
        };
        config.apply_env();
        config.validate()?;
        Ok(config)
    }

    /// Overlay `IMP_*` environment variables.
    fn apply_env(&mut self) {
        if let Ok(value) = std::env::var("IMP_MODEL") {
            self.provider.model = value;
        }
        if let Ok(value) = std::env::var("IMP_BASE_URL") {
            self.provider.base_url = value;
        }
        if let Ok(value) = std::env::var("IMP_API_KEY_ENV") {
            self.provider.api_key_env = value;
        }
        if let Ok(value) = std::env::var("IMP_LOG_LEVEL") {
            self.logging.level = value;
        }
        if let Ok(value) = std::env::var("IMP_MAX_ITERATIONS")
            && let Ok(parsed) = value.parse()
        {
            self.agent.max_iterations = parsed;
        }
    }

    /// Fill the small-model profile in where the user has not chosen a value.
    ///
    /// Called by `--lean`. It never clears a setting the user made: an explicit
    /// `context_tokens`, `max_tokens`, `stream_idle_timeout_secs` and the quirks
    /// are left as they are, and `max_iterations`/`max_tool_calls_per_turn` are
    /// only lowered when they are still at their "unset" defaults. The flag is
    /// applied *after* the config file, so it wins exactly as a global flag
    /// does — this is not a second config layer (D45).
    pub fn apply_lean_profile(&mut self) {
        self.agent.small_model = true;
        self.agent.nudge_on_empty = true;
        self.agent.repair_arguments = true;
        self.agent.context_tokens = self.agent.context_tokens.or(Some(LEAN_CONTEXT_TOKENS));
        self.agent.tool_result_chars = self
            .agent
            .tool_result_chars
            .or(Some(LEAN_TOOL_RESULT_CHARS));
        self.agent.max_tokens = self.agent.max_tokens.or(Some(LEAN_MAX_TOKENS));
        if self.agent.max_tool_calls_per_turn == 0 {
            self.agent.max_tool_calls_per_turn = LEAN_MAX_TOOL_CALLS;
        }
        // `supports_usage_in_stream` defaults on and a local server usually
        // ignores `stream_options`; only flip it from the built-in default.
        if self.provider.supports_usage_in_stream
            && self.provider.stream_idle_timeout_secs
                == ProviderConfig::default().stream_idle_timeout_secs
        {
            self.provider.supports_usage_in_stream = false;
        }
        if self.provider.stream_idle_timeout_secs
            == ProviderConfig::default().stream_idle_timeout_secs
        {
            self.provider.stream_idle_timeout_secs = LEAN_STREAM_IDLE_SECS;
        }
        self.provider.quirks.sanitize_schemas = true;
        self.provider.quirks.omit_parallel_tool_calls = true;
    }

    /// Reject configurations that cannot work, before any network call.
    fn validate(&self) -> Result<()> {
        if self.provider.base_url.trim().is_empty() {
            return Err(Error::Config(
                "provider.base_url must not be empty".to_string(),
            ));
        }
        if self.provider.model.trim().is_empty() {
            return Err(Error::Config(
                "provider.model must not be empty".to_string(),
            ));
        }
        if self.agent.max_iterations == 0 {
            return Err(Error::Config(
                "agent.max_iterations must be at least 1".to_string(),
            ));
        }
        if self.workspace.roots.is_empty() {
            return Err(Error::Config(
                "workspace.roots must list at least one root".to_string(),
            ));
        }
        if self.exec.shell.trim().is_empty() {
            return Err(Error::Config("exec.shell must not be empty".to_string()));
        }
        // A blank tool name can never match anything, so it is a typo rather
        // than a silent no-op. Rejecting it here says so at startup.
        for (section, names) in [
            ("tools.only", &self.tools.only),
            ("tools.hide", &self.tools.hide),
        ] {
            if names.iter().any(|name| name.trim().is_empty()) {
                return Err(Error::Config(format!(
                    "{section} must not name an empty tool"
                )));
            }
        }
        // A `[policy.read_only].extra` entry is a bare command verb. A path, a
        // pattern or an empty string can never match the way the classifier
        // does, so it is a typo worth surfacing rather than a silent no-op.
        for verb in &self.policy.read_only.extra {
            if !crate::read_only::is_bare_verb(verb) {
                return Err(Error::Config(format!(
                    "policy.read_only.extra entry `{verb}` must be a bare command verb"
                )));
            }
        }
        if self.http_fetch.max_bytes == 0 {
            return Err(Error::Config(
                "http_fetch.max_bytes must be at least 1".to_string(),
            ));
        }
        if self.http_fetch.timeout_secs == 0 {
            return Err(Error::Config(
                "http_fetch.timeout_secs must be at least 1".to_string(),
            ));
        }
        if self.guard.timeout_secs == 0 {
            return Err(Error::Config(
                "guard.timeout_secs must be at least 1".to_string(),
            ));
        }
        if self.provider.stream_idle_timeout_secs == 0 {
            return Err(Error::Config(
                "provider.stream_idle_timeout_secs must be at least 1".to_string(),
            ));
        }
        // Zero is not "unlimited" for these: an absent budget is spelled by
        // omitting the key, so a zero is a mistake worth surfacing.
        for (name, value) in [
            ("agent.context_tokens", self.agent.context_tokens),
            ("agent.tool_result_chars", self.agent.tool_result_chars),
        ] {
            if value == Some(0) {
                return Err(Error::Config(format!("{name} must be at least 1 when set")));
            }
        }
        if self.agent.max_tokens == Some(0) {
            return Err(Error::Config(
                "agent.max_tokens must be at least 1 when set".to_string(),
            ));
        }
        // `extra_body` exists for backend-specific sampling. Letting it shadow a
        // core request field would silently break the agent loop, so a collision
        // is rejected rather than merged.
        for key in self.provider.extra_body.keys() {
            if PROTECTED_BODY_KEYS.contains(&key.as_str()) {
                return Err(Error::Config(format!(
                    "provider.extra_body may not set `{key}`; it is part of the request the \
                     agent loop builds"
                )));
            }
        }
        // Thresholds are probabilities. A NaN fails both range checks, which is
        // the point: an unset threshold must not silently become a comparison
        // that can only ever allow.
        for (name, value) in [
            ("allow_threshold", self.guard.allow_threshold),
            ("deny_threshold", self.guard.deny_threshold),
        ] {
            if !(0.0..=1.0).contains(&value) {
                return Err(Error::Config(format!(
                    "guard.{name} must be a probability between 0 and 1, was {value}"
                )));
            }
        }
        if self.guard.allow_threshold > self.guard.deny_threshold {
            return Err(Error::Config(
                "guard.allow_threshold must not exceed guard.deny_threshold".to_string(),
            ));
        }
        if self.guard.enabled && self.guard.base_url.trim().is_empty() {
            return Err(Error::Config(
                "guard.base_url must be set when guard.enabled is true".to_string(),
            ));
        }
        if self.cron.timezone.trim().is_empty() {
            return Err(Error::Config(
                "cron.timezone must be an IANA name such as UTC or America/Mexico_City".to_string(),
            ));
        }
        if self.cron.max_concurrent_jobs == 0 {
            return Err(Error::Config(
                "cron.max_concurrent_jobs must be at least 1".to_string(),
            ));
        }
        if self.cron.missed_run_cap == 0 {
            return Err(Error::Config(
                "cron.missed_run_cap must be at least 1".to_string(),
            ));
        }
        // A server is spawned by its `command` or reached at its `url`, and its
        // table key is half of every tool name it publishes. Both are checked
        // here rather than at first use, so a typo is a startup error and not a
        // server that quietly never contributes a tool (§5.10).
        for (name, server) in &self.mcp.client.servers {
            if name.trim().is_empty() {
                return Err(Error::Config(
                    "an mcp.client.servers key must not be empty".to_string(),
                ));
            }
            if name.contains("__") {
                return Err(Error::Config(format!(
                    "mcp.client.servers.{name}: a server name must not contain `__`, \
                     which is the separator in `mcp__<server>__<tool>`"
                )));
            }
            let has_command = !server.command.trim().is_empty();
            let has_url = !server.url.trim().is_empty();
            if has_command && has_url {
                return Err(Error::Config(format!(
                    "mcp.client.servers.{name}: `command` (stdio) and `url` (HTTP) are \
                     mutually exclusive; set exactly one"
                )));
            }
            if !has_command && !has_url {
                return Err(Error::Config(format!(
                    "mcp.client.servers.{name}: one of `command` (stdio) or `url` (HTTP) \
                     is required"
                )));
            }
            if has_url {
                let url = server.url.trim();
                if !(url.starts_with("http://") || url.starts_with("https://")) {
                    return Err(Error::Config(format!(
                        "mcp.client.servers.{name}.url must be an `http://` or `https://` \
                         endpoint, was `{url}`"
                    )));
                }
                if !server.args.is_empty() {
                    return Err(Error::Config(format!(
                        "mcp.client.servers.{name}: `args` belongs to a stdio `command`, \
                         not to a `url`"
                    )));
                }
            }
        }
        // Peers: a name is half of `peer__<name>_ask`, and a peer is reached by
        // its `command` or its `url`. Both are checked at load time so a typo is
        // a startup error rather than a delegation tool that never works (M10.2).
        for (name, peer) in &self.peers {
            if name.trim().is_empty() {
                return Err(Error::Config("a peers key must not be empty".to_string()));
            }
            if name.contains("__") {
                return Err(Error::Config(format!(
                    "peers.{name}: a peer name must not contain `__`, which separates \
                     the namespace in `peer__<name>_ask`"
                )));
            }
            let has_command = !peer.command.trim().is_empty();
            let has_url = !peer.url.trim().is_empty();
            if has_command && has_url {
                return Err(Error::Config(format!(
                    "peers.{name}: `command` (stdio) and `url` (HTTP) are mutually \
                     exclusive; set exactly one"
                )));
            }
            if !has_command && !has_url {
                return Err(Error::Config(format!(
                    "peers.{name}: one of `command` (stdio) or `url` (HTTP) is required"
                )));
            }
            if has_url {
                let url = peer.url.trim();
                if !(url.starts_with("http://") || url.starts_with("https://")) {
                    return Err(Error::Config(format!(
                        "peers.{name}.url must be an `http://` or `https://` endpoint, \
                         was `{url}`"
                    )));
                }
                if !peer.args.is_empty() {
                    return Err(Error::Config(format!(
                        "peers.{name}: `args` belongs to a stdio `command`, not to a `url`"
                    )));
                }
            }
            if peer.result_cap_bytes == 0 {
                return Err(Error::Config(format!(
                    "peers.{name}.result_cap_bytes must be at least 1"
                )));
            }
        }
        // The server half: `stdio` (the default) or `http`. A second transport is
        // a deliberate config value, and an unknown one is a startup error
        // rather than a server that quietly does something else.
        if self.mcp.server.enabled {
            match self.mcp.server.transport.trim() {
                "stdio" => {}
                transport if transport.eq_ignore_ascii_case("http") => {
                    // The bind policy runs at load time, so a wildcard, a public
                    // address, or a non-loopback bind without a token is refused
                    // before the socket is ever opened (fail-closed).
                    self.mcp.server.bind_socket()?;
                }
                other => {
                    return Err(Error::Config(format!(
                        "mcp.server.transport must be \"stdio\" or \"http\", was `{other}`"
                    )));
                }
            }
        }
        // The update channel is a network endpoint whose response decides what
        // binary gets installed, so its shape is checked here rather than at the
        // first request: a misconfigured channel fails before anything is
        // downloaded, and plain `http` is refused except on loopback.
        if self.update.api_url.trim().is_empty() {
            return Err(Error::Config(
                "update.api_url must not be empty".to_string(),
            ));
        }
        if !crate::update::url_is_permitted(&self.update.api_url) {
            return Err(Error::Config(format!(
                "update.api_url must be https (or http on loopback), was `{}`",
                self.update.api_url
            )));
        }
        if self.update.asset_prefix.trim().is_empty() {
            return Err(Error::Config(
                "update.asset_prefix must not be empty".to_string(),
            ));
        }
        let repo = self.update.repo.trim();
        if repo.split('/').count() != 2 || repo.split('/').any(|part| part.trim().is_empty()) {
            return Err(Error::Config(format!(
                "update.repo must be `owner/name`, was `{repo}`"
            )));
        }
        Ok(())
    }

    /// Read the API key named by `provider.api_key_env`.
    ///
    /// The key is never stored in the config file and never logged.
    /// Read the API key.
    ///
    /// Resolution order, so a single shell or CI job can override the stored
    /// secret without editing anything:
    ///
    /// 1. the environment variable named by `provider.api_key_env`, if set and non-empty;
    /// 2. `provider.api_key` in the file named by `provider.api_key_file`;
    /// 3. otherwise an error, unless *both* sources are unset, which is how a
    ///    keyless local backend is expressed.
    pub fn api_key(&self) -> Result<Option<String>> {
        resolve_secret(
            &self.provider.api_key_env,
            &self.provider.api_key_file,
            "credentials",
            " (or run `imp init`)",
        )
    }

    /// Headers to send on every provider request.
    ///
    /// `${session}` in a value is replaced by the conversation identifier by the
    /// provider client; it is passed through verbatim here.
    pub fn request_headers(&self) -> Vec<(String, String)> {
        self.provider
            .headers
            .iter()
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect()
    }

    /// Canonical read-write root, resolved against `cwd`.
    pub fn workspace_root(&self, cwd: &Path) -> Result<PathBuf> {
        let raw = self
            .workspace
            .roots
            .first()
            .ok_or_else(|| Error::Config("workspace.roots is empty".to_string()))?;
        let path = cwd.join(raw);
        Ok(path.canonicalize().unwrap_or(path))
    }

    /// Path the session database lives at.
    pub fn database_path(&self) -> PathBuf {
        project_dirs(APP)
            .and_then(|dirs| dirs.state_dir().map(Path::to_path_buf))
            .unwrap_or_else(|| PathBuf::from("."))
            .join(format!("{APP}.db"))
    }
}

/// The `ProjectDirs` for one application directory name.
fn project_dirs(app: &str) -> Option<ProjectDirs> {
    ProjectDirs::from("", "", app)
}

/// `$XDG_CONFIG_HOME/imp/config.toml`, or the macOS equivalent.
pub fn user_config_path() -> Option<PathBuf> {
    project_dirs(APP).map(|dirs| dirs.config_dir().join("config.toml"))
}

/// The default credentials file, beside the user config.
///
/// Kept separate from the config so that a config may be committed while the
/// secret never is.
pub fn default_credentials_path() -> Option<PathBuf> {
    project_dirs(APP).map(|dirs| dirs.config_dir().join("credentials"))
}

/// The project config file to read from `cwd` (SDD D46).
///
/// `imp.toml` is the current name. A workspace that still carries the
/// pre-rename `minion.toml` and no `imp.toml` is read from the old file rather
/// than silently falling back to the built-in defaults, and the notice says
/// which file was used. The file is *read*, never moved: a checkout is the
/// user's, and a tool that renames files in someone's repository to tidy up
/// is a worse neighbour than one that says what it read. The notice is a plain
/// `eprintln!` because this runs while the config is being loaded, which is
/// before the redacting log sink exists; it carries a path and no secret.
fn project_config_path(cwd: &Path) -> PathBuf {
    let current = cwd.join(PROJECT_CONFIG);
    if current.exists() {
        warn_about_project_config(&current);
        return current;
    }
    let legacy = cwd.join(LEGACY_PROJECT_CONFIG);
    if legacy.exists() {
        eprintln!(
            "imp: reading {} — rename it to {} (SDD D46)",
            legacy.display(),
            current.display()
        );
        warn_about_project_config(&legacy);
        return legacy;
    }
    current
}

/// Rename the pre-rename directories onto their `imp` counterparts (SDD D46).
///
/// Returns one human-readable notice per thing it did, refused to do or failed
/// to do, for the caller to print at startup. Called once, from `main`, before
/// the configuration is loaded: the config file, the credentials and the
/// session database all live under these directories, so [`ProjectDirs`]
/// switching its `APP` without this would leave `imp` starting on an empty
/// database while the user's sessions and cron jobs sat next door under the
/// old name.
///
/// Two rules keep it safe rather than clever:
///
/// - It only runs when the destination does **not** exist. A directory
///   `rename` is atomic and keeps modes — the credentials file stays `0600` —
///   but two directories that both exist are ambiguous, so nothing is touched
///   and the notice says to merge them by hand. Half-merging is worse than
///   not merging.
/// - A failure is reported, never fatal, and never a fallback to an empty
///   database: if the rename cannot proceed the old files are still exactly
///   where they were.
pub fn migrate_legacy_state() -> Vec<String> {
    let mut notes = Vec::new();
    let (Some(old), Some(new)) = (project_dirs(LEGACY_APP), project_dirs(APP)) else {
        return notes;
    };

    notes.extend(migrate_dir("config", old.config_dir(), new.config_dir()));
    if let (Some(old_state), Some(new_state)) = (old.state_dir(), new.state_dir()) {
        notes.extend(migrate_dir("state", old_state, new_state));
    }
    // The database is the file the two directory names also disagree about,
    // and it is the one thing in the state directory. Renaming the directory
    // leaves `imp/minion.db`, so the file is renamed in place afterwards; it
    // is also what is left to do when an `imp` directory already existed.
    if let Some(state) = new.state_dir() {
        match migrate_db_files(state) {
            Ok(0) => {}
            Ok(_) => notes.push(format!(
                "renamed the session database in {} to {APP}.db",
                state.display()
            )),
            Err(err) => notes.push(format!(
                "could not rename the old session database in {}: {err}",
                state.display()
            )),
        }
    }
    notes
}

/// Move `old` onto `new`, but only when `new` does not exist yet.
fn migrate_dir(label: &str, old: &Path, new: &Path) -> Vec<String> {
    if !old.exists() {
        return Vec::new();
    }
    if new.exists() {
        return vec![format!(
            "both {label} directories exist ({} and {}); nothing was migrated — merge them by hand",
            old.display(),
            new.display()
        )];
    }
    if let Some(parent) = new.parent()
        && let Err(err) = std::fs::create_dir_all(parent)
    {
        return vec![format!("could not create {}: {err}", parent.display())];
    }
    match std::fs::rename(old, new) {
        Ok(()) => vec![format!(
            "migrated the {label} directory {} -> {}",
            old.display(),
            new.display()
        )],
        // Another `imp` that won the race makes the source vanish under us;
        // that is the outcome we wanted, not a failure worth reporting.
        Err(_) if !old.exists() || new.exists() => Vec::new(),
        Err(err) => vec![format!(
            "could not migrate the {label} directory {}: {err}",
            old.display()
        )],
    }
}

/// Rename `minion.db` and its SQLite sidecars to `imp.db` inside `dir`.
///
/// Returns how many files were renamed. A missing `dir` is not an error: that
/// is what a first run on a fresh machine looks like.
fn migrate_db_files(dir: &Path) -> std::io::Result<usize> {
    if !dir.exists() {
        return Ok(0);
    }
    let mut moved = 0;
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        // `minion.db` itself, plus `-wal`, `-shm` and `-journal`: renaming the
        // database out from under a live write-ahead log would leave SQLite
        // reading a log that belongs to a file it can no longer find.
        let Some(suffix) = name.strip_prefix(LEGACY_DB_FILE) else {
            continue;
        };
        if !suffix.is_empty() && !suffix.starts_with('-') {
            continue;
        }
        let destination = dir.join(format!("{APP}.db{suffix}"));
        if destination.exists() {
            continue;
        }
        match std::fs::rename(entry.path(), &destination) {
            Ok(()) => moved += 1,
            Err(_) if !entry.path().exists() => {}
            Err(err) => return Err(err),
        }
    }
    Ok(moved)
}

/// The secret half of the configuration.
///
/// This file is written by `imp init` and is never committed, never logged,
/// and never included in `--json` output.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Credentials {
    /// The API key, stored as written.
    pub api_key: String,
}

impl Credentials {
    /// Read the credentials file, treating an absent file as empty.
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(path)?;
        toml::from_str(&text).map_err(|err| {
            Error::Auth(format!(
                "cannot read credentials at {}: {err}",
                path.display()
            ))
        })
    }

    /// Write the credentials file with owner-only permissions.
    pub fn write(path: &Path, api_key: &str) -> Result<()> {
        let body = format!(
            "# imp credentials. Written by `imp init`; never commit this.\n\
             # The `Authorization` header is built from this value at run time.\n\n\
             api_key = {}\n",
            toml::Value::String(api_key.to_string())
        );
        crate::fs::write_private_file(path, &body)
    }
}

/// Resolve a secret from an environment-variable *name* and a credentials-file
/// *path*, neither of which is the secret itself.
///
/// This is the one resolution both `provider.api_key` and every bearer token
/// (the MCP client's and the MCP server's) go through, so "where does a secret
/// come from" has a single answer: the environment variable wins when it is set
/// and non-empty, then `api_key` in the credentials file — the `0600` file
/// `imp init` writes. The config only ever carries names and paths.
///
/// `Ok(None)` means "no source is configured", which is how a keyless backend —
/// or a peer that needs no token — is expressed. A source that *is* named but
/// yields nothing is an error: silently sending no credentials would turn a
/// typo into an auth failure somewhere else.
fn resolve_secret(
    env_name: &str,
    file_path: &str,
    what: &str,
    remedy: &str,
) -> Result<Option<String>> {
    let env_name = env_name.trim();
    let file_path = file_path.trim();

    if !env_name.is_empty()
        && let Ok(value) = std::env::var(env_name)
        && !value.trim().is_empty()
    {
        return Ok(Some(value));
    }

    if !file_path.is_empty() {
        let path = expand_home(file_path);
        let credentials = Credentials::load(&path)?;
        if !credentials.api_key.trim().is_empty() {
            return Ok(Some(credentials.api_key));
        }
    }

    if env_name.is_empty() && file_path.is_empty() {
        return Ok(None);
    }

    let mut sources = Vec::new();
    if !env_name.is_empty() {
        sources.push(format!("the environment variable `{env_name}`"));
    }
    if !file_path.is_empty() {
        sources.push(format!("`api_key` in {file_path}"));
    }
    Err(Error::Auth(format!(
        "no {what} found — set {}{remedy}",
        sources.join(" or ")
    )))
}

/// Expand a leading `~` to the user's home directory.
fn expand_home(path: &str) -> PathBuf {
    let Some(rest) = path.strip_prefix('~') else {
        return PathBuf::from(path);
    };
    let Some(home) = directories::BaseDirs::new().map(|dirs| dirs.home_dir().to_path_buf()) else {
        return PathBuf::from(path);
    };
    match rest.trim_start_matches('/') {
        "" => home,
        rest => home.join(rest),
    }
}

/// Merge a TOML file into `base`, ignoring a missing file.
fn merge_file(base: &mut toml::Value, path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let text = std::fs::read_to_string(path)?;
    let value: toml::Value =
        toml::from_str(&text).map_err(|err| Error::Config(format!("{}: {err}", path.display())))?;
    deep_merge(base, value);
    Ok(())
}

/// Recursively overlay tables; scalars and arrays replace wholesale.
fn deep_merge(base: &mut toml::Value, over: toml::Value) {
    match (base, over) {
        (toml::Value::Table(base_table), toml::Value::Table(over_table)) => {
            for (key, value) in over_table {
                match base_table.get_mut(&key) {
                    Some(slot) => deep_merge(slot, value),
                    None => {
                        base_table.insert(key, value);
                    }
                }
            }
        }
        (slot, value) => *slot = value,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write(path: &Path, body: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
    }

    #[test]
    fn defaults_match_the_spec() {
        let config = Config::default();
        assert_eq!(config.agent.max_iterations, 25);
        assert_eq!(
            config.exec.output_cap_bytes, 262_144,
            "NFR-4: a 256 KiB cap"
        );
        assert_eq!(
            config.exec.default_timeout_secs, 120,
            "NFR-5: a two-minute default command timeout"
        );
        assert_eq!(config.policy.default, Decision::Ask);
        assert_eq!(config.policy.noninteractive, Decision::Deny);
        assert!(!config.workspace.follow_symlinks);
    }

    /// The M10.3 knobs must be inert by default: an operator opts in, and an
    /// existing config keeps behaving exactly as it did.
    #[test]
    fn the_loop_ergonomics_default_to_the_current_behaviour() {
        let config = Config::default();
        assert_eq!(
            config.agent.max_tool_calls_per_turn, 0,
            "0 means unlimited, so the budget is off until it is set"
        );
        assert!(!config.agent.small_model);
        assert!(config.tools.only.is_empty());
        assert!(config.tools.hide.is_empty());
        assert!(config.tools.selection().is_unrestricted());
        assert!(!config.provider.strict_tool_arguments);
    }

    #[test]
    fn the_tools_selection_is_read_from_a_project_file() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join("imp.toml"),
            "[tools]\nonly = [\"read_file\", \"edit_file\"]\nhide = [\"http_fetch\"]\n",
        );

        let config = Config::load_with(None, None, dir.path()).unwrap();

        assert_eq!(
            config.tools.only,
            vec!["read_file".to_string(), "edit_file".to_string()]
        );
        assert_eq!(config.tools.hide, vec!["http_fetch".to_string()]);
        let selection = config.tools.selection();
        assert!(selection.admits("read_file"));
        assert!(!selection.admits("http_fetch"), "hide beats only");
        assert!(!selection.admits("run_command"), "only restricts the rest");
    }

    #[test]
    fn a_blank_tool_name_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join("imp.toml"),
            "[tools]\nonly = [\"read_file\", \"\"]\n",
        );

        let err = Config::load_with(None, None, dir.path()).unwrap_err();

        assert!(err.to_string().contains("tools.only"), "was: {err}");
    }

    #[test]
    fn the_small_model_and_strict_arguments_quirks_are_read() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join("imp.toml"),
            "[agent]\nsmall_model = true\nmax_tool_calls_per_turn = 12\n\
             \n[provider]\nstrict_tool_arguments = true\n",
        );

        let config = Config::load_with(None, None, dir.path()).unwrap();

        assert!(config.agent.small_model);
        assert_eq!(config.agent.max_tool_calls_per_turn, 12);
        assert!(config.provider.strict_tool_arguments);
    }

    #[test]
    fn http_fetch_defaults_to_a_closed_guard() {
        let config = Config::default();
        assert!(
            config.http_fetch.allowed_domains.is_empty(),
            "an empty allowlist means nothing is fetchable until it is configured"
        );
        assert!(
            config.http_fetch.block_private_ips,
            "the private-IP block must default on; it is the SSRF mitigation"
        );
        assert_eq!(config.http_fetch.max_bytes, 1_048_576);
        assert_eq!(config.http_fetch.timeout_secs, 20);
    }

    #[test]
    fn the_http_fetch_section_is_read_from_a_project_file() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join("imp.toml"),
            "[http_fetch]\nallowed_domains = [\"docs.rs\", \"*.github.com\"]\n\
             block_private_ips = false\nmax_bytes = 4096\ntimeout_secs = 3\n",
        );

        let config = Config::load_with(None, None, dir.path()).unwrap();

        assert_eq!(
            config.http_fetch.allowed_domains,
            vec!["docs.rs".to_string(), "*.github.com".to_string()]
        );
        assert!(!config.http_fetch.block_private_ips);
        assert_eq!(config.http_fetch.max_bytes, 4096);
        assert_eq!(config.http_fetch.timeout_secs, 3);
    }

    #[test]
    fn an_unusable_http_fetch_cap_is_rejected_before_use() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join("imp.toml"),
            "[http_fetch]\nmax_bytes = 0\n",
        );

        let err = Config::load_with(None, None, dir.path()).unwrap_err();

        assert!(matches!(err, Error::Config(_)), "unexpected error: {err}");
    }

    #[test]
    fn the_guard_is_disabled_by_default() {
        let config = Config::default();
        assert!(
            !config.guard.enabled,
            "the guard is a network call carrying command text, so it stays opt-in"
        );
        assert!(config.guard.base_url.is_empty());
        assert_eq!(config.guard.allow_threshold, 0.25);
        assert_eq!(config.guard.deny_threshold, 0.75);
        assert_eq!(config.guard.timeout_secs, 10);
    }

    #[test]
    fn the_guard_section_is_read_from_a_project_file() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join("imp.toml"),
            "[guard]\nenabled = true\nbase_url = \"http://127.0.0.1:8080\"\n\
             allow_threshold = 0.1\ndeny_threshold = 0.9\ntimeout_secs = 3\n",
        );

        let config = Config::load_with(None, None, dir.path()).unwrap();

        assert!(config.guard.enabled);
        assert_eq!(config.guard.base_url, "http://127.0.0.1:8080");
        assert_eq!(config.guard.allow_threshold, 0.1);
        assert_eq!(config.guard.deny_threshold, 0.9);
        assert_eq!(config.guard.timeout_secs, 3);
    }

    #[test]
    fn cron_defaults_match_the_spec() {
        let config = Config::default();
        assert!(
            config.cron.enabled,
            "the tick loop is on unless told otherwise"
        );
        assert_eq!(config.cron.missed_run_policy, MissedRunPolicy::RunOnce);
        assert_eq!(config.cron.max_concurrent_jobs, 2);
        assert_eq!(config.cron.missed_run_cap, 20);
    }

    #[test]
    fn the_cron_section_is_read_from_a_project_file() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join("imp.toml"),
            "[cron]\nenabled = false\ntimezone = \"America/Mexico_City\"\n\
             missed_run_policy = \"run_all\"\nmax_concurrent_jobs = 4\nmissed_run_cap = 5\n",
        );

        let config = Config::load_with(None, None, dir.path()).unwrap();

        assert!(!config.cron.enabled);
        assert_eq!(config.cron.timezone, "America/Mexico_City");
        assert_eq!(config.cron.missed_run_policy, MissedRunPolicy::RunAll);
        assert_eq!(config.cron.max_concurrent_jobs, 4);
        assert_eq!(config.cron.missed_run_cap, 5);
    }

    #[test]
    fn an_unusable_cron_bound_is_rejected_before_use() {
        for body in [
            "[cron]\nmax_concurrent_jobs = 0\n",
            "[cron]\nmissed_run_cap = 0\n",
            "[cron]\ntimezone = \"\"\n",
        ] {
            let dir = tempfile::tempdir().unwrap();
            write(&dir.path().join("imp.toml"), body);
            let err = Config::load_with(None, None, dir.path()).unwrap_err();
            assert!(matches!(err, Error::Config(_)), "`{body}` gave: {err}");
        }
    }

    #[test]
    fn an_enabled_guard_needs_a_base_url() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("imp.toml"), "[guard]\nenabled = true\n");

        let err = Config::load_with(None, None, dir.path()).unwrap_err();

        assert!(matches!(err, Error::Config(_)), "unexpected error: {err}");
        assert!(err.to_string().contains("base_url"), "was: {err}");
    }

    #[test]
    fn mcp_servers_are_read_from_a_project_file() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join("imp.toml"),
            "[mcp.client.servers.files]\n\
             command = \"mcp-server-files\"\n\
             args = [\"--root\", \"/srv\"]\n\
             lazy = true\n\
             tool_allow = [\"read_*\", \"list\"]\n\
             approval = \"auto\"\n\
             \n\
             [mcp.client.servers.web]\n\
             command = \"mcp-server-web\"\n",
        );

        let config = Config::load_with(None, None, dir.path()).unwrap();

        let files = &config.mcp.client.servers["files"];
        assert_eq!(files.command, "mcp-server-files");
        assert_eq!(files.args, vec!["--root".to_string(), "/srv".to_string()]);
        assert!(files.lazy);
        assert_eq!(files.approval, Some(Decision::Auto));
        assert!(files.allows("read_file"), "`read_*` must name `read_file`");
        assert!(files.allows("list"));
        assert!(!files.allows("write_file"));

        let web = &config.mcp.client.servers["web"];
        assert_eq!(web.command, "mcp-server-web");
        assert!(web.args.is_empty());
        assert!(!web.lazy, "a server is eager unless it says otherwise");
        assert_eq!(web.approval, None);
        assert!(
            !web.allows("anything"),
            "an empty tool_allow must allow nothing; the server is fail-closed"
        );
    }

    #[test]
    fn an_empty_tool_allow_is_not_a_wildcard() {
        let server = McpServerConfig {
            command: "x".to_string(),
            ..McpServerConfig::default()
        };
        assert!(!server.allows("read_file"));

        let wildcard = McpServerConfig {
            tool_allow: vec!["*".to_string()],
            ..server
        };
        assert!(wildcard.allows("read_file"));
        assert!(wildcard.allows("anything_at_all"));
    }

    #[test]
    fn flattened_names_come_from_one_place() {
        assert_eq!(
            McpServerConfig::flattened("files", "read_file"),
            "mcp__files__read_file"
        );
        assert_eq!(McpServerConfig::tool_prefix("files"), "mcp__files__");
    }

    #[test]
    fn a_server_name_that_breaks_the_namespace_is_rejected() {
        for (name, body) in [
            ("a__b", "[mcp.client.servers.a__b]\ncommand = \"x\"\n"),
            ("empty", "[mcp.client.servers.\"\"]\ncommand = \"x\"\n"),
            ("nocommand", "[mcp.client.servers.b]\nargs = [\"x\"]\n"),
        ] {
            let dir = tempfile::tempdir().unwrap();
            write(&dir.path().join("imp.toml"), body);
            let err = Config::load_with(None, None, dir.path()).unwrap_err();
            assert!(
                matches!(err, Error::Config(_)),
                "{name} was accepted: {err}"
            );
        }
    }

    #[test]
    fn no_mcp_servers_is_the_default() {
        let config = Config::default();
        assert!(config.mcp.client.servers.is_empty());
    }

    // -------------------------------------------------------------- peers (M10.2)

    #[test]
    fn no_peers_is_the_default() {
        let config = Config::default();
        assert!(config.peers.is_empty());
    }

    /// A peer names *one* tool, `peer__<name>_ask`, and is reached like any
    /// other MCP server. Both are config facts, so both are tested here.
    #[test]
    fn a_peer_is_read_from_a_project_file_and_names_one_tool() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join("imp.toml"),
            "[peers.big]\nurl = \"http://big.tailnet.ts.net:8788/mcp\"\n\
             token_file = \"/home/me/.config/imp/big.credentials\"\n\
             max_tokens = 512\nresult_cap_bytes = 4096\napproval = \"ask\"\n",
        );

        let config = Config::load_with(None, None, dir.path()).unwrap();
        let big = &config.peers["big"];
        assert!(big.is_http());
        assert_eq!(big.url, "http://big.tailnet.ts.net:8788/mcp");
        assert_eq!(big.max_tokens, Some(512));
        assert_eq!(big.result_cap_bytes, 4096);
        assert_eq!(big.approval, Some(Decision::Ask));
        assert_eq!(Peer::tool_name("big"), "peer__big_ask");
    }

    /// Each check is a startup error rather than a delegation tool that never
    /// works: a peer with no reach, two reaches, a bad scheme, or no room for a
    /// result is refused before anything is called.
    #[test]
    fn a_peer_needs_exactly_one_reach_and_a_usable_cap() {
        let cases = [
            ("neither", "[peers.a]\n"),
            (
                "both",
                "[peers.a]\ncommand = \"x\"\nurl = \"http://h/mcp\"\n",
            ),
            ("scheme", "[peers.a]\nurl = \"127.0.0.1:8788/mcp\"\n"),
            (
                "args",
                "[peers.a]\nurl = \"http://h/mcp\"\nargs = [\"-x\"]\n",
            ),
            ("cap", "[peers.a]\ncommand = \"x\"\nresult_cap_bytes = 0\n"),
            ("name", "[peers.a__b]\ncommand = \"x\"\n"),
        ];
        for (label, body) in cases {
            let dir = tempfile::tempdir().unwrap();
            write(&dir.path().join("imp.toml"), body);
            let err = Config::load_with(None, None, dir.path()).unwrap_err();
            assert!(
                matches!(err, Error::Config(_)),
                "{label}: expected a config error, got: {err}"
            );
        }
    }

    /// §5.13: the result cap defaults to "around 8 KB", because the model that
    /// receives it has a small context.
    #[test]
    fn a_peer_result_cap_defaults_to_around_eight_kilobytes() {
        assert_eq!(Peer::default().result_cap_bytes, 8192);
        assert_eq!(Peer::default().max_tokens, None);
    }

    /// §5.1: the server half defaults to the read-only surface (D8, T5). Shell
    /// execution and file writes are off until an operator says otherwise.
    #[test]
    fn the_mcp_server_defaults_to_a_read_only_surface() {
        let config = Config::default();
        assert!(config.mcp.server.enabled);
        assert_eq!(config.mcp.server.transport, "stdio");
        assert!(!config.mcp.server.expose_exec);
        assert!(!config.mcp.server.expose_write);
        assert!(config.mcp.server.expose_cron_write);
    }

    #[test]
    fn the_mcp_server_section_is_read_from_a_project_file() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join("imp.toml"),
            "[mcp.server]\nexpose_exec = true\nexpose_cron_write = false\n",
        );

        let config = Config::load_with(None, None, dir.path()).unwrap();

        assert!(config.mcp.server.expose_exec);
        assert!(
            !config.mcp.server.expose_write,
            "an unset flag keeps its default"
        );
        assert!(!config.mcp.server.expose_cron_write);
    }

    /// R5: stdio is the only transport, and a second one is a startup error
    /// rather than a server that quietly does something else.
    #[test]
    fn a_non_stdio_mcp_transport_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join("imp.toml"),
            "[mcp.server]\ntransport = \"sse\"\n",
        );

        let err = Config::load_with(None, None, dir.path()).unwrap_err();

        assert!(matches!(err, Error::Config(_)), "unexpected error: {err}");
        assert!(err.to_string().contains("stdio"), "was: {err}");
    }

    // ------------------------------------------------- mcp transport (M10.1)

    /// §5.10/D25: a client server is reached by `command` (stdio) or `url`
    /// (HTTP), never both and never neither.
    #[test]
    fn a_client_server_is_stdio_or_http_but_not_both() {
        let cases: [(&str, &str, bool); 5] = [
            ("stdio", "[mcp.client.servers.a]\ncommand = \"x\"\n", true),
            (
                "http",
                "[mcp.client.servers.a]\nurl = \"http://127.0.0.1:8788/mcp\"\n",
                true,
            ),
            (
                "both",
                "[mcp.client.servers.a]\ncommand = \"x\"\nurl = \"http://127.0.0.1:8788/mcp\"\n",
                false,
            ),
            ("neither", "[mcp.client.servers.a]\nlazy = true\n", false),
            (
                "args-with-url",
                "[mcp.client.servers.a]\nurl = \"http://127.0.0.1:8788/mcp\"\nargs = [\"-x\"]\n",
                false,
            ),
        ];

        for (label, body, ok) in cases {
            let dir = tempfile::tempdir().unwrap();
            write(&dir.path().join("imp.toml"), body);
            let loaded = Config::load_with(None, None, dir.path());
            assert_eq!(loaded.is_ok(), ok, "{label}: {loaded:?}");
        }
    }

    /// The URL scheme is checked, so `ftp://` or a bare host is a startup error
    /// rather than a transport that fails at the first request.
    #[test]
    fn a_client_url_must_be_http() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join("imp.toml"),
            "[mcp.client.servers.a]\nurl = \"127.0.0.1:8788/mcp\"\n",
        );

        let err = Config::load_with(None, None, dir.path()).unwrap_err();

        assert!(matches!(err, Error::Config(_)), "unexpected error: {err}");
        assert!(err.to_string().contains("http://"), "was: {err}");
    }

    /// A client token is resolved from `token_file` (the `0600` credentials
    /// file), reusing the provider's resolution, and never from the config.
    #[test]
    fn a_client_token_comes_from_the_credentials_file() {
        let dir = tempfile::tempdir().unwrap();
        let credentials = dir.path().join("credentials");
        Credentials::write(&credentials, "peer-secret").unwrap();

        let server = McpServerConfig {
            url: "http://127.0.0.1:8788/mcp".to_string(),
            token_env: "IMP_ABSENT_PEER_VAR".to_string(),
            token_file: credentials.display().to_string(),
            ..McpServerConfig::default()
        };

        assert_eq!(
            server.bearer_token().unwrap().as_deref(),
            Some("peer-secret")
        );
    }

    /// A named-but-empty token source is an error, not a silent anonymous call.
    #[test]
    fn a_named_but_absent_token_is_an_error() {
        let server = McpServerConfig {
            token_env: "IMP_ABSENT_PEER_VAR".to_string(),
            token_file: "/nonexistent/credentials".to_string(),
            ..McpServerConfig::default()
        };

        let err = server.bearer_token().unwrap_err();

        assert!(matches!(err, Error::Auth(_)), "unexpected error: {err}");
        assert!(
            err.to_string().contains("IMP_ABSENT_PEER_VAR"),
            "was: {err}"
        );
    }

    /// `transport = "http"` is accepted; anything else is still a startup error.
    #[test]
    fn http_is_a_transport_and_nothing_else_is() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join("imp.toml"),
            "[mcp.server]\ntransport = \"http\"\nbind = \"127.0.0.1:0\"\n",
        );
        let config = Config::load_with(None, None, dir.path()).unwrap();
        assert!(config.mcp.server.is_http());

        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join("imp.toml"),
            "[mcp.server]\ntransport = \"ws\"\n",
        );
        let err = Config::load_with(None, None, dir.path()).unwrap_err();
        assert!(matches!(err, Error::Config(_)), "unexpected error: {err}");
    }

    /// The bind policy (D27): wildcard and public addresses are always refused;
    /// a non-loopback bind needs a token; loopback and a token-backed tailnet
    /// address are allowed.
    #[test]
    fn the_http_bind_is_fail_closed() {
        let with = |bind: &str| McpServerSection {
            transport: "http".to_string(),
            bind: bind.to_string(),
            ..McpServerSection::default()
        };

        // Loopback needs nothing.
        assert!(with("127.0.0.1:8788").bind_socket().is_ok());
        assert!(with("[::1]:8788").bind_socket().is_ok());

        // Wildcards are always refused — they are the public interface.
        assert!(with("0.0.0.0:8788").bind_socket().is_err());
        assert!(with("[::]:8788").bind_socket().is_err());

        // A globally routable address is never served.
        assert!(with("93.184.216.34:8788").bind_socket().is_err());

        // A tailnet address without a token is refused...
        assert!(with("100.101.102.103:8788").bind_socket().is_err());
        assert!(with("[fd7a:115c:a1e0::1]:8788").bind_socket().is_err());

        // ...and allowed once a token source is named.
        let tailnet = McpServerSection {
            transport: "http".to_string(),
            bind: "100.101.102.103:8788".to_string(),
            token_env: "IMP_PEER_TOKEN".to_string(),
            ..McpServerSection::default()
        };
        assert!(tailnet.bind_socket().is_ok());
        assert!(tailnet.has_token_source());

        // A malformed address is a clear config error, not a panic.
        let err = with("not-an-address").bind_socket().unwrap_err();
        assert!(err.to_string().contains("mcp.server.bind"), "was: {err}");
    }

    /// (b) A non-loopback bind without a token is refused when the config is
    /// loaded, so `imp mcp serve` never reaches `TcpListener::bind`: the
    /// listener does not start, rather than starting unauthenticated.
    #[test]
    fn a_non_loopback_http_bind_without_a_token_never_loads() {
        for bind in ["0.0.0.0:8788", "[::]:8788", "100.101.102.103:8788"] {
            let dir = tempfile::tempdir().unwrap();
            write(
                &dir.path().join("imp.toml"),
                &format!("[mcp.server]\ntransport = \"http\"\nbind = \"{bind}\"\n"),
            );

            let err = Config::load_with(None, None, dir.path()).unwrap_err();

            assert!(matches!(err, Error::Config(_)), "{bind}: {err}");
        }
    }

    #[test]
    fn guard_thresholds_must_be_ordered_probabilities() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join("imp.toml"),
            "[guard]\nallow_threshold = 0.9\ndeny_threshold = 0.1\n",
        );
        let err = Config::load_with(None, None, dir.path()).unwrap_err();
        assert!(matches!(err, Error::Config(_)), "unexpected error: {err}");

        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join("imp.toml"),
            "[guard]\nallow_threshold = 1.5\n",
        );
        let err = Config::load_with(None, None, dir.path()).unwrap_err();
        assert!(matches!(err, Error::Config(_)), "unexpected error: {err}");
    }

    #[test]
    fn a_zero_guard_timeout_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("imp.toml"), "[guard]\ntimeout_secs = 0\n");

        let err = Config::load_with(None, None, dir.path()).unwrap_err();

        assert!(matches!(err, Error::Config(_)), "unexpected error: {err}");
    }

    #[test]
    fn project_file_overrides_only_the_keys_it_sets() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path();
        write(
            &cwd.join("imp.toml"),
            "[provider]\nmodel = \"local-model\"\n",
        );

        let config = Config::load_with(None, None, cwd).unwrap();

        assert_eq!(config.provider.model, "local-model");
        // Untouched keys keep their defaults.
        assert_eq!(config.provider.base_url, ProviderConfig::default().base_url);
        assert_eq!(config.agent.max_iterations, 25);
    }

    #[test]
    fn explicit_config_beats_the_project_file() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path();
        write(
            &cwd.join("imp.toml"),
            "[provider]\nmodel = \"from-project\"\n",
        );
        let explicit = cwd.join("elsewhere.toml");
        write(&explicit, "[provider]\nmodel = \"from-flag\"\n");

        let config = Config::load_with(None, Some(&explicit), cwd).unwrap();

        assert_eq!(config.provider.model, "from-flag");
    }

    #[test]
    fn nested_tables_merge_rather_than_replace() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path();
        write(
            &cwd.join("imp.toml"),
            "[policy]\nnoninteractive = \"auto\"\n\n[[policy.allow]]\ntool = \"run_command\"\npattern = \"ls *\"\n",
        );

        let config = Config::load_with(None, None, cwd).unwrap();

        assert_eq!(config.policy.noninteractive, Decision::Auto);
        // Sibling key under the same table survives the merge.
        assert_eq!(config.policy.default, Decision::Ask);
        assert_eq!(config.policy.allow.len(), 1);
        assert_eq!(config.policy.allow[0].scope, "session");
    }

    #[test]
    fn the_read_only_command_table_parses_and_defaults_off() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path();
        write(
            &cwd.join("imp.toml"),
            "[policy.read_only]\nenabled = true\nextra = [\"fd\", \"delta\"]\n",
        );

        let config = Config::load_with(None, None, cwd).unwrap();

        assert!(config.policy.read_only.enabled);
        assert_eq!(config.policy.read_only.extra, vec!["fd", "delta"]);

        // Absent, it is off and empty.
        let dir = tempfile::tempdir().unwrap();
        let config = Config::load_with(None, None, dir.path()).unwrap();
        assert!(!config.policy.read_only.enabled);
        assert!(config.policy.read_only.extra.is_empty());
    }

    #[test]
    fn a_read_only_extra_entry_that_is_not_a_bare_verb_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path();
        write(
            &cwd.join("imp.toml"),
            "[policy.read_only]\nextra = [\"rm -rf\"]\n",
        );

        let err = Config::load_with(None, None, cwd).unwrap_err();

        assert!(matches!(err, Error::Config(_)), "unexpected error: {err}");
    }

    #[test]
    fn invalid_values_are_rejected_before_use() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path();
        write(&cwd.join("imp.toml"), "[agent]\nmax_iterations = 0\n");

        let err = Config::load_with(None, None, cwd).unwrap_err();

        assert!(matches!(err, Error::Config(_)), "unexpected error: {err}");
    }

    #[test]
    fn a_user_config_is_layered_under_the_project_file() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path();
        let user = cwd.join("user.toml");
        write(
            &user,
            "[provider]\nmodel = \"from-user\"\nbase_url = \"http://user/v1\"\n",
        );
        write(
            &cwd.join("imp.toml"),
            "[provider]\nmodel = \"from-project\"\n",
        );

        let config = Config::load_with(Some(&user), None, cwd).unwrap();

        // The project file wins on the key both set...
        assert_eq!(config.provider.model, "from-project");
        // ...and the user file still supplies the rest.
        assert_eq!(config.provider.base_url, "http://user/v1");
    }

    #[test]
    fn tests_never_read_the_developers_real_user_config() {
        // Regression guard: a test that passes `None` for the user path must not
        // pick up whatever `imp init` last wrote to the real config location.
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("imp.toml"), "[provider]\nmodel = \"m\"\n");

        let config = Config::load_with(None, None, dir.path()).unwrap();

        assert_eq!(config.provider.base_url, ProviderConfig::default().base_url);
    }

    #[test]
    fn unknown_keys_are_ignored_for_forward_compatibility() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path();
        write(&cwd.join("imp.toml"), "[future]\nsome_key = true\n");

        assert!(Config::load_with(None, None, cwd).is_ok());
    }

    #[test]
    fn credentials_default_to_a_separate_private_file() {
        let config = Config::default();

        assert!(!config.provider.api_key_file.is_empty());
        assert!(
            config.provider.api_key_file.ends_with("credentials"),
            "unexpected default: {}",
            config.provider.api_key_file
        );
    }

    #[test]
    fn the_credentials_file_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials");

        Credentials::write(&path, "sk-secret-value").unwrap();
        let loaded = Credentials::load(&path).unwrap();

        assert_eq!(loaded.api_key, "sk-secret-value");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(
                mode, 0o600,
                "credentials must not be group- or world-readable"
            );
        }
    }

    #[test]
    fn a_missing_credentials_file_is_empty_rather_than_an_error() {
        let dir = tempfile::tempdir().unwrap();

        let loaded = Credentials::load(&dir.path().join("absent")).unwrap();

        assert_eq!(loaded.api_key, "");
    }

    #[test]
    fn a_malformed_credentials_file_is_reported_as_a_credential_problem() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials");
        fs::write(&path, "api_key = ").unwrap();

        let err = Credentials::load(&path).unwrap_err();

        assert!(matches!(err, Error::Auth(_)), "unexpected error: {err}");
    }

    /// A config wired to one specific credentials file and env var.
    fn credential_config(dir: &Path, api_key_file: &str, api_key_env: &str) -> Config {
        let mut config = Config::default();
        config.provider.api_key_file = api_key_file.to_string();
        config.provider.api_key_env = api_key_env.to_string();
        config.provider.base_url = "http://localhost/v1".to_string();
        let _ = dir;
        config
    }

    #[test]
    fn the_stored_key_is_used_when_no_environment_variable_is_set() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials");
        Credentials::write(&path, "from-file").unwrap();
        // A name that is deliberately unset in any environment.
        let config = credential_config(dir.path(), path.to_str().unwrap(), "IMP_ABSENT_VAR");

        let key = config.api_key().unwrap();

        assert_eq!(key.as_deref(), Some("from-file"));
    }

    #[test]
    fn a_keyless_backend_declares_itself_by_emptying_both_sources() {
        let mut config = Config::default();
        config.provider.api_key_env = String::new();
        config.provider.api_key_file = String::new();

        assert!(config.api_key().unwrap().is_none());
    }

    #[test]
    fn a_configured_but_absent_credential_is_a_clear_error() {
        let dir = tempfile::tempdir().unwrap();
        let config = credential_config(dir.path(), "/nonexistent/credentials", "IMP_ABSENT_VAR");

        let err = config.api_key().unwrap_err();

        assert!(matches!(err, Error::Auth(_)), "unexpected error: {err}");
        let message = err.to_string();
        assert!(message.contains("IMP_ABSENT_VAR"), "message was: {message}");
        assert!(
            message.contains("imp init"),
            "message should say how to fix it: {message}"
        );
    }

    #[test]
    fn the_lean_flag_fills_the_small_model_defaults() {
        let mut config = Config::default();

        config.apply_lean_profile();

        assert!(config.agent.small_model);
        assert_eq!(config.agent.context_tokens, Some(8192));
        assert_eq!(config.agent.tool_result_chars, Some(6000));
        assert_eq!(config.agent.max_tokens, Some(1024));
        assert_eq!(config.agent.max_tool_calls_per_turn, 12);
        assert!(config.agent.nudge_on_empty);
        assert_eq!(config.provider.stream_idle_timeout_secs, 300);
        assert!(!config.provider.supports_usage_in_stream);
        assert!(config.provider.quirks.omit_parallel_tool_calls);
    }

    #[test]
    fn the_lean_flag_keeps_an_explicit_budget() {
        let mut config = Config::default();
        config.agent.context_tokens = Some(4096);
        config.agent.max_tokens = Some(256);

        config.apply_lean_profile();

        assert_eq!(
            config.agent.context_tokens,
            Some(4096),
            "an explicit key wins"
        );
        assert_eq!(config.agent.max_tokens, Some(256));
        // A key the user did not set still comes from the profile.
        assert_eq!(config.agent.tool_result_chars, Some(6000));
    }

    #[test]
    fn extra_body_may_not_shadow_a_core_request_key() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join("imp.toml"),
            "[provider.extra_body]\nmodel = \"hijacked\"\n",
        );

        let err = Config::load_with(None, None, dir.path()).unwrap_err();

        assert!(matches!(err, Error::Config(_)), "unexpected error: {err}");
        assert!(err.to_string().contains("model"), "was: {err}");
    }

    #[test]
    fn extra_body_carries_sampling_parameters() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join("imp.toml"),
            "[provider.extra_body]\ntop_p = 0.9\nrepeat_penalty = 1.05\n",
        );

        let config = Config::load_with(None, None, dir.path()).unwrap();

        assert_eq!(config.provider.extra_body["top_p"], serde_json::json!(0.9));
        assert_eq!(
            config.provider.extra_body["repeat_penalty"],
            serde_json::json!(1.05)
        );
    }

    #[test]
    fn a_zero_context_budget_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join("imp.toml"),
            "[agent]\ncontext_tokens = 0\n",
        );

        let err = Config::load_with(None, None, dir.path()).unwrap_err();

        assert!(matches!(err, Error::Config(_)), "unexpected error: {err}");
    }

    // --------------------------------------------------------- rename (D46)

    /// The credentials file is why the migration renames a whole directory
    /// instead of copying files: `rename` keeps its `0600` mode.
    #[cfg(unix)]
    #[test]
    fn migrating_a_directory_keeps_the_credentials_mode() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join("minion");
        let new = dir.path().join("imp");
        write(&old.join("config.toml"), "[provider]\nmodel = \"m\"\n");
        std::fs::write(old.join("credentials"), "api_key = \"sk-secret\"\n").unwrap();
        std::fs::set_permissions(
            old.join("credentials"),
            std::fs::Permissions::from_mode(0o600),
        )
        .unwrap();

        let notes = migrate_dir("config", &old, &new);

        assert_eq!(
            notes.len(),
            1,
            "one line for one moved directory: {notes:?}"
        );
        assert!(!old.exists(), "the old directory must be gone");
        assert_eq!(
            std::fs::read_to_string(new.join("credentials")).unwrap(),
            "api_key = \"sk-secret\"\n"
        );
        let mode = std::fs::metadata(new.join("credentials"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "a rename must not widen the credentials file");
    }

    /// Two directories that both exist are ambiguous, so nothing is moved and
    /// the notice says so: half-merging two databases is worse than leaving
    /// both alone.
    #[test]
    fn both_directories_present_moves_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join("minion");
        let new = dir.path().join("imp");
        write(&old.join("config.toml"), "a = 1\n");
        write(&new.join("config.toml"), "b = 2\n");

        let notes = migrate_dir("config", &old, &new);

        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(notes[0].contains("merge them by hand"), "{notes:?}");
        assert_eq!(
            std::fs::read_to_string(old.join("config.toml")).unwrap(),
            "a = 1\n"
        );
        assert_eq!(
            std::fs::read_to_string(new.join("config.toml")).unwrap(),
            "b = 2\n"
        );
    }

    /// A first run on a fresh machine has nothing to migrate and says nothing.
    #[test]
    fn a_fresh_machine_migrates_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let notes = migrate_dir(
            "config",
            &dir.path().join("minion"),
            &dir.path().join("imp"),
        );
        assert!(notes.is_empty(), "{notes:?}");
    }

    /// The database and its SQLite sidecars move together: renaming only the
    /// `.db` would leave a write-ahead log SQLite cannot pair with anything.
    #[test]
    fn the_database_and_its_sidecars_are_renamed_together() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["minion.db", "minion.db-wal", "minion.db-shm"] {
            write(&dir.path().join(name), "x");
        }
        write(&dir.path().join("unrelated.txt"), "x");

        let moved = migrate_db_files(dir.path()).unwrap();

        assert_eq!(moved, 3);
        for name in ["imp.db", "imp.db-wal", "imp.db-shm"] {
            assert!(dir.path().join(name).exists(), "{name} was not renamed");
        }
        assert!(!dir.path().join("minion.db").exists());
        assert!(dir.path().join("unrelated.txt").exists());
    }

    /// A database that is already in place is never overwritten.
    #[test]
    fn an_existing_database_is_never_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("minion.db"), "old");
        write(&dir.path().join("imp.db"), "new");

        let moved = migrate_db_files(dir.path()).unwrap();

        assert_eq!(moved, 0);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("imp.db")).unwrap(),
            "new"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("minion.db")).unwrap(),
            "old"
        );
    }

    /// A workspace that has not renamed its project file is still configured
    /// by it, rather than silently by the defaults (D46).
    #[test]
    fn a_legacy_project_file_is_still_read() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join("minion.toml"),
            "[provider]\nmodel = \"legacy-model\"\n",
        );

        let config = Config::load_with(None, None, dir.path()).unwrap();

        assert_eq!(config.provider.model, "legacy-model");
    }

    /// `imp.toml` wins when both names are present.
    #[test]
    fn the_new_project_file_wins_over_the_legacy_one() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join("minion.toml"),
            "[provider]\nmodel = \"legacy\"\n",
        );
        write(
            &dir.path().join("imp.toml"),
            "[provider]\nmodel = \"current\"\n",
        );

        let config = Config::load_with(None, None, dir.path()).unwrap();

        assert_eq!(config.provider.model, "current");
    }

    /// `[ui]` is cosmetic and deep-merged like everything else: a partial
    /// section keeps the other defaults, and an unknown key is ignored.
    #[test]
    fn the_ui_section_is_parsed_and_defaults_are_kept() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join("imp.toml"),
            "[ui]\nicons = \"ascii\"\nspinner = true\nsomething_new = 1\n",
        );

        let config = Config::load_with(None, None, dir.path()).unwrap();

        assert_eq!(config.ui.icons, IconChoice::Ascii);
        assert!(config.ui.spinner);
        assert_eq!(
            config.ui.color,
            ColorChoice::Auto,
            "an unset key keeps its default"
        );

        let default = Config::default();
        assert_eq!(default.ui.color, ColorChoice::Auto);
        assert_eq!(default.ui.icons, IconChoice::Auto);
        assert!(default.ui.spinner, "motion is on by default");
    }

    /// A bad enum value is a startup error, not a silent fallback.
    #[test]
    fn an_unknown_ui_colour_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("imp.toml"), "[ui]\ncolor = \"rainbow\"\n");

        assert!(Config::load_with(None, None, dir.path()).is_err());
    }
}
