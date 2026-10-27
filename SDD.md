# Software Design Document — `imp`

**A minimal, Unix-native AI agent harness with a shell-like REPL, an OpenAI-compatible model client, and an MCP server.**

| Field | Value |
|---|---|
| Project | `imp` |
| Document | Software Design Document (SDD) |
| Version | 0.1 (Draft) |
| Status | For review |
| Date | 2026-09-29 |
| Language | Rust (edition 2024), `tokio` async runtime |
| Primary artifact | Single static binary `imp` |
| Platforms | macOS, Linux (primary); Windows best-effort |

### Glossary

| Term | Meaning |
|---|---|
| Turn | One user request → model calls → tool executions → final answer |
| Tool | A capability the model can invoke (JSON-schema described) |
| Job | A cron-scheduled prompt owned by the in-process scheduler |
| Approval | Interactive gate before a side-effecting action executes |
| MCP | Model Context Protocol (server = we expose, client = we consume) |
| Workspace | The directory tree the agent is permitted to touch |

---

## 1. Overview

### 1.1 Problem

Most AI agent harnesses are heavy: a full-screen TUI, an IDE-like pane layout, a daemon with a web UI, and a large dependency surface. That makes them awkward to embed, script, pipe, and audit. `fx` (Vercel Labs, Zig) demonstrated the opposite: an agent that behaves like a **Unix shell** — preserves scrollback, emits minimal output, and composes with pipes and scripts.

`imp` applies that philosophy to a general-purpose (not coding-specific) mini agent that:

1. Talks to any **OpenAI-compatible** `/v1/chat/completions` endpoint.
2. Ships a **small, audited tool set** — command execution, file access, HTTP, cron, memory.
3. Presents an **fx-like line REPL** (no alternate screen).
4. Runs an **in-process cron scheduler**.
5. Is **both an MCP server and an MCP client**, so other models can drive it and it can drive others.

### 1.2 Goals

| ID | Goal |
|---|---|
| G1 | One small static binary; no daemon required for interactive use |
| G2 | Provider-agnostic via the OpenAI Chat Completions API |
| G3 | Line-oriented REPL that preserves scrollback and composes with pipes |
| G4 | Side effects are gated: nothing dangerous runs without policy consent |
| G5 | Durable sessions, jobs, and memory in a single SQLite file |
| G6 | Expose the agent as an MCP server; consume external MCP servers as tools |
| G7 | Every component independently testable without network access |

### 1.3 Non-goals (v1)

- No subagents / recursive task spawning.
- No embeddings or vector search (memory is keyword/FTS based).
- No full-screen TUI panes.
- No browser automation or GUI control.
- No multi-user server, auth, or tenancy.
- No own model serving (we are a client, not an inference server).

### 1.4 Design principles

1. **Unix-native.** stdout is data, stderr is diagnostics. Respect `NO_COLOR`, `PIPE`, `--json`.
2. **Fail closed.** Unknown tool → denied. Non-TTY → no implicit `--yes`.
3. **Explicit over magic.** No hidden retries on side-effecting tools. No silent network calls.
4. **One source of truth.** SQLite owns state; config files own policy; nothing else persists.
5. **Small surface.** Six tool families. Each has a schema, a policy class, and a test.

---

## 2. Requirements

### 2.1 Functional requirements

**Model / provider**

| ID | Requirement |
|---|---|
| FR-1 | Call any OpenAI-compatible `/v1/chat/completions` with configurable `base_url` and API key env var |
| FR-2 | Stream responses (SSE) and render text incrementally |
| FR-3 | Support OpenAI tool calling: send `tools[]`, parse `tool_calls`, return `role:"tool"` results |
| FR-4 | Retry transient failures (429, 5xx) with exponential backoff + jitter, honoring `Retry-After` |
| FR-5 | Track token usage per turn from the `usage` field; degrade gracefully when absent |
| FR-6 | Support provider quirks via config flags (disable parallel tool calls, disable usage streaming) |

**Agent loop**

| ID | Requirement |
|---|---|
| FR-7 | Iterate model ⇄ tools until a final text answer, iteration cap, or budget cap |
| FR-8 | Execute multiple tool calls from one assistant message, bounded concurrency |
| FR-9 | Halt with a machine-readable reason (`completed`, `iteration_limit`, `token_budget`, `cancelled`, `error`) |
| FR-10 | Cancel an in-flight turn on Ctrl-C, killing child processes |

**Tools**

| ID | Requirement |
|---|---|
| FR-11 | `run_command`, `cron_add`/`cron_list`/`cron_remove`, `read_file`/`write_file`/`edit_file`, `http_fetch`, `mcp_call`, `remember`/`recall` |
| FR-12 | Every tool declares a JSON schema, a risk class, a timeout, and an output cap |
| FR-13 | Tool errors are returned to the model as structured results, not panics |

**Approval / policy**

| ID | Requirement |
|---|---|
| FR-14 | Classify each invocation as `auto`, `ask`, or `deny` from config + static risk rules |
| FR-15 | Interactive approval prompt with one-shot, session, and persistent allowlist options |
| FR-16 | Persistent allowlist entries are scoped (tool + pattern + workspace) and revocable |
| FR-17 | Non-interactive mode never prompts: config decides, default `deny` for side effects |
| FR-43 | Optional System One guard (`POST {base}/v1/systemone`) may resolve a flagged `run_command` prompt when the category is eligible and the model is confident; it may never widen what the static rules allow |
| FR-44 | Guard eligibility is a floor: `privilege`, `remote-execution` and `destructive` never reach the model and always prompt |
| FR-45 | Every guard failure — network, timeout, rate limit, unparseable body, withdrawn model — falls back to prompting, never to allowing |
| FR-46 | Only the command string is sent as System One `state`; the transcript, tool output and file contents are never included |
| FR-47 | Two thresholds with a human in the middle: a verdict above `deny_threshold` or between the thresholds prompts; only at or below `allow_threshold` resolves silently, and every verdict is audited |

**Cron**

| ID | Requirement |
|---|---|
| FR-18 | Create/list/remove jobs with 5-field cron expressions and IANA timezone |
| FR-19 | Fire jobs while the agent runs; persist before acknowledgment |
| FR-20 | Catch up missed runs on startup according to a per-job policy (`skip`, `run_once`, `run_all`) |
| FR-21 | Record each run with status, duration, and output reference |

**MCP**

| ID | Requirement |
|---|---|
| FR-22 | Serve MCP over stdio: tools, resources, and a prompt for "ask the agent" |
| FR-23 | Consume external MCP servers (stdio) declared in config; expose their tools as `mcp__<server>__<tool>` |
| FR-24 | Deny `run_command` exposure over MCP unless explicitly enabled |
| FR-25 | Per-server allowlists and per-server approval policy |

**Interface**

| ID | Requirement |
|---|---|
| FR-26 | REPL default; one-shot `imp run "<prompt>"`; stdin piping |
| FR-27 | Slash commands for local control (`/help`, `/model`, `/sessions`, `/cron`, `/allow`, `/quit`) |
| FR-28 | `!<cmd>` shell escape that bypasses the model |
| FR-29 | `--json` machine-readable output for one-shot and management subcommands |
| FR-30 | Session management: list, show, resume, delete |
| FR-31 | `imp init` configures the model backend (base URL, API key env var, model) and writes a config file |
| FR-32 | The config file records only the API key env var *name*; the key value is never written to a config, so a config may be committed |
| FR-33 | `init` discovers models via `GET {base_url}/models` when reachable, and falls back to free-text entry when it is not |
| FR-34 | `init` is idempotent and non-destructive: an existing config is diffed and requires `--force` to replace, and writes are atomic |
| FR-35 | `init --check` probes the backend for reachability and auth before writing, and writes nothing when the probe fails |
| FR-36 | Interactive `init` asks for the real API key with terminal echo disabled **before** the prompt is printed, and stores it in a `0600` credentials file; `--no-store-token` opts out |
| FR-37 | Credential resolution order is `provider.api_key_env` (if set) → `provider.api_key_file` → error, so one shell or CI job can override the stored secret without editing files |
| FR-38 | Arbitrary extra headers for the provider are configurable under `[provider.headers]`, and `init --header` records them |
| FR-39 | A header value may reference `${session}`, expanded to the stable per-conversation id so gateways can pin routing and reuse prompt caches |
| FR-40 | An empty `provider.api_key_env` *and* an empty `provider.api_key_file` means the backend needs no credentials, and no `Authorization` header is sent |
| FR-41 | `apply_patch` applies a set of anchored operations across one or more files atomically, failing the whole patch if any operation is ambiguous or absent |
| FR-42 | A file-writing or command-executing tool never runs without a resolved decision; a denied or unattributable call returns an error to the model instead of executing |

### 2.2 Non-functional requirements

| ID | Requirement | Target |
|---|---|---|
| NFR-1 | Cold start to prompt | < 60 ms |
| NFR-2 | Idle RSS | < 40 MB |
| NFR-3 | Release binary size (stripped, musl) | < 15 MB |
| NFR-4 | Tool output captured to memory capped by default | 256 KiB (configurable) |
| NFR-5 | Default command timeout | 120 s |
| NFR-6 | Crash safety: no lost job or message on `SIGKILL` | SQLite WAL + write-before-ack |
| NFR-7 | Offline unit/integration test suite | No network required |
| NFR-8 | Accessibility | No color dependence; `NO_COLOR` honored; screen-reader-safe line output. Markdown rendering is additive — it reflows plain text and adds table borders, and no meaning is carried by colour alone |
| NFR-9 | Log redaction | API keys and env values never logged |
| NFR-10 | `init` performs no network I/O unless `--check` is passed | Setup works offline (local backends, air-gapped machines) |

---

## 3. Architecture

### 3.1 High-level view

```
┌────────────┐        ┌──────────────────────── imp (one binary) ─────────────────────┐
│  Terminal  │        │                                                                  │
│ TTY / pipe │◄──────►│  ┌──────────┐        ┌──────────────┐      ┌────────────────┐     │
└────────────┘        │  │  REPL /  │        │  Agent Loop  │      │  Provider      │─────┼──► OpenAI-compatible
                      │  │  Frontend│◄──────►│ (turn state  │─────►│  Client        │     │    /chat/completions
                      │  │ rustyline│        │  machine)    │◄─────│  (SSE + tools) │     │
                      │  └────┬─────┘        └──────┬───────┘      └────────────────┘     │
                      │       │                     │                                     │
                      │       │ approval            │ dispatch                            │
                      │       ▼                     ▼                                     │
                      │  ┌──────────┐        ┌──────────────┐      ┌────────────────┐     │
                      │  │ Approval │        │  Tool        │─────►│  MCP Client    │─────┼──► external MCP
                      │  │ Engine   │        │  Registry    │      │  (stdio)       │     │    servers
                      │  └────┬─────┘        └──────┬───────┘      └────────────────┘     │
                      │       │                     │                                     │
                      │       ▼                     ▼                                     │
                      │  ┌──────────┐        ┌──────────────┐                              │
                      │  │ Policy   │        │  SQLite      │                              │
                      │  │ Store    │───────►│  Store (WAL) │                              │
                      │  └──────────┘        └──────┬───────┘                              │
                      │                              ▲                                      │
                      │  ┌──────────────┐            │                                      │
                      │  │  Cron        │────────────┘                                      │
                      │  │  Scheduler   │                                                   │
                      │  └──────┬───────┘                                                   │
                      │         │ inject prompt                                               │
                      │         └──────────────► Agent Loop                                   │
                      │                                                                      │
                      │  ┌────────────────────────────────────────┐                          │
                      │  │  MCP Server (stdio)                    │◄──── other models / hosts│
                      │  │  tools: agent_ask, agent_*, cron_*, …  │                          │
                      │  └────────────────────────────────────────┘                          │
                      └──────────────────────────────────────────────────────────────────────┘
```

### 3.2 Component responsibilities

| Component | Responsibility | Must not |
|---|---|---|
| Frontend (REPL/TUI) | Read input, render stream, prompt for approval | Contain policy logic |
| Agent Loop | Turn state machine, message assembly, tool dispatch | Execute commands itself |
| Provider Client | HTTP + SSE to the model, retries, usage accounting | Know about tools' semantics |
| Tool Registry | Schema, validation, timeouts, output caps, risk class | Decide approval outcomes |
| Approval Engine | Resolve `auto`/`ask`/`deny`, prompt, persist allowlist | Execute tools |
| Policy Store | Persist allowlist, deny rules, workspace roots | Interpret commands |
| Cron Scheduler | Compute due jobs, dispatch prompts, record runs | Mutate job definitions |
| SQLite Store | Sole persistence: sessions, messages, jobs, runs, memory, policy | Hold secrets in plaintext |
| MCP Client | Discover/call external MCP tools | Bypass approval |
| MCP Server | Expose imp capabilities to other models | Expose `run_command` by default |

### 3.3 Execution model

- **One `tokio` runtime.** A multi-threaded runtime with a modest worker count (`min(4, num_cpus)`).
- **Frontend** runs on the main task; it owns terminal I/O exclusively (single writer).
- **Agent loop** runs as a spawned task per turn. Ctrl-C sends cancellation via a `CancellationToken`.
- **Tool execution** uses a `JoinSet` with a semaphore (default concurrency 4). Each tool runs in its own task so a hung tool cannot block the loop.
- **Cron scheduler** is a long-lived task ticking every second, comparing `next_run_at <= now` against the store.
- **MCP server** is a separate task over stdio; it must not interleave with REPL output — when serving MCP, the REPL is disabled.
- **Channels:** `mpsc` for events frontend ← loop, `oneshot` per approval request, `broadcast` for cancellation and shutdown.

### 3.4 A turn, end to end

```
1. Input arrives (REPL line, `run` arg, stdin, cron fire, or MCP agent_ask)
2. Persist user message
3. Assemble request: system prompt + policy digest + tool schemas + history window
4. POST /chat/completions (stream)
5. Stream deltas → render content; accumulate tool_calls
6. If finish_reason != tool_calls → persist assistant, emit final, done
7. Else: persist assistant message with tool_calls
8. For each tool call (bounded concurrency):
     a. Registry validates args against schema      → invalid: tool error result
     b. Approval engine resolves policy             → ask: prompt; deny: tool error result
     c. Execute with timeout + output cap + cancellation
     d. Persist tool result
9. Inject tool results as role:"tool"; loop to 4
10. Stop on completed / iteration cap / budget / cancel / error
```

---

## 4. Module layout

Proposed crates (single workspace, one binary + library for testability):

```
imp/
├── Cargo.toml                # workspace
├── crates/
│   ├── imp-core/          # lib: loop, messages, tool traits, policy  (no I/O deps on terminal)
│   ├── imp-provider/      # OpenAI-compatible client (SSE, retries, usage)
│   ├── imp-tools/         # built-in tools + registry
│   ├── imp-store/         # SQLite schema, migrations, repositories
│   ├── imp-cron/          # scheduler + catch-up policy
│   ├── imp-mcp/           # MCP server + client (rmcp)
│   └── imp-cli/           # bin: clap, REPL frontend, subcommands
└── tests/                    # cross-crate integration + fixtures
```

Rationale: `imp-core` has zero terminal and zero network dependencies, so the loop is unit-testable with a scripted mock provider.

### 4.1 Dependencies (indicative)

| Concern | Crate |
|---|---|
| Async runtime | `tokio` (rt-multi-thread, process, signal, sync, time) |
| CLI | `clap` (derive) |
| Serialization | `serde`, `serde_json`, `toml` |
| Schemas | `schemars` (derive tool JSON schemas from Rust types) |
| HTTP + SSE | `reqwest` (rustls), `eventsource-stream` or manual SSE line parser |
| SQLite | `rusqlite` (`bundled`) + a small pool |
| MCP | `rmcp` (stdio server + client) |
| REPL | `rustyline` (history, line editing, no alternate screen) |
| Cron | `cron` (parse) + custom tick loop |
| Time | `chrono`, `chrono-tz` |
| Paths | `directories` |
| Globbing | `globset`, `ignore` |
| Diffing | `similar` |
| Errors | `thiserror` (libraries), `anyhow` (binary) |
| Logging | `tracing`, `tracing-subscriber` |
| IDs | `uuid` (v7) |
| Testing | `insta`, `wiremock`, `assert_cmd`, `proptest`, `tempfile` |

---

## 5. Detailed design

### 5.1 Configuration

**Precedence** (highest first): CLI flags → env vars (`IMP_*`) → project `imp.toml` → synced master layer (`config sync`, §5.14) → user config → built-in defaults.

Locations: `$XDG_CONFIG_HOME/imp/config.toml` (or `~/Library/Application Support/imp/` on macOS), plus `./imp.toml` in the workspace, plus the machine-local `synced.toml` written by `config sync` (§5.14).

```toml
[provider]
base_url = "https://api.openai.com/v1"
api_key_env = "OPENAI_API_KEY"   # name only; the value is never stored or logged
api_key_file = "~/.config/imp/credentials"  # the value itself lives here, 0600
model = "gpt-4.1-mini"
temperature = 0.2
stream = true
request_timeout_secs = 120
max_retries = 3
# provider quirks
supports_usage_in_stream = true
parallel_tool_calls = true
strict_tool_arguments = false   # ask the backend to constrain tool args to the schema (M10.3)
context_window = 128000         # explicit window; unset probes the endpoint (D49)
context_probe = "auto"          # auto | off

# Extra headers sent on every request. A value may contain `${session}`, which
# expands to the stable identifier of the current conversation. Gateways that
# pin a conversation to one upstream need this; OpenCode Go rejects requests
# without it.
[provider.headers]
"x-opencode-session" = "${session}"

[agent]
system_prompt_file = "~/.config/imp/system.md"
max_iterations = 25
max_tool_calls_per_turn = 0    # 0 = unlimited; a small model benefits from a small number (M10.3)
small_model = false            # append short numbered rules for a 2-4B local model (M10.3)
history_window = 40            # messages sent when a transcript is reloaded
context_tokens = 8192          # budget for one request; unset derives it (D49)
tool_result_chars = 6000       # cap on any one tool result; unset leaves it uncapped
compact = true                 # summarize old turns instead of dropping them (D50)
compact_threshold = 0.8        # fraction of the budget that triggers it
compact_keep_turns = 2         # recent turns kept verbatim
compact_model = ""             # empty: the session's own model

# Which tools the model is offered (M10.3). Narrowing only: it changes what the
# model sees, never what the gate decides. A hidden tool is also unresolvable,
# so `hide` can never turn a denied call into an allowed one. `hide` beats
# `only` when a name appears in both.
[tools]
only = []                      # non-empty: offer only these names
hide = ["http_fetch"]          # always removed, even if listed in `only`

[workspace]
roots = ["."]                  # path guard boundary; canonicalized at load
read_only_roots = []
max_file_bytes = 2_097_152
follow_symlinks = false

[exec]
default_timeout_secs = 120
max_timeout_secs = 3600
output_cap_bytes = 262_144
shell = "/bin/sh"
login_shell = false

[policy]
default = "ask"                # auto | ask | deny
noninteractive = "deny"        # policy when stdin is not a TTY
allow = [
  { tool = "run_command", pattern = "ls *",   scope = "session" },
  { tool = "run_command", pattern = "git status", scope = "always" },
]
deny = [
  { tool = "run_command", pattern = "*sudo*" },
  { tool = "run_command", pattern = "*rm -rf /*" },
]

[http_fetch]
allowed_domains = ["docs.rs", "*.github.com"]
block_private_ips = true
max_bytes = 1_048_576
timeout_secs = 20

# Optional System One judge for a flagged `run_command` (M3.5, D16). Off by
# default: it is a network call carrying command text to a third party.
[guard]
enabled = false
base_url = "http://127.0.0.1:8081"   # server root; the request goes to {base_url}/v1/systemone
allow_threshold = 0.25               # at or below this, a verdict resolves the prompt
deny_threshold = 0.75                # above this, a confident refusal; the band between prompts too
timeout_secs = 10

[cron]
enabled = true
timezone = "America/Mexico_City"
missed_run_policy = "run_once"   # skip | run_once | run_all
max_concurrent_jobs = 2
missed_run_cap = 20              # ceiling on the runs one run_all catch-up may start

[mcp.server]
enabled = true
transport = "stdio"              # stdio | http
bind = "127.0.0.1:8788"          # only for transport = "http"; loopback by default
token_env = ""                   # for "http": the NAME of the var holding the bearer token
token_file = ""                  # ...or the PATH to the 0600 credentials file holding it
expose_exec = false              # run_command over MCP defaults OFF
expose_write = false
expose_cron_write = true

# A server is reached one of two ways, and they are mutually exclusive: a
# `command` imp spawns over stdio, or a `url` it speaks Streamable HTTP to.
[mcp.client.servers.filesystem]
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "/data"]
approval = "ask"
tool_allow = ["read_file", "list_directory"]

[mcp.client.servers.peer]
url = "http://peer.tailnet.ts.net:8788/mcp"
token_file = "~/.config/imp/peer.credentials"
approval = "ask"
tool_allow = ["*"]

[logging]
level = "info"
format = "text"                  # text | json
file = "~/.local/state/imp/imp.log"
redact_env = true

# The master config channel (M11, §5.14). This section is read from the *local*
# layers only — it is the bootstrap that tells a node where the master repo is,
# so the synced layer itself may never define it. `remote`/`ref` name the git
# channel; `node` names *this* machine and is resolved when it is empty.
[configsync]
remote = "https://git.albruiz.dev/albruiz/imp-config.git"
ref = "main"                     # branch or tag; a pinned commit is also accepted
node = ""                        # empty → `hostname -s` → the self entry of `tailscale status`
```

**Secrets.** The config records only the *names* of where a secret comes from. The value is read at
run time, held in a `SecretString`-like wrapper that has no `Debug`/`Display`, and never written to
logs, SQLite, or a config file. `init` stores the value it collects in a separate `0600`
credentials file, so a config may be committed while the secret never is.

**Credential resolution.** `Config::api_key()` is tried in order:

1. `provider.api_key_env` — if the named variable is set and non-empty, it wins. This is what lets a
   single shell or CI job override a stored key without editing anything.
2. `provider.api_key_file` — a TOML file holding `api_key = "…"`, written by `imp init`.
3. Otherwise an error naming both sources, *unless both are empty*, which is how a keyless local
   backend is expressed.

Both keys are strings rather than optionals, because "unset" and "empty" must mean the same thing:
an empty value must override the built-in default rather than fall back to it. A keyless backend
therefore has to set **both** to `""` — clearing only `api_key_env` would leave the default
credentials path active and the backend would still appear to need a key.


#### 5.1.1 `imp init` — configuring the model backend

`init` is the supported way to produce a first config. It is an opinionated wizard over the schema
above: it asks only for what the provider layer actually needs, then writes one file.

**Question flow** (interactive, on a TTY):

1. **Backend preset**:

   | Preset | `base_url` | `api_key_env` | Notes |
   |---|---|---|---|
   | OpenAI | `https://api.openai.com/v1` | `OPENAI_API_KEY` | default |
   | OpenRouter | `https://openrouter.ai/api/v1` | `OPENROUTER_API_KEY` | |
   | OpenCode Go | `https://opencode.ai/zen/go/v1` | `OPENCODE_API_KEY` | sets `x-opencode-session = "${session}"` |
   | OpenCode Zen | `https://opencode.ai/zen/v1` | `OPENCODE_API_KEY` | same session header |
   | Ollama (local) | `http://localhost:11434/v1` | *none* | no key required |
   | vLLM / LM Studio (local) | `http://localhost:8000/v1` | *none* | |
   | Custom | free text | free text or none | |

2. **Base URL** — prefilled from the preset, editable.
3. **API key environment variable** — the *name* to record, prefilled and editable. Local presets may
   answer *none* (flag: `--no-api-key`), which clears the name. The value is never requested here.
4. **API key** — the real token, asked for once, on a TTY. Echo is disabled *before* the prompt is
   printed, so even a paste cannot put it into terminal scrollback. Pressing Enter skips it and falls
   back to the environment variable. `--no-store-token` keeps the value in memory for discovery and
   `--check` without persisting it.
5. **Model** — if `GET {base_url}/models` succeeds the results are offered as a picker; otherwise
   the user types an id. Discovery uses the token just typed, so setup works before anything has been
   exported. A failed listing is a warning, never an error.
6. **Workspace root** — default `.`, written into `workspace.roots`.
7. **Confirmation** showing the target path and a summary of the values about to be written.

**Where the key is stored.** The token goes to a `0600` credentials file beside the user config
(`$XDG_CONFIG_HOME/imp/credentials`), created inside a `0700` directory. The config is written
*without* the value, so `--project` output stays committable. `--credentials-file` chooses a
different location, and that path is then recorded as `provider.api_key_file` so the two cannot
disagree. In the default case the path is left unstated, because a machine-specific path in a shared
config would be noise at best and misleading at worst.

**Extra headers.** `--header "Name: value"` may be repeated and is recorded under
`[provider.headers]`; a header supplied this way overrides the preset's value of the same name.
This is how a backend whose gateway requires a routing header — OpenCode Go's
`x-opencode-session` — is configured without a bespoke code path:

```
imp init --non-interactive --preset custom \
  --base-url https://gateway.internal/v1 --model internal-1 \
  --api-key-env GATEWAY_KEY --header 'x-opencode-session: ${session}'
```

**Target file.** The user config path from §5.1 by default; `--project` writes `./imp.toml`
instead (for a shared, secret-free setup that can be committed). Parent directories are created
`0700` and the file is created `0600`. Writes are atomic (temp file in the same directory, then
rename), so an interrupted `init` never leaves a partial config.

**Refusing to clobber.** If the target already exists, `init` prints a diff of the keys it would
change and exits `4` without writing, unless `--force` is passed.

**Non-interactive form.** Every prompt has a flag, so `init` is usable from provisioning scripts:

```
imp init --non-interactive \
  --base-url http://localhost:11434/v1 \
  --model llama3.1 --no-api-key --project --force
```

**Validation.** `init --check` probes `GET {base_url}/models` (falling back to a single-token
completion when the endpoint does not implement `/models`) *before* writing. On failure it writes
nothing and exits `3` with the provider's own message, so a misconfigured backend never produces a
config that merely looks correct.

**Credential warning.** If a credential is named but nothing can be found — the env var is unset and
no token was just stored — the config is still written, but the gap is printed, and `imp doctor`
repeats it. Storing a token *is* a credential, so a key saved moments earlier must not trigger this
warning.

**`--json`.** Emits `{"type":"init","path":…,"credentials":…,"provider":{…},"check":"ok|skipped|failed"}`
so scripted setups can consume the result. It reports the credentials *path*; the value is never in
the output.

**Relationship to `config`.** `imp config init` is retained as an alias for `imp init`;
`imp config show|path` stay read-only.

### 5.2 Provider client

A thin, hand-rolled client rather than a heavy SDK, for control over base URLs and provider quirks.

```rust
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<Message>,
    pub tools: Option<Vec<ToolSchema>>,
    pub tool_choice: ToolChoice,          // Auto | None | Required | {name}
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
    pub stream: bool,
    pub parallel_tool_calls: Option<bool>,
    pub stream_options: Option<StreamOptions>, // include_usage
}

pub enum ChatEvent {
    TextDelta(String),
    ToolCallDelta { index: usize, id: Option<String>, name: Option<String>, args_fragment: String },
    Usage(Usage),
    Done { finish_reason: FinishReason },
}

pub trait Provider: Send + Sync {
    fn stream(&self, req: ChatRequest, cancel: CancellationToken)
        -> impl Stream<Item = Result<ChatEvent>>;
}
```

**Behaviors**

| Aspect | Decision |
|---|---|
| Transport | `POST {base_url}/chat/completions`, `Accept: text/event-stream` |
| SSE parsing | Line-buffered; ignore comment/keepalive lines; tolerate missing `[DONE]` |
| Tool-call assembly | Accumulate fragments by `index`; concatenate `arguments` strings before JSON parse |
| Malformed args | Return a `tool` message with `{"error":"invalid_json"}` so the model can self-correct |
| Retries | Only before any delta is emitted; never mid-stream, never for non-idempotent tools |
| Backoff | `min(2^attempt * 250ms, 8s)` + jitter; honor `Retry-After` |
| Errors | Map HTTP/JSON failures to typed errors: `Auth`, `RateLimit{retry_after}`, `BadRequest{body}`, `Transport`, `Provider{status,body}` |
| Timeouts | Connect 10 s, idle-stream 60 s (resets per delta), total `request_timeout_secs` |
| Custom headers | `[provider.headers]` is applied to every request; `${session}` expands to the conversation id |
| Authentication | `Authorization: Bearer` is sent only when a credential is resolved; `api_key_env` first, then `api_key_file`. Both empty means a keyless backend |
| Model discovery | `GET {base_url}/models` returns the advertised ids, used by `init` for the picker and by `--check` |

#### 5.2.1 Conversation identity and `${session}`

Some gateways need to recognise that two requests belong to the same conversation, so they can pin
it to one upstream and reuse its prompt cache. OpenCode Go is the concrete case: it requires an
`x-opencode-session` header on every request and rejects calls that omit it.

Rather than hard-coding a provider-specific header, the client exposes one generic mechanism:

1. Every session gets an identifier from `new_session_id()` — a v7 UUID, so it is unique and also
   sorts by creation time.
2. The id is stable for the whole conversation: the REPL keeps one for its lifetime, `run` uses one
   per invocation, and M1 persists it as `sessions.id`.
3. Configured header values containing `${session}` are expanded with it at request time.

A header whose value is empty *after* expansion is dropped rather than sent blank, so a config that
references `${session}` still works for a one-off call that has no session.

This keeps two properties: the mechanism is provider-agnostic (any header can use the placeholder),
and the identifier is never invented per request — it cannot be, or prompt-cache routing would break.

### 5.3 Agent loop

```rust
pub enum StopReason { Completed, IterationLimit, ToolBudget, TokenBudget, Cancelled, ProviderError }

pub struct TurnOutcome {
    pub final_text: Option<String>,
    pub stop: StopReason,
    pub usage: Usage,
    pub iterations: u32,
}
```

Rules:

- The system prompt is assembled once per turn: static persona + a **policy digest** (workspace roots, non-interactive status, available tools, notable denies) so the model knows what it may do.
- History is trimmed to `history_window`; if `summarize_on_truncate`, older turns are replaced by a generated summary message flagged `[summary]`.
- Tool results are `role:"tool"` messages with `tool_call_id`; every assistant `tool_calls` entry must get exactly one matching result, or the request is invalid and the loop repairs it with a synthetic error result.
- Token budget is checked after each provider response; exceeding it stops with `TokenBudget`.
- Cancellation is cooperative: the `CancellationToken` is checked between iterations and passed into every tool.

**Ergonomics for a small local model (M10.3).** A 2–4B model is a poor tool-caller, and four
loop-level rules exist so its failures do not become the session's (D32–D37):

- **A truncated tool call is discarded, not parsed.** When a turn ends with `finish_reason: length`
  *and* carries at least one tool call, the arguments are half a JSON object. The whole batch is
  dropped — never parsed, never run — an assistant message with any text is kept, a `system` notice
  asks for a shorter answer, and the loop asks again. The turn ends as a normal conversation, not as
  a parse error.
- **Repeated calls are answered once.** Within one turn, a call whose `name` and *canonical*
  arguments repeat an earlier one is answered from the first result instead of re-running it, and the
  repeat is marked as such on the event stream. Any successful call that can change the machine
  (`Risk::is_observation() == false`) clears that cache first, so a read is never answered from
  before a write.
- **The tool budget bounds the turn.** `agent.max_tool_calls_per_turn` (0 = unlimited) counts every
  call the model asks for, repeats included. On exhaustion, the remaining calls in the batch are
  still *answered* — a `tool` message with a budget error, so no `tool_calls` id is left dangling —
  a `system` notice explains why, and the turn stops with `StopReason::ToolBudget`, which is distinct
  from `IterationLimit`.
- **The advertised surface is trimmable.** `[tools] only`/`hide` narrow the catalogue the model is
  shown, and the prompt's tool digest follows it. Hiding is applied *after* shadowing resolution, so
  a hidden built-in cannot be replaced by a same-named MCP tool.

Every one of these is inert by default: a config that does not set the new keys behaves exactly as
before.

### 5.4 Tool framework

```rust
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &'static str;
    fn description(&self) -> &'static str;
    fn schema(&self) -> serde_json::Value;          // from schemars
    fn risk(&self) -> Risk;                          // ReadOnly | Write | Execute | Network
    fn default_timeout(&self) -> Duration;
    async fn invoke(&self, ctx: ToolCtx, args: serde_json::Value) -> Result<ToolOutput>;
}

pub struct ToolOutput {
    pub content: String,       // fed to the model
    pub truncated: bool,
    pub metadata: serde_json::Value, // e.g. exit code, bytes, duration
}

pub struct ToolCtx {
    pub workspace: WorkspaceGuard,
    pub store: Arc<Store>,
    pub cancel: CancellationToken,
    pub session_id: Uuid,
    pub progress: mpsc::Sender<Progress>,
}
```

Invariants enforced by the registry, not by individual tools:

- Args are validated against `schema()` before `invoke`; failures produce a tool-level error result.
- Output is truncated to `output_cap_bytes` with a `truncated` flag and a hint appended.
- Every invocation is wrapped in `tokio::time::timeout` and tied to the cancellation token.
- Every invocation emits a `tracing` span and an audit row.
- **A name is offered and resolvable, or it is neither.** `[tools] only`/`hide` (M10.3) select a
  subset for the model; a name the selection rejects is dropped from `schemas()` *and* from `get()`,
  so a call to it is answered exactly like a misspelled one. Selection can only remove entries, so
  it never grants a permission the approval engine would refuse — that boundary stays where D14/D15
  put it.

### 5.5 Tool specifications

#### `run_command`

| Field | Spec |
|---|---|
| Risk | Execute |
| Params | `command: string` (required), `cwd?: string`, `timeout_ms?: int`, `stdin?: string` |
| Execution | `<shell> -c <command>` (or `-lc` if `login_shell`); new process group so the whole tree can be killed |
| Returns | `{ exit_code, stdout, stderr, duration_ms, truncated }` |
| Caps | `output_cap_bytes` per stream; `max_timeout_secs` clamps whatever the model asks for |
| Kill | On timeout or cancel: `SIGTERM` to the process group, then `SIGKILL` after 5 s |
| Env | Not settable by the model. A free-form `env` map is a way to smuggle values into a child; a fixed allowlist can come later if something needs it. |
| Approval | At least `ask` unless an allowlist pattern matches. Destructive, privileged, and network-pipe commands stay at `ask` even when `policy.default = "auto"`. A recognised read-only command may run without approval when `[policy.read_only].enabled` is set — see D48. |
| Notes | Streaming output options: `--stream` shows live output to stderr without entering model context |

#### `read_file` / `write_file` / `edit_file`

| Field | Spec |
|---|---|
| Risk | ReadOnly / Write / Write |
| Path guard | Canonicalize; must be inside a `workspace.roots` entry; reject symlink escape when `follow_symlinks = false`; reject non-UTF-8 by default |
| `read_file` | Params `path`, `offset?`, `limit?`; returns numbered lines; caps at `max_file_bytes` |
| `write_file` | Params `path`, `content`, `create_dirs?: bool`, `overwrite?: bool` (default false) |
| `edit_file` | Params `path`, `old_string`, `new_string`, `replace_all?: bool`; exact-match, must be unique unless `replace_all`; returns a unified diff |
| Atomicity | Write to temp file in the same directory, `fsync`, then `rename` |
| Approval | Writes `ask` by default; in-workspace writes may be allowlisted |

#### `apply_patch`

`edit_file` asks the model to reproduce one small snippet verbatim. For a change spread over several
sites, or across several files, that is both verbose and the most common way a model goes wrong. So
`apply_patch` exists alongside it: a set of *anchored operations* applied in one atomic call.

The format is structured JSON rather than a textual diff, so every operation is self-describing and
failures are exact instead of "patch did not apply":

```json
{
  "files": [
    {
      "path": "src/store.rs",
      "operations": [
        { "op": "replace",       "old": "let x = 1;", "new": "let x = 2;" },
        { "op": "insert_before", "anchor": "fn main() {", "new": "    setup()?;\n" },
        { "op": "insert_after",  "anchor": "use std::io;", "new": "use std::path::Path;" },
        { "op": "delete",        "old": "    // TODO: remove\n" }
      ]
    },
    { "path": "tests/store.rs", "operations": [ ... ] }
  ]
}
```

| Rule | Behaviour |
|---|---|
| Anchor | The operation's `old`/`anchor` snippet must match **exactly once**. A unique anchor is required; guessing between several matches is how a patch corrupts a file. |
| `count` | Optional. When given, the snippet must match exactly `count` times and all are replaced. Any other number is an error. |
| `replace` | Swap `old` for `new`. |
| `insert_before` / `insert_after` | Insert `new` relative to a unique anchor. |
| `delete` | Remove `old`. |
| Atomicity | Every operation is computed against the original text, and files are written only if **all** operations across **all** files succeed. A failure in operation 3 of 5 leaves every file untouched. |
| Ordering | Operations apply in order to the accumulating result, so a later operation may anchor on text an earlier one inserted. |
| Existence | Target files must already exist; creating a file is `write_file`'s job. |
| Guards | Same path guard and `max_file_bytes` cap as `write_file`. |
| Returns | A unified diff per file, plus the count of operations applied. |
| Approval | `Write`. One approval covers the whole patch, not one per operation. |

`edit_file` is retained: for a one-line change it is a smaller, less error-prone request than a patch.

#### `cron_add` / `cron_list` / `cron_remove`

| Field | Spec |
|---|---|
| Risk | Write |
| `cron_add` | Params `name?`, `schedule` (5-field cron), `prompt`, `cwd?`, `timezone?`, `session_mode?` (`reuse` \| `new`), `max_runs?`, `allow_overlap?` |
| `cron_list` | Returns jobs with `next_run_at`, `last_status` |
| `cron_remove` | Params `id` or `name` |
| Validation | Cron expression parsed before insert; next run computed; reject if never fires |
| Approval | `ask` for add/remove; list is `auto` |

#### `http_fetch`

| Field | Spec |
|---|---|
| Risk | Network |
| Params | `url`, `method?: GET\|POST\|PUT\|DELETE`, `headers?`, `body?`, `max_bytes?` |
| Guards | Scheme must be `https` (or `http` only if the host is explicitly allowlisted); domain must match `allowed_domains` globs; resolve DNS and reject private/loopback/link-local ranges when `block_private_ips`; no cross-domain redirects unless allowlisted; response capped |
| Returns | `{ status, headers, body, truncated }` |
| Approval | `auto` if domain allowlisted, otherwise `ask` |

#### `remember` / `recall`

| Field | Spec |
|---|---|
| Risk | Write / ReadOnly |
| `remember` | Params `key`, `value`, `tags?: string[]`; upsert into `memory` (namespace = workspace) |
| `recall` | Params `query`, `limit?`; SQLite FTS5 over `key`, `value`, `tags`; returns ranked snippets |
| Notes | Not a vector store; deterministic and inspectable. Useful for cross-session facts and preferences. |

#### `mcp_call`

> **Not implemented (M5).** The flattened `mcp__<server>__<tool>` tools below are the surface that
> ships; `mcp_call` is deferred because the per-server approval policy keys on a tool name and this
> tool would have to be special-cased to read `args.server` for its decision. See §5.10.

| Field | Spec |
|---|---|
| Risk | Inherited from the target tool's declared risk, minimum `Network` |
| Params | `server`, `tool`, `arguments` |
| Guards | Server must be configured; tool must be in `tool_allow` for that server; approval per server config |
| Returns | Raw MCP tool result content, size-capped |
| Discovery | External tools are also surfaced directly to the model as `mcp__<server>__<tool>` so the model can call them without the indirection; `mcp_call` exists as a generic escape hatch and for models that prefer a compact tool list |

### 5.6 Approval engine

Policy resolution for each invocation:

```
deny-rule match            → Deny
allowlist match (any scope)→ Auto
read-only command (opt-in) → Auto   -- run_command only; see D48
tool.risk == ReadOnly      → Auto
!stdin.is_tty()            → policy.noninteractive (default Deny)   -- for everything else
tool.risk == Execute|Write → Ask
otherwise                  → policy.default
```

`ReadOnly` is allowed before the TTY is consulted, so a piped process can still read. `Network` is
gated with `Execute`/`Write` rather than with `ReadOnly`, because the request leaves the machine and
can be induced by untrusted content. See D15.

The *read-only command* line is the invocation-level analogue of `ReadOnly` for `run_command`, which
is always `Execute`: a command the classifier in `imp_core::read_only` recognises as an observation
is approved without a prompt when `[policy.read_only].enabled` is set, or after `/auto` turns the
shortcut on for the session. It is consulted after deny rules, allow rules and the command
classifier, and only when the classifier flagged nothing, so it can shorten the path to a prompt but
never bypass a refusal. See D48.

**Optional System One guard (M3.5, D16).** When `[guard].enabled` is set and a flagged `run_command`
falls in an eligible category, the engine may consult a `/v1/systemone` model before deciding to
prompt. It sits between rule 3 and rule 4 and can only convert a prompt into a silent allow:

```
classifier flags an ineligible category   → Ask, with no network call
classifier flags an eligible category    → guard; allow at/below allow_threshold, else Ask
```

Deny rules and allowlists are unaffected and still run first, and a guard error resolves to `Ask`
rather than `Ok`. FR-43 through FR-47 are the normative statement.

**Per-family fallbacks (M5, D21).** The engine can also carry a list of `ToolPolicy { prefix, decision }`
that substitutes the *fallback* rules 5 and 6 for a family of tools. It is how an MCP server gets its
own approval policy: the client registers one entry per server, keyed on the `mcp__<server>__` prefix
its tools carry, so a trusted local server can be `auto` while a network server stays `ask`. Only the
fallback moves — deny rules, allow rules and the classifier still run first, and the longest matching
prefix wins so the outcome does not depend on the order the servers were configured in.

The guard is consulted only where a prompt would otherwise be shown. A non-interactive run has no
prompt to resolve, so it never reaches the model and still takes `policy.noninteractive` (D15): a
silent allow there would widen a decision the static rules already made. Eligibility is a static
floor — `privilege`, `remote-execution` and `destructive` are resolved in-process, so an ineligible
command never leaves the machine (FR-44). The request is `POST {base_url}/v1/systemone` with
`{"state": "<command>"}` and nothing else (FR-46); the reply is a number, `{"unsafe": <0..1>}`, and
the thresholds are applied here, in our code (D16). Bands are audited as `guard_allow`,
`guard_uncertain`, `guard_deny`, and a guard that fails is audited as `guard_error` before the
ordinary prompt takes over (FR-45, FR-47).

Deny rules always win; an explicit deny cannot be allowlisted away at runtime.

Interactive prompt:

```
⚠  run_command requests execution
   $ rm -rf ./build
   risk: execute   cwd: /Users/me/proj

   [o]nce  [s]ession  [a]lways  [d]eny  [e]dit
```

- `once` → this call only.
- `session` → in-memory pattern for this process.
- `always` → persisted to `approvals` with a normalized pattern derived from the command's first token/verb (e.g. `rm *` is *not* derivable; the UI shows the exact pattern being persisted and requires confirmation).
- `edit` → hand the command back to the user as a shell-escape so they can run it themselves.

Risk classification is a small, auditable rule set, not a whitelist of everything:

```
Execute patterns requiring explicit consent:
  - filesystem destructive: rm -r, dd, mkfs, shred, truncate on existing paths
  - privilege: sudo, doas, su, chown, chmod with setuid/777
  - remote execution: curl|wget piped to sh, bash -c from network input
  - version control history: git push --force, git reset --hard, filter-branch
  - package/global mutation: npm -g, brew install, apt/pip system installs
  - process control: kill -9 of non-child pids, systemctl
  - anything writing outside workspace roots
```

### 5.7 Cron scheduler

- A single task ticks every 1000 ms (aligned to the next second boundary) and queries `jobs WHERE enabled = 1 AND next_run_at <= now`.
- Firing: insert a `job_runs` row with status `running` **before** dispatch, then dispatch a prompt into the agent loop as a synthetic user message tagged `[cron:<name>]`.
- Session mode: `new` creates a fresh session per run (default for runaway isolation); `reuse` appends to a named session so the job accumulates context.
- Concurrency capped by `max_concurrent_jobs`; excess due jobs queue in memory and are recorded as `queued`.
- On completion, `next_run_at` is recomputed from the cron expression and the job is `UPDATE`d in the same transaction as the run's terminal status.
- **Catch-up on startup:** for each job with `next_run_at` in the past, apply `missed_run_policy`:
  - `skip` — recompute `next_run_at` and record a `skipped` run.
  - `run_once` — run once, then recompute (default).
  - `run_all` — run each missed occurrence, bounded by a safety cap (e.g. 20).
- Preventing overlap: a job that is still `running` when due is marked `skipped (overlap)` unless it has `allow_overlap = true`.
- Job prompts run with the **non-interactive** policy: they cannot prompt for approval, so any tool that would `ask` is denied unless allowlisted. This is intentional and documented.
- `missed_run_policy` is a `[cron]` setting rather than a per-job one: the `jobs` table has no column for it, and adding one would mean a migration for a knob nobody has asked to vary per job.

**Fire semantics (as implemented, M4).** A fire *consumes* its occurrence: `next_run_at` is advanced to the next occurrence and `last_run_at`/`last_status` are set in the same transaction that inserts the `job_runs` row. Without that, a job whose run outlasts its own interval would look due on every tick. Completion then recomputes `next_run_at` from the cron expression **and the completion instant**, which can only move it later, and writes it in the same transaction as the run's terminal status — the two rules together are what §5.7's "on completion" sentence means in practice.

The statuses are used distinctly: `skipped` is a catch-up decision, `overlap` is a live run colliding with its own next occurrence, `queued` is an accepted run waiting for a concurrency slot, and `running`/`ok`/`failed` are the run's own life. A `reuse` job's session id is written on completion as a *separate* statement: `jobs.session_id` is a foreign key, and an adoption that fails must not roll back the run's terminal status and leave the row claiming to be running forever.

Catch-up replays occurrences with no overlap check, because the occurrences being replayed never ran — there is no live run to collide with. The `max_concurrent_jobs` cap still applies, so a `run_all` burst is bounded twice: by `missed_run_cap` and by the queue.

### 5.8 SQLite store

Location: `$XDG_STATE_HOME/imp/imp.db` (or macOS Application Support). WAL mode, `foreign_keys=ON`, `busy_timeout=5000`, `synchronous=NORMAL`.

```sql
CREATE TABLE schema_migrations (
  version    INTEGER PRIMARY KEY,
  applied_at TEXT NOT NULL
);

CREATE TABLE sessions (
  id          TEXT PRIMARY KEY,          -- uuid v7
  title       TEXT,
  cwd         TEXT NOT NULL,
  model       TEXT,
  provider    TEXT,
  created_at  TEXT NOT NULL,
  updated_at  TEXT NOT NULL,
  meta        TEXT NOT NULL DEFAULT '{}'
);

CREATE TABLE messages (
  id           TEXT PRIMARY KEY,
  session_id   TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
  seq          INTEGER NOT NULL,
  role         TEXT NOT NULL CHECK (role IN ('system','user','assistant','tool')),
  content      TEXT,
  tool_calls   TEXT,                     -- JSON array, assistant only
  tool_call_id TEXT,                     -- tool role only
  tool_name    TEXT,
  is_summary   INTEGER NOT NULL DEFAULT 0,
  tokens_in    INTEGER,
  tokens_out   INTEGER,
  created_at   TEXT NOT NULL,
  UNIQUE (session_id, seq)
);
CREATE INDEX idx_messages_session ON messages(session_id, seq);

CREATE TABLE jobs (
  id               TEXT PRIMARY KEY,
  name             TEXT UNIQUE,
  schedule         TEXT NOT NULL,        -- 5-field cron
  timezone         TEXT NOT NULL,
  prompt           TEXT NOT NULL,
  cwd              TEXT NOT NULL,
  session_id       TEXT REFERENCES sessions(id) ON DELETE SET NULL,
  session_mode     TEXT NOT NULL DEFAULT 'new',
  enabled          INTEGER NOT NULL DEFAULT 1,
  allow_overlap    INTEGER NOT NULL DEFAULT 0,
  max_runs         INTEGER,
  runs_count       INTEGER NOT NULL DEFAULT 0,
  created_at       TEXT NOT NULL,
  last_run_at      TEXT,
  next_run_at      TEXT,
  last_status      TEXT
);
CREATE INDEX idx_jobs_due ON jobs(enabled, next_run_at);

CREATE TABLE job_runs (
  id           TEXT PRIMARY KEY,
  job_id       TEXT NOT NULL REFERENCES jobs(id) ON DELETE CASCADE,
  session_id   TEXT REFERENCES sessions(id) ON DELETE SET NULL,
  started_at   TEXT NOT NULL,
  finished_at  TEXT,
  status       TEXT NOT NULL,            -- queued|running|ok|failed|skipped|overlap
  exit_summary TEXT,
  output_ref   TEXT
);
CREATE INDEX idx_job_runs_job ON job_runs(job_id, started_at DESC);

CREATE TABLE approvals (
  id          TEXT PRIMARY KEY,
  tool        TEXT NOT NULL,
  pattern     TEXT NOT NULL,
  scope       TEXT NOT NULL,             -- workspace path
  decision    TEXT NOT NULL CHECK (decision IN ('allow','deny')),
  created_at  TEXT NOT NULL,
  expires_at  TEXT,
  UNIQUE (tool, pattern, scope, decision)
);

CREATE TABLE memory (
  id         TEXT PRIMARY KEY,
  namespace  TEXT NOT NULL,              -- workspace hash
  key        TEXT NOT NULL,
  value      TEXT NOT NULL,
  tags       TEXT NOT NULL DEFAULT '[]',
  session_id TEXT,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  UNIQUE (namespace, key)
);

CREATE VIRTUAL TABLE memory_fts USING fts5(
  key, value, tags, content='memory', content_rowid='rowid'
);

CREATE TABLE audit_log (
  id         INTEGER PRIMARY KEY AUTOINCREMENT,
  ts         TEXT NOT NULL,
  session_id TEXT,
  turn_id    TEXT,
  tool       TEXT,
  risk       TEXT,
  decision   TEXT,                       -- auto|allow_once|allow_session|allow_always|deny
  args_digest TEXT,                      -- hash, not raw args, for commands
  outcome    TEXT,
  duration_ms INTEGER
);
```

Notes:
- `output_ref` points at a file under the state dir when a job's output exceeds an inline cap.
- `args_digest` stores a hash of sensitive arguments so the audit trail is useful without persisting secrets.
- Messages are append-only; the `seq` counter is allocated inside the transaction that writes the message.

**Migrations.** A forward-only list in `imp-store::migrate`, each entry applied once and recorded
in `schema_migrations`. A database newer than this build understands is refused rather than read
optimistically, and `PRAGMA quick_check` runs before any migration so a corrupt file fails loudly
instead of half-initialising. Tables that later milestones own are created up front, with the FTS
triggers that keep `memory_fts` in step — without those, `recall` would silently match nothing in M3.

**Write semantics.** A turn is persisted as one transaction: the user message, every assistant and
tool message, and the session's `updated_at` either all land or none do, so a transcript can never
contain half a turn. A hard crash mid-turn loses that turn; nothing before it.

**History windowing.** Reloading a long transcript trims to `agent.history_window`, but index 0 (the
system prompt) is configuration rather than history and is always retained. The cut is advanced to a
`user` message, and never left sitting on a `tool` result: an assistant `tool_calls` message whose
results were dropped is rejected by the provider, so a turn must never be split.

**Compaction.** A budget that no longer fits is compacted rather than only trimmed: everything before
the last `compact_keep_turns` turns is replaced by one `[summary]` message, generated by the session's
own provider (or `agent.compact_model`), so the decisions, paths and errors survive the drop. It runs
at `compact_threshold` of the budget, before the request would be rejected, because writing a summary
needs room of its own. A summary that would cost more than the messages it replaces is discarded, and
a failed summarization falls back to trimming. The stored transcript keeps every message (R3): the
summary is a view over history, not a rewrite of it, so compaction cannot corrupt a resume.

### 5.9 MCP server

The server task is started with `imp mcp serve`, over **stdio** or **Streamable HTTP**
(`[mcp.server].transport = "http"`). It uses `rmcp`'s server trait and shares the same core loop, store, and policy engine.

**Exposed tools** (default surface):

| Tool | Purpose | Default gate |
|---|---|---|
| `agent_ask` | Run one agent turn; params `prompt`, `session_id?`, `model?`, `max_tokens?`, `max_iterations?`, `allow_tools?` | `auto` (read-only tool subset) |
| `agent_list_sessions` | Enumerate sessions | `auto` |
| `agent_get_session` | Fetch transcript | `auto` |
| `agent_list_tools` | Introspect enabled tools + risk classes | `auto` |
| `cron_add` / `cron_list` / `cron_remove` | Manage jobs | `auto` if `expose_cron_write`, else denied |
| `agent_run_command` | Shell execution | **denied unless `mcp.server.expose_exec = true`** |
| `agent_write_file` | File writes | **denied unless `mcp.server.expose_write = true`** |

**Resources:** `imp://sessions`, `imp://sessions/{id}`, `imp://jobs`, `imp://config-redacted`.

**Prompts:** `imp_agent` — a parameterized prompt that frames a task for the agent, so hosts that only support prompts (not tools) can still use imp.

**Protocol posture:** read-only by default; write/exec capabilities are opt-in flags that are printed loudly at server startup, so an operator cannot enable them unknowingly.

**As implemented (M6).** The server is `imp mcp serve` over stdio, in
`imp-cli::mcp_serve`. It assembles the same pieces a session does — the same
store, the same built-in tool registry, the same `PolicyEngine` — and it never
starts a REPL, because both own stdin/stdout (R5). A turn's output goes nowhere
but the protocol, so diagnostics stay on stderr.

- **The surface, name for name.** `agent_ask`, `agent_list_sessions`,
  `agent_get_session`, `agent_list_tools`, then `cron_add`/`cron_list`/
  `cron_remove` when `expose_cron_write`, then `agent_run_command` and
  `agent_write_file`. `agent_ask`'s inner agent gets the **read-only subset** of
  the built-in registry, chosen by the risk class itself rather than by a
  hand-kept list, and `allow_tools` can narrow it further (an unknown name is
  dropped, fail-closed). So the default surface observes and nothing else.
- **A disabled capability is denied, not hidden.** `agent_run_command` and
  `agent_write_file` are always listed. Without their flag the gate carries a
  deny rule that refuses every call, and the refusal is written to `audit_log`
  like any other decision; the caller receives it as a readable tool error, not
  an opaque protocol error. Hiding the tool would leave the host guessing at
  what the server can do.
- **The gate is non-interactive, and an enabled family substitutes the fallback.**
  stdin is the protocol pipe, so there is no terminal to prompt on: an enabled
  family gets a `ToolPolicy` that resolves to `auto` (D21) and everything else
  takes `policy.noninteractive`. Deny and allow rules run first as always, which
  is why `agent_run_command` is decided under the name `run_command` — an
  operator's existing `run_command` rules protect the MCP surface too, and the
  command classifier still sees the command.
- **Resources and prompt.** `imp://sessions`, `imp://jobs`,
  `imp://config-redacted` and the `imp://sessions/{id}` template; the
  `imp_agent` prompt frames a task for a host that has prompts but no tools.
  `config-redacted` serialises the effective config with header values whose
  *name* looks like a credential replaced by `<redacted>`, so the resource cannot
  become an exfiltration path for a hand-written config.
- **The scheduler does not run here.** A job added over MCP is stored in the
  same database and runs the next time a process with a scheduler opens it.
  `mcp serve` is a protocol server, not a daemon; starting a scheduler inside it
  is a separate decision and is not part of this milestone.

**Transport (as implemented, M10).** With `transport = "http"` the same surface
is served over `rmcp`'s Streamable HTTP transport on `bind`, instead of owning
stdin/stdout. The gate, the read-only default and the `agent_*` naming are
unchanged — the transport is a socket where stdio was a pipe, nothing above it
moves. The bind policy (D27) runs at config load, before `TcpListener::bind`, so
a wildcard, a globally routable address, or a non-loopback bind with no token is
refused as a startup error rather than served unauthenticated; the default is
`127.0.0.1:8788`. When a token source is configured the bearer token is required
on every request, loopback included (D26); the config carries only the *name* of
the environment variable or the *path* to the `0600` credentials file, resolved
through the same `resolve_secret` the provider key uses. Off a loopback bind the
`Host` allowlist is switched off deliberately: a MagicDNS name cannot be derived
from an `IP:port` bind, and the token — not the request's `Host` or its source
address — is the authentication boundary. There is no TLS, also deliberately
(D26): inside the tailnet WireGuard already encrypts the traffic.

### 5.10 MCP client

- Servers from `[mcp.client.servers.*]` are reached at startup — spawned over stdio when they have a `command`, or contacted over Streamable HTTP when they have a `url` (lazily on first use if `lazy = true`).
- `tools/list` results are flattened into the registry as `mcp__<server>__<tool>`, with `inputSchema` passed through verbatim (imp does not rewrite third-party schemas).
- `tool_allow` per server filters what reaches the model; anything not listed is invisible and uncallable.
- Per-server approval policy overrides the global default, so a trusted local server can be `auto` while a network server is `ask`.
- Server failures degrade gracefully: the tools are removed from the catalog and a `system` notice explains why. They are retried on the next turn.
- Tool name collisions with built-ins are resolved by prefixing, never shadowing: built-ins always win.

**Configuration.** Each `[mcp.client.servers.<name>]` entry is reached one of two ways, and they are
mutually exclusive: a `command` (with `args`) imp spawns over stdio, or a `url` it speaks
Streamable HTTP to. `Config::validate` accepts exactly one of the two — both, or neither, is a startup
error, as is an `args` list beside a `url` or a scheme that is not `http(s)`. A `url` server may name a
bearer token source — `token_env` (a variable *name*) or `token_file` (a credentials-file *path*),
sent as `Authorization: Bearer` — and the value is never in the config. The table key is the namespace:
it names every tool of that server, and a key containing `__` is rejected at load time rather than left
to collide with another server's. `command` is spawned directly, never through a shell, so an argument
cannot become a second command.

```toml
[mcp.client.servers.files]
command = "mcp-server-files"
args = ["--root", "/srv"]
lazy = true                       # contact it on the first turn, not at startup
tool_allow = ["read_*", "list"]   # empty allows nothing; ["*"] allows everything
approval = "auto"                 # this server's tools substitute the global default

[mcp.client.servers.peer]         # a second imp, on the same tailnet
url = "http://peer.tailnet.ts.net:8788/mcp"
token_file = "~/.config/imp/peer.credentials"   # api_key = "…", mode 0600
approval = "ask"
tool_allow = ["*"]
```

**What is published, and what is gated (as implemented, M5).** An external tool is an ordinary `Tool`:
the same registry, the same `Agent::dispatch`, and the same approval gate as `run_command`. There is no
path from the model to a server that skips a policy decision. Its risk class is always `Network` —
`§5.5` says "inherited from the target tool's declared risk, minimum `Network`", and since MCP states
risk only in `ToolAnnotations`, which are *hints from a server imp does not vouch for*, an inherited
hint could only ever lower the class. With the floor at `Network` the honest reading is the floor
itself (D21, T6). `tool_allow` is applied before the catalogue is built, so a tool the operator did not
name is absent from the schemas the model receives *and* unresolvable by name; there is one thing the
operator has to write to widen it, and it is in the config.

**The catalogue is live, not a snapshot.** `imp-core` exposes a `ToolCatalog` trait, and the
registry consults it on every read of the tool list. That is what makes §5.10's "retried on the next
turn" real: at the start of each turn the client re-attempts every server that is not up, and a server
that answers publishes its tools into the catalogue the model is about to be offered — through the
same frozen `Arc<ToolRegistry>` the session was built with. Ordering is the shadowing rule: registered
tools come first and a name already taken is skipped, so an `mcp__…` tool can never displace a
built-in, whatever a server calls itself.

**`lazy`.** A lazy server is not contacted while the session is assembled; the first turn does it
(`OnStart::Eager` versus `OnStart::All`). Its tools still reach the catalogue from that point, because
a tool list cannot be discovered without a connection — `lazy` buys a cheaper session start, not a
hidden tool set. `imp mcp list` and `imp mcp tools <server>` are explicit uses and start a lazy
server immediately, which is also how an operator inspects one.

**HTTP transport (as implemented, M10).** A `url` server goes through the same
`McpClient`, so the catalogue, `tool_allow`, the per-server policy, the transition
notices and the retry are the stdio behaviour unchanged — a peer that is down is a
notice and a retry on the next turn, exactly as a child process that failed to
spawn is. The difference is the handshake: `connect_http` opens `rmcp`'s Streamable
HTTP client against the endpoint and, when a token is configured, sends it as
`Authorization: Bearer` on every request. The token is resolved from `token_env`
then `token_file` with the provider's resolution; a source that is *named* but
yields nothing is an error rather than a silent anonymous call. `imp mcp list`
prints the `url` in place of the `command` line for such a server.

**Failure.** A server that cannot be started costs its tools and nothing else: the turn completes, the
tools are gone from the catalogue, and the reason arrives as a `system` message. Notices are emitted on
*transitions* only, so a server that stays down is explained once rather than on every turn.

**Shutdown.** `imp` closes its MCP connections on the way out of both the REPL and a one-shot run,
so a session does not leave server processes behind. A hard kill (SIGKILL) can still orphan a child;
that is documented rather than hidden.

**`mcp_call` is not implemented.** §5.5 lists it as a generic escape hatch for models that prefer a
compact tool list. The flattened tools are the primary surface this milestone delivers, and a generic
`mcp_call(server, tool, …)` would need a policy decision that the per-server policy cannot express —
the gate decides on a tool name, and `mcp_call` would have to be special-cased to read `args.server`.
Deferred rather than half-gated; see AGENTS.md's known gaps.

### 5.11 Frontend (fx-like REPL)

**Philosophy:** the terminal is a scrollback, not a canvas. No alternate screen, no full redraw, no mouse capture.

```
$ imp
imp 0.1 · gpt-4.1-mini · /Users/me/proj · policy: ask

› summarize the TODOs in this repo and open a job to nag me weekly

  ▸ recall {"query":"todo conventions"}            auto      12ms
  ▸ run_command {"command":"rg -n TODO --stats"}   ask    ▶ approved (once)

  You have 14 TODOs across 6 files. The bulk are in `src/store.rs`...

  ▸ cron_add {"schedule":"0 9 * * 1","prompt":"..."} ask   ▶ approved (always)

  Created job "weekly-todo-nag" — next run Mon 09:00 America/Mexico_City.

› /cron
  weekly-todo-nag   0 9 * * 1   next Mon 09:00   last —      new session
```

Interaction details:

| Input | Behavior |
|---|---|
| Free text | New turn in the current session |
| `!<cmd>` | Run locally in the shell, output to the terminal only (never enters model context) |
| `/help`, `/?` | Slash command help |
| `/model` | List the models advertised by the provider, marking the current one |
| `/model <name>` | Switch the model for the session |
| `/new` | Start a fresh conversation, keeping the same session object |
| `/sessions` | List stored conversations, newest first |
| `/resume <id>` | Continue a stored conversation; accepts a full id, a unique prefix, or a position from `/sessions` |
| `/rename <title>` | Set this conversation's title |
| `/clear` | Forget the messages, keep the system prompt and the session id so the transcript stays resumable |
| `/tools` | Enabled tools with risk classes and approval status |
| `/session` | Show the conversation id sent to the provider in `${session}` headers |
| `/where` | Show the database path |
| `/cost` | Token and estimated cost for the session |
| `/context` | The model's window, the budget, and what the request already costs |
| `/compact [on\|off]` | Summarize older turns now, or toggle automatic compaction |
| `/auto [on\|off]` | Toggle the opt-in read-only-command shortcut (D48): a recognised read-only shell command runs without a prompt |
| `/cron`, `/jobs` | Job table with next-run times (M4) |
| `/allow <tool> <pattern>` / `/deny …` / `/approvals` | Inspect and edit policy (M2) |
| `/quit`, Ctrl-D | Exit |
| Ctrl-C | Cancel current turn; twice in a row exits |
| Ctrl-L | Clear screen (local only; scrollback intact) |

Resuming keeps the stored conversation's id, so the `${session}` header stays identical across the
restart — that is the point of the header, and a fresh id would look like a new conversation to the
gateway.

Rendering rules:
- Assistant text streams to stdout; tool activity goes to **stderr** as single dimmed lines, so `imp run ... > out.txt` yields clean output.
- On a terminal, assistant text is rendered as markdown: headings, bold/italic/strikethrough, inline and fenced code, lists, blockquotes, rules, and pipe tables with box-drawing borders and column alignment. Rendering is **per block**, not per token — a block is drawn as soon as it is complete and nothing already printed is ever revised, which is what keeps the scrollback intact. A table cannot be laid out until its last row arrives, so it waits for one.
- **Piped output is not rendered.** When stdout is not a terminal the raw markdown is emitted, because the source is the more useful thing to capture and reformat later. `--markdown` does not override this.
- Layout and colour are separate. `NO_COLOR` and `--no-color` suppress the ANSI escapes but keep table borders and list markers, since alignment carries meaning that colour does not. `--no-markdown` turns rendering off entirely.
- Width comes from `--width`, then `$COLUMNS`, then 80. A table that cannot fit the width is emitted as plain rows rather than drawn into a mangled grid.
- `--json` emits newline-delimited JSON events (`{"type":"text"…}`, `{"type":"tool_call"…}`, `{"type":"done"…}`) for scripting, and never renders markdown: a JSON consumer wants the model's own text, not a drawn table.
- Piped stdin: `echo "..." | imp run -` reads the prompt from stdin; combined with a non-TTY, policy is enforced as non-interactive.
- Colors via a tiny in-tree ANSI helper (`imp-cli/src/style.rs`) honoring `NO_COLOR`, `--no-color` and `[ui].color`; 16-colour SGR only, no truecolor dependency. Colour is never the only signal — every coloured mark also carries a word or a glyph (NFR-8).
- Tool activity is one line per call carrying its risk class, then a finished line carrying the wall-clock duration: `▸ run_command {"command":"ls"}  execute` followed by `  ↳ 12ms …`. The approval prompt prints the outcome it recorded (`approved (once)`), so the decision is visible where it was made as well as in the audit trail.
- `[ui]` holds the cosmetic choices: `color = auto|always|never`, `icons = auto|ascii|none`, and `spinner` (on by default, drawn only on a terminal). It never changes what the agent may do, and `--json` ignores it.
- With `[ui].spinner`, a single transient activity mark is drawn on stderr while a turn waits. It is erased before the first real output, and never shown off a terminal or under `--json`/`--quiet`; the terminal stays a scrollback, not a canvas (D2).
- Resuming a conversation (`--resume`, `--continue`, `session resume`) prints it on start, unless `--no-history`: the user's prompts carry the same `›` as the live prompt and the assistant's replies are printed bare, with each assistant tool call on an indented line and the system prompt never shown. A resumed prompt is context, not a blank slate.
- The approval prompt collapses to one line — `▸ run_command · approved (once)` — once it is answered, on a terminal. The questionnaire is erased rather than left in the scrollback; a redirected stderr keeps the full prompt.

### 5.12 CLI surface

```
imp [OPTIONS] [PROMPT]              # REPL, or one-shot if PROMPT given
imp run [OPTIONS] <PROMPT|->        # one-shot, non-interactive-friendly
imp session list|show <id>|rm <id>|resume <id>
imp cron add --schedule <CRON> --prompt <TEXT> [--name N] [--cwd P]
imp cron list [--json]
imp cron remove <id|name>
imp mcp serve [--stdio]             # run as MCP server
imp mcp list                        # configured servers + discovered tools
imp mcp tools <server>              # inspect one server
imp init [--check] [--project] [--force] [--non-interactive]
            [--header 'Name: value']… [--credentials-file <PATH>] [--no-store-token]
                                       # first-run setup for the model backend (§5.1.1)
imp config show|path [--origin] [--json]
                                       # read-only; `--origin` names the layer each value
                                       # came from (§5.14); `config init` aliases `imp init`
imp config sync [--dry-run] [--check] [--node <NAME>] [--remote <URL>] [--ref <REF>]
                                       # converge the machine layer from the master repo (§5.14)
imp doctor                          # env, config, db, provider reachability
imp update [--check] [--yes] [--force] [--rollback]
                                       # check a release channel and replace the installed
                                       # binary (§9); `--check` writes nothing and exits 1
                                       # when an update is available

Global options:
  --model <NAME>        --base-url <URL>      --api-key-env <VAR>
  --yes                 # auto-approve policy.default == ask (dangerous; prints a warning)
  --deny                # force deny for everything not allowlisted
  --json                --no-color            --verbose|-v  --quiet|-q
  --markdown  --no-markdown      --width <n>
  --cwd <DIR>           --resume <SESSION>    --max-iterations <N>
  --continue|-c         # continue the most recent session in --cwd
  --config <FILE>       --db <FILE>
```

**Exit codes:** `0` success · `1` turn failed, or `imp update --check` found an update available · `2` usage error · `3` provider/auth error, or an update could not reach its release channel · `4` refused (policy denied in non-interactive mode, or an operation declined to proceed — e.g. `init` against an existing config without `--force`, an update whose checksum or commit did not verify, or one cancelled at the prompt) · `5` internal error.

---

### 5.13 Peers — asking a larger model

A **peer** is another imp's `mcp serve` endpoint. Nothing new is spoken: a peer is
an MCP server like any other (M10.1, D25). What this section adds is the *shape of the
delegation* — how a small local model hands a hard question to a big remote one.

```toml
[peers.big]                                  # one delegation tool, `peer__big_ask`
url = "http://big.tailnet.ts.net:8788/mcp"   # a remote imp; `command` is the local form
token_file = "~/.config/imp/big.credentials"   # api_key = "…", mode 0600
max_tokens = 512          # forwarded to the peer's `agent_ask` when a call does not name one
result_cap_bytes = 8192   # what the local model sees; around 8 KB by default
approval = "ask"          # this peer's fallback, substituting policy.default (D21)
```

- **One tool per peer, flattened `peer__<name>_ask`.** This is deliberate, and the
  alternative is refused on principle: a generic `delegate(target = "…")` would move the
  approval decision off the tool *name* and into an argument, which is precisely the trap
  `mcp_call` is deferred over (D20) — the engine keys its rules on names, and a parameter
  recreates the ambiguity the flattened tools exist to remove. The name is also the family
  key: `peer__big_ask` is the string a `[policy.allow]`/`[policy.deny]` rule and a per-peer
  `approval` key on, and an operator's `mcp__…` rules are untouched by it.
- **Params `{ brief, context?, max_tokens? }`.** `brief` is required and non-empty.
  A `tools` argument is *refused*, not ignored: `agent_ask` already runs the read-only
  subset of the peer's own registry (D22), and the caller does not get to widen it.
- **A brief, not a trajectory.** Only the brief — with `context`, if given, prefixed — is
  sent; nothing from the local transcript rides along. This is the evidence-backed choice:
  the handoff-tax study (arXiv 2608.24358) measures that escalating with the full
  trajectory recovers *less than half* of the quality gap and that the escalation works
  better the less of the weak model's history it carries. It is also mechanical: the local
  model that receives the answer has a 4–8K context, so a long answer back is what kills
  it. Hence the result cap.
- **Risk is `Network`** — the MCP floor (D21). The brief leaves the machine, so the gate
  is consulted *before* anything is sent, and with `policy.noninteractive = "deny"` (the
  default) a cron job cannot escalate: `build_cron_gate` is non-interactive, so
  `peer__<name>_ask` is refused there exactly as `http_fetch` is. This is the same
  reasoning the System One guard carries — the payload leaves the machine (D16).
- **The peer's answer is data.** It is placed in the tool result and nowhere else; it
  never becomes a system prompt and is never executed. A peer is a remote endpoint, and
  its text is untrusted input, exactly like any other tool output (T14). A delegated turn
  is told so in the tool's own description.
- **Depth cap 1.** A peer answers through its own `agent_ask`, whose inner agent holds
  only the read-only subset of the built-in registry (D22). A delegation tool is
  `Network`, so it is never in that subset: a peer cannot pass the brief on to a third
  model. The cap is the read/write line itself (D15), not a counter a caller could reset,
  and `mcp serve` never assembles `[peers.*]` — so two small models passing the ball is
  not reachable by construction. `crates/imp-cli/src/mcp_serve.rs` pins the property
  with a test on the inner surface.
- **Cost is visible.** The peer's `agent_ask` reports the tokens it spent; the delegation
  tool puts them in the tool result's metadata (`usage`), and the loop folds them into the
  turn's usage (`ToolOutput::reported_usage`). `/cost` therefore includes what the peer
  spent. Advisory, as always (R8): a peer that reports nothing simply does not count, and
  no turn is blocked on it.
- **Failure is a tool error.** A peer that cannot be reached, refuses the brief, or dies
  mid-call becomes a tool result the model can read; the turn carries on, and the
  connection is retried on the next call. Nothing panics, and nothing hangs the loop.
  A `result_cap_bytes` that the answer exceeds cuts the answer and marks it `truncated`.

`mcp serve` ignores `[peers.*]`, for the same reason it ignores `[mcp.client.servers.*]`:
the delegated turn's surface is the built-ins' read-only subset, and a peer there would be
the second hop the depth cap exists to forbid.

---

### 5.14 Master config layer — `imp config sync`

A **master** lets one place describe how all the other nodes are configured, without depending on
the network between them. It is deliberately *not* a process: the master is a **git repository plus
a convention**, and each node converges by pulling it. This section fixes the shape of that pull;
it is a design proposal (M11), not a built feature (D38–D42).

**Pull, never push.** Every node runs `imp config sync`. The master never reaches into a node,
so convergence works for a node behind NAT, on a different network, or on one that is not on the
tailnet at all — which is the case of the 1050 Ti today. The evaluated alternative is an HTTP
endpoint served by the master (either a live `GET /config` or a push channel): it is **refused**
because it needs a service that is always on (against the no-daemon principle and the non-goals
below), it needs every node to be able to *reach* the master (exactly the connectivity problem the
pull solves), and it re-implements — worse — what git already is: versioning, transport auth,
history, and an offline cache. Recorded as **D38**.

**Channel: the git repo.** `[configsync].remote` and `.ref` name the channel; both are read from
the **local layers only** (flags or the user config), because this is the bootstrap that tells a
node where the master is — the synced layer itself may never define it.

```
imp-config/                 # a repo of its own (Forgejo), not the imp source tree
├── base.toml                  # shared by every node
├── nodes/<name>.toml          # the overlay for one node
└── checksums.txt              # the commit and a sha256 per file (generated, committed)
```

`sync` shells out to the system `git` (a bare mirror under `$XDG_CACHE_HOME/imp/config-sync/`),
rather than growing a second protocol: git is already where the nodes are, it already carries the
host's auth (SSH key or HTTPS token), and it is the versioned, auditable store this milestone is
about. If `git` is absent the command fails loudly — it never degrades to a silent no-op.

**Base + overlay.** The synced layer is `base.toml` deep-merged with `nodes/<name>.toml` on
`toml::Value`, the existing merge, so an overlay only restates the keys it changes. A directory of
per-node files is chosen over one `[node."<name>"]` table in a shared file: adding a node is a file
add, two nodes never collide on one table (a merge hotspot), and one node's policy is one file to
read. **D40**.

**How a node knows its own name.** In order: the `--node` flag → `[configsync].node` → `hostname
-s` → the self entry of `tailscale status --json` (`DNSName`, domain stripped). The resolved name
and the rule that produced it are logged and printed by `config show --origin`. A name that does
not appear in `nodes/` is **not** an error: the node applies the base alone and warns, so a freshly
provisioned machine converges before its entry exists. A node that *is* listed but whose overlay
fails to parse is a hard error — a policy that cannot be honoured must not be silently dropped.

**Precedence.** The synced layer fits the existing order without replacing it (highest first):

```
CLI flags → env (IMP_*) → project imp.toml → synced layer → user config → defaults
```

The argument is the one the existing order already uses — *more specific wins*. The synced layer is
a **machine** layer; a project `imp.toml` is a **workspace** layer, therefore more specific, and
stays above it; the user config is also machine-level but expresses a personal preference, and an
admin policy outranks it, so the synced layer sits above the user file. `sync` writes the merged
base+overlay to a machine-local `synced.toml` beside the user config (never committed, never part
of a `--project` write) with a sibling state file recording `{remote, ref, commit, files:sha256,
applied_at}`; `Config::load` then merges it like any other layer, so a machine that has never synced
has no file and behaves exactly as today. No database is involved. **D39**.

Two consequences are written down rather than hidden. Because the project file is above the synced
layer, **a cloned repository's committed `imp.toml` can override master policy** (for example
widen `[policy.allow]`); and because the synced layer is above the user file, **an operator cannot
override master policy in their own config**. If master policy should instead be a floor that
neither can loosen, that is a different mechanism — a "locked keys" list, not an order — and it is
flagged for the spec owner (§11, R10) rather than decided here.

**Observability is mandatory, not a nicety.** A sync nobody can inspect is magic, and magic is not
debuggable.

- `imp config sync --dry-run` fetches and verifies, prints the *effective* diff, and writes
  nothing:

  ```
  config sync: node "1050ti" (from hostname)
    channel  https://git.albruiz.dev/albruiz/imp-config.git @ 4f2a1c9 (ref main)
    verified 3 files, sha256 ok, commit ok
    synced layer (base.toml + nodes/1050ti.toml):
      + peers.big.url          nodes/1050ti.toml
      + peers.big.token_file   base.toml
      ~ policy.noninteractive  base.toml   "ask" -> "deny"   (shadowed by project ./imp.toml)
    dry-run: nothing written
  ```

- `imp config sync --check` resolves the remote commit and exits `1` when it differs from the
  applied one, writing nothing — the same shape as `imp update --check`.

- `imp config show --origin` names, for every effective key, the layer it came from:
  `default` / `user:<path>` / `synced:<commit> <file>` / `project:<path>` / `env:<VAR>` / `flag`.
  It is implemented by keeping each layer's `toml::Value` and walking them highest-first: a leaf's
  origin is the topmost layer that defines it. `--json` is the script form.

  ```
  provider.model         = "qwen2.5:14b"   [user:~/.config/imp/config.toml]
  peers.big.url          = "http://big…"   [synced:4f2a1c9 nodes/1050ti.toml]
  policy.noninteractive  = "deny"          [project:./imp.toml]
  workspace.roots        = ["."]           [default]
  ```

**Integrity, the same contract as `imp update` (D23).** The bundle carries `checksums.txt` with
the commit and a sha256 per config file. Before anything is applied, in order: **(1)** every file
hashes (computed in-tree, `imp_core::sha256`) to its entry, and **(2)** the commit the ref
resolves to equals the `commit` line. Any mismatch refuses the whole bundle — exit `4`, nothing
written (fail-closed). Two checks, not one, for update's reason: the **checksum** binds the bytes to
the manifest and the **commit** binds the manifest to a revision, so a bundle that swapped both
files and manifest still fails the commit check. Both are needed even though git hashes objects,
because git proves the objects it fetched and says nothing about a hand-edited cache, and it does
not bind a bundle read any other way to a revision. Applying is atomic: `synced.toml` and its state
file are written to a temp file and `rename()`d, so an interrupted sync never leaves a half-applied
layer. **D41**.

The honest limit, stated as the risk it is: **whoever controls the channel controls the effective
policy of every node.** The manifest lives in the repo, so a malicious publisher rewrites the files
*and* the manifest together and passes both checks — the residual is publisher trust, exactly the
limit D23 already carries, not a claim of supply-chain security. It is written up as **T15** and
**R10**. What keeps the blast radius bounded is §5.1: the channel can only ever distribute *names*,
never a secret value, so a compromised master distributes policy — it cannot exfiltrate keys.

**Relation to peers (M10).** The peer cohort is the clearest use case. Today `[peers.big]` is
repeated in every node's config, so adding a node means editing N files. With the master, the peer
lives once in `base.toml`:

```toml
# base.toml
[peers.big]
url = "http://big.tailnet.ts.net:8788/mcp"
token_file = "~/.config/imp/big.credentials"   # a path, never the token
approval = "ask"
```

Every node gains `peer__big_ask` after its next `config sync`; adding a node is one
`nodes/<name>.toml`; rotating the peer's endpoint is one line in the base. Only **names** travel —
`url` plus a `token_file` *path* or a `token_env` *variable name* — so §5.1 holds and the master is
never where the tokens live. Each node's own `0600` credentials file (or environment) supplies the
value; provisioning those values is out of scope, below.

**What does not enter.** **D42.**

- **Centralizing secret *values*.** That is the token-manager's job, a separate system. The master
  records names and paths, never values (§5.1, D9/D11). A master that held every node's key would
  be the single point of compromise this whole section exists to avoid.
- **Automatic node discovery or enrolment.** A node is in the repo because a human put it there.
  No tailnet scan, no self-registration, no "join" protocol.
- **Any permanently-running master service.** No daemon, no endpoint, no listener (D38).
- **A sync timer inside imp.** `sync` runs when it is invoked. A node that wants it periodic
  wires its own cron/launchd to call `imp config sync`; imp starts no background loop for it.

**Exit codes:** `0` up to date · `2` usage · `3` channel unreachable (remote, auth, or ref missing)
· `4` refused (integrity mismatch, or a listed overlay that will not parse) · `5` internal.
`--check` exits `1` when drift exists.

**Placement.** The rules — bundle shape, the manifest parse, verification, the overlay merge and the
origin walk — are pure and live in `imp-core` (they take bytes and return values, no I/O). The
one part that shells out to `git` lives in a new small crate **`imp-master`**, mirroring the split
`imp-update` and `imp-guard` already use to keep `imp-core` free of I/O; the subcommand is
`imp-cli/src/config.rs`. `[configsync]` is `ConfigSyncConfig` in `imp-core/src/config.rs`.

---

## 6. Security and threat model

### 6.1 Assets

API keys; filesystem contents; shell access; the SQLite store (may contain sensitive transcripts); the MCP channel (may be driven by an untrusted model).

### 6.2 Threats and mitigations

| ID | Threat | Mitigation |
|---|---|---|
| T1 | Prompt injection in fetched content drives `run_command` | Approval gate + risk classifier + never auto-`--yes` in a TTY; fetched content is tagged as untrusted in the system prompt |
| T14 | The System One guard is steered by the same injected content it exists to catch | The guard may only narrow prompts, never widen permissions (FR-43); the static category list is a floor the model cannot cross (FR-44), so an ineligible command is never sent at all; `state` is the command string alone (FR-46); every failure falls back to prompting (FR-45) |
| T2 | Path traversal / symlink escape | Canonicalize and prefix-check against workspace roots; `follow_symlinks = false`; re-check after open where feasible |
| T3 | SSRF via `http_fetch` | Domain allowlist, private-IP block, no cross-domain redirects, response cap, scheme restriction |
| T4 | Secret exfiltration via tools or logs | Keys never in the model context unless a tool explicitly returns them; env redaction in logs; audit stores digests |
| T5 | MCP server exposes shell to an untrusted model | `expose_exec`/`expose_write` default false; read-only default surface; startup banner states enabled capabilities |
| T6 | Malicious external MCP server | Per-server allowlists, per-server approval, no implicit trust; third-party output treated as untrusted |
| T7 | Resource exhaustion | Per-tool timeouts, output caps, iteration/token budgets, job concurrency cap, process-group kill |
| T8 | Cron job runs with escalated intent | Jobs always execute under non-interactive policy (fail closed) |
| T9 | TOCTOU between approval and execution | Approval binds to the exact argv being executed; any mutation re-triggers policy |
| T10 | Store tampering / corruption | WAL + integrity check at startup; migrations versioned; corrupted DB refuses to start with a clear message |
| T11 | `init` writes a config that leaks a credential | The value goes only to the separate `0600` credentials file; the config records env var *names* and paths, so `--project` output stays committable; the token is never printed, never logged, and never appears in `--json` |
| T12 | `init` silently overwrites a hand-tuned config | Existing target is diffed and requires `--force`; writes go through a temp file and rename, so no partial config is left behind |
| T13 | The API key leaks into terminal scrollback or a screen share | Echo is disabled *before* the prompt is printed, not after, closing the window in which a fast paste would be echoed; the guard restores the previous termios on drop, including on panic |
| T15 | The master config channel is the softest target in the fleet: whoever controls it controls the effective policy of *every* node | The bundle is verified before it is applied (per-file SHA-256 against a committed manifest, plus the commit the ref resolves to), fail-closed (D41, §5.14); the channel can only *name* secrets, never carry a value (§5.1), so a compromised channel cannot exfiltrate keys — it can only distribute policy; sync is an explicit pull, never a daemon, so nothing applies without a decision. The residual — a manifest that agrees with a malicious commit — is publisher trust, the same acknowledged limit `imp update` carries (D23) |

### 6.3 Principle of least exposure

For each capability, an operator should be able to answer "who can trigger this?" from the config alone. The SDD therefore treats every entry point as a separate trust boundary: REPL user, piped stdin, cron, and each MCP client are distinct.

---

## 7. Observability

| Signal | Mechanism |
|---|---|
| Structured logs | `tracing` spans: `turn`, `provider.request`, `tool.invoke`, `approval`, `cron.fire` |
| Log sinks | stderr (human), file (text or JSON), `--verbose` raises level to `debug` |
| Audit trail | `audit_log` table for every tool decision and outcome |
| Cost | `usage` per message; `/cost` aggregates per session |
| Cron history | `job_runs` with status and durations; `/cron log <job>` |
| Redaction | A `tracing` layer scrubs values matching configured env var names and `Bearer *` |

Implemented (M7) as D43: the audit row for a tool call is written once and carries the decision,
its outcome (`ok`/`error`/`denied`), the duration, and the session and turn it belonged to; `/cost`
aggregates per conversation from the `session_usage` table rather than per process; redaction is a
property of the log sink (`RedactingMakeWriter`), which masks the resolved key, the configured env
var's value, secret-shaped header values and any `Bearer` token before the line reaches stderr or
`[logging].file`. The measured NFR figures are in `NFR.md`.

---

## 8. Testing strategy

| Layer | Approach | Tools |
|---|---|---|
| Unit | Policy resolution, risk classifier, cron next-run, path guard, SSE parser, tool-call fragment assembly, diff output | `#[test]`, `proptest` |
| Provider | Fake HTTP server with scripted SSE streams, including malformed frames, mid-stream disconnect, 429 | `wiremock`, fixtures |
| Loop | Scripted provider emitting tool calls; assert message sequence, stop reasons, budget enforcement | in-memory store + mock provider |
| Tools | Real filesystem in temp dirs; process kill on timeout; output cap boundaries | `tempfile`, `assert_cmd` |
| Store | Migration up/down, concurrent writers, WAL recovery after simulated crash | `rusqlite` temp DBs |
| Cron | Virtual clock; assert catch-up policies, overlap handling, DST boundaries | injected clock |
| MCP | Our server driven by `rmcp` client; external server driven by a stub; golden tool lists | `insta` snapshots |
| REPL | Approval prompt flows, non-TTY fail-closed, `--json` event stream, `!` escape | `assert_cmd`, `insta` |
| E2E | Temp workspace + stub provider + real binary; scripted multi-turn scenario | shell-driven harness |

**Determinism:** all time is injected via a `Clock` trait; all randomness (jitter, UUIDs) is seedable in tests. No test requires network access.

---

## 9. Packaging and distribution

- `cargo build --release` produces one binary; a release workflow builds the platform targets and publishes them as assets of a tagged GitHub release, with a `checksums.txt` alongside. (The workflow itself is a separate deliverable; this section states the contract it must meet.)
- **Release pipeline.** `.github/workflows/release.yml` runs on a push to `main`. A single `gate` job derives the next `vX.Y.Z` from the newest tag, refuses to publish unless that equals `[workspace.package].version`, and runs `cargo +1.89.0 test --workspace --locked`; a `build` matrix then produces the six assets — `{linux, macOS, Windows} × {amd64, arm64}` — each carrying the commit and date embedded (`IMP_GIT_SHA`/`IMP_GIT_DATE`), and a `publish` job hashes all six into one `checksums.txt` and attaches the seven files to a GitHub Release, verifying the release is not left as a draft and that the uploaded manifest is the one hashed. Each target is built on a runner that can also *run* it and the built binary is executed with `--version` to assert both stamps before it is published, so no architecture goes out unverified (D28). The six asset names are exactly the ones `imp-core::update::asset_for` looks up (§9.1): a name published differently is a 404 to `imp update`, not a fallback, and a unit test pins the table so the two cannot drift. The tag, the version the binary reports, and the assets are the three things `imp update` consumes; the workflow fails rather than publishing a set that disagrees. The procedure, and how to cut a tag by hand, are in README → Releasing. The Linux assets are gnu rather than musl (D24); the per-target runner choices are D28.
- Homebrew tap, `cargo install imp-cli`, and a curl installer script.
- **Version reporting.** `imp --version` prints the workspace version, the git SHA and commit date the binary was built from, and the capability families compiled in:

  ```
  imp 0.1.0 (f7581dae470a 2026-10-07) [features: cron,guard,mcp,update]
  ```

  The stamps are embedded at compile time (`imp-cli/build.rs`), so `--version` needs no git, no network and no config, and it degrades to `unknown` rather than failing when git is absent. `imp update` reads the same line back out of a downloaded binary to tie it to a commit.
- Config/db paths created on first run with `0600`/`0700` permissions.

### 9.1 `imp update`

`update` is the self-update path: it asks a release channel what the latest version is and, when it is newer, downloads the prebuilt binary for the current platform, verifies it, and replaces the installed one.

**Channel.** A GitHub-compatible releases API, configured under `[update]`:

```toml
[update]
api_url = "https://api.github.com"   # or a mirror; https, or http only on loopback
repo = "Az107/imp"                # owner/name
asset_prefix = "imp"              # assets are `<prefix>-<os>-<arch>` and `checksums.txt`
```

No token is sent: the repository is public. A `401`/`403`/`404` is reported as "this works without a token only for a public repository" rather than retried, and authentication is deliberately not implemented (D23).

**What it installs.** Only the asset whose name is `<prefix>-<os>-<arch>` for the running platform (`linux`/`darwin`/`windows` × `amd64`/`arm64`). A release without that asset is a refusal, not a best-effort choice among the others.

**Verification, in order, before anything is replaced:**

1. The `checksums.txt` asset of the *same release* is fetched and the downloaded binary's SHA-256 must match its entry; a mismatch aborts with the binary untouched. The hash is computed in-tree (`imp-core::sha256`) so the check adds no dependency.
2. The commit the release declares (`target_commitish` as a hex SHA, or a `build-commit:` line in the notes) must match the SHA the downloaded binary reports for `--version`. A release that declares no commit is refused rather than trusted, and a binary reporting a different commit is not installed.

**Replacement.** The verified bytes are written to a staging file beside the target and moved over it with `rename()`, which is atomic and works for a running binary on Linux. The previous binary is kept as `imp.old-<version>`, which `--rollback` restores (keeping the replaced one in turn). The target is `current_exe()`, canonicalized, or `$IMP_UPDATE_BINARY` when set. Nothing else is touched: not the config, not the database, not the credentials file.

**When the directory is not writable**, imp does not elevate. The verified binary is staged under the temp directory and the operator is handed the two literal commands to run with `sudo` (back up first, then install), exiting `4`.

**Consent.** Without `--yes` and without a terminal, nothing is installed: the same fail-closed rule as the rest of the tool. On a terminal the release notes are shown and one `y`/`yes` proceeds.

**Flags.** `--check` reports installed/remote/asset and writes nothing (`0` up to date, `1` update available); `--force` reinstalls the current version; `--rollback` restores the most recent backup without contacting the channel; `--json` emits one machine-readable object. `--yes` is the global consent flag.

### 9.2 Build, packaging and private-file modes

Implemented (M7): `cargo build --release` produces one binary (`lto = "thin"`,
`codegen-units = 1`, `strip = true`); `make dist` builds the per-target tarballs and
`.sha256` that `install.sh` consumes; `imp --version` reports the git SHA and the
enabled features from `build.rs` (which watches `.git/refs`, `.git/logs/HEAD` and
`.git/packed-refs` so the SHA tracks the commit, not just `.git/HEAD` — the suite
asserts the banner against `git rev-parse`); `write_private_file` and `Store::open` set
`0600`/`0700` before writing content, and `imp doctor` warns when a private file is
not owner-only. A musl C compiler is required for the static Linux targets — see
`NFR.md` for the no-root recipe used here.

---

## 10. Milestones

| Milestone | Status | Scope | Exit criteria |
|---|---|---|---|
| M0 — Skeleton | done | Workspace, config loading, provider client, `run` one-shot | A prompt returns streamed text from a compatible endpoint |
| M0.5 — Provider init | done | `imp init` wizard, presets, `/models` discovery, `--check`, `--project`/`--force`, `--header`, session-id headers | A fresh machine reaches a working config in one command, an existing config is never clobbered, and a gateway requiring `${session}` headers works unmodified |
| M1 — Session core | done | Store, migrations, sessions/messages, REPL with streaming | `/resume` restores a conversation |
| M2 — Tools + policy | done | Registry, `read_file`/`write_file`/`edit_file`, `run_command`, approval engine, allowlist | A risky command cannot run without consent |
| M3 — Memory + HTTP | done | `remember`/`recall`, `http_fetch` with allowlist | FTS recall works; SSRF guard tested |
| M3.5 — System One guard | done | Optional `/v1/systemone` judge for flagged `run_command`, two thresholds, category floor, audited verdicts | An ineligible command never reaches the model; every failure path prompts; a guard that returns `Err` cannot produce an allow |
| M4 — Cron | done | Scheduler, job CRUD, run history, catch-up | A weekly job fires on a virtual clock test |
| M5 — MCP client | done | External servers, namespaced tools, per-server policy | External tool callable with approval |
| M6 — MCP server | done | `mcp serve` with read-only default surface and opt-in exec/write | Another model drives `agent_ask` end to end |
| M7 — Hardening | done | `--json`, `doctor`, audit log, redaction, packaging, docs | NFR targets met and measured (`NFR.md`); installers written (`install.sh`, `make dist`). Uploading a tagged release is the remaining step and needs a tag on `main`, which is the owner's |
| M8 — Self-update | done | `imp update`: release channel, checksum and commit verification, atomic replace, `--rollback`, and `--version` carrying the git SHA | An installed binary fetches, verifies and replaces itself from a published release; a bad checksum or an unverifiable release changes nothing; `--check` writes nothing |
| M9 — Release pipeline | done | GitHub Actions workflow: version derivation from tags, tag/`Cargo.toml` agreement gate, test gate, `linux/amd64` + `linux/arm64` via gcc cross, `checksums.txt`, GitHub Release | A push to `main` publishes a coherent release (tag, embedded version and assets agree) that `imp update` installs; a mismatched version or a failing test publishes nothing |
| M9.1 — Six targets | done | The release pipeline covers `{linux, macOS, Windows} × {amd64, arm64}`: macOS and Windows on their own runners, Windows/arm64 native, every built binary run to assert both stamps, and one `checksums.txt` over the six named assets (D24, D28) | A push to `main` publishes all six assets under the names `imp update` looks up, each carrying the version and commit it was built from, and the uploaded `checksums.txt` covers the six |
| M10 — Peer transport | done | Streamable HTTP for both halves: `url` on `[mcp.client.servers.*]` with bearer-token auth (`token_env`/`token_file`), `transport = "http"` + `bind` on `[mcp.server]`, a fail-closed bind policy, no TLS by design | Two imp instances on a tailnet discover and call each other's gated tools over HTTP; a non-loopback bind with no token refuses to start; a wrong token lists nothing |
| M10.2 — Peer delegation | done | One `peer__<name>_ask` tool per `[peers.*]` entry, carrying a *brief* to the peer's `agent_ask`; a configurable result cap (~8 KB), the read-only depth cap, remote usage folded into the turn, and the whole thing behind the `Network` gate | A small model escalates a self-contained brief to a bigger peer through the gate and gets its answer as tool-result data; a cron job cannot escalate; the gate decides before the network is touched; a peer that fails or goes missing is a tool error, never a panic or a hang |
| M10.3 — Small-model loop ergonomics | done | A trimmable tool surface (`[tools] only`/`hide`), recovery from a tool call cut off by `finish_reason: length`, per-turn dedup of repeated calls, `agent.max_tool_calls_per_turn` with a `tool_budget` stop, numbered imperative rules behind `agent.small_model`, and strict tool arguments behind `[provider] strict_tool_arguments` | A 2–4B model drives a turn end to end without a parse error or a runaway loop: a truncated call is discarded and re-asked, a repeated call runs once, the budget ends the turn with every `tool_call_id` answered, and every knob is inert unless set |
| M11 — Master config node | proposed | A master as a **git repo, not a service**: `imp config sync` converges a machine-local synced layer (`base.toml` + `nodes/<name>.toml`) from a Forgejo repo, under the D23 integrity contract, slotted into the existing precedence, with `--dry-run` and `config show --origin`. Design only (§5.14, D38–D42); no code in this milestone card | A node behind NAT — or one not yet on the tailnet — converges its config by *pulling*; a manipulated bundle is refused and nothing changes; adding a peer cohort member is one edit to the repo, not N node configs |

*Note (integration, M7 fold):* the M7 row is `m7-hardening`'s own claim, merged here. The NFR figures
in `NFR.md` were measured on the **M6+M7** binary, before M8–M11 put `axum`, `reqwest`/`rustls` and
`imp-update` in the release path, so they describe that artifact rather than this one and are
pending re-measurement — NFR-3 (static musl size, 14.58 MB against a 15 MB budget) is the one to
check first, because the C in `aws-lc-sys` is why D24 publishes gnu instead of musl.

---

## 11. Risks and open questions

| ID | Risk / question | Proposed handling |
|---|---|---|
| R1 | Command risk classification will both over- and under-block | Keep the classifier small, documented, and testable; make it easy to override with allowlist entries; log every classification |
| R9 | A model-based judge inherits the injection surface it guards (T14) | Treat the guard as a noise filter inside a boundary that static code drew without it: it resolves prompts, never permissions. Disabled by default, and a vendor-documented adversarial-content failure mode is assumed rather than disproved |
| R2 | `run_command` is inherently unsandboxable in-process | Ship v1 with approval + allowlist; add optional `sandbox-exec`/`bwrap` execution mode later behind config |
| R3 | History summarization can lose critical detail | Mark summaries explicitly; keep the full transcript in SQLite; allow `/compact off` |
| R4 | Provider drift across OpenAI-compatible endpoints | Keep the client thin, expose quirk flags, and add a compatibility test matrix |
| R5 | MCP server + REPL contend for stdio | Mutually exclusive modes; `mcp serve` never starts a REPL |
| R6 | Cron in-process means jobs don't run when imp is closed | Documented; optional `--system` crontab/launchd integration deferred to v2 |
| R7 | `always` allowlist could persist an over-broad pattern | Show the exact pattern before persisting; require explicit confirmation; cap pattern length and forbid wildcards alone |
| R8 | Token/cost accounting differs per provider | Treat usage as advisory; never block a turn solely on a missing usage field |
| R10 | The master channel is the fleet's single point of policy injection, and the chosen precedence lets a cloned project — and blocks a local user — override master policy | The bundle is verified before it is applied, fail-closed (D41), and the channel carries names, never secret values (§5.1), so a compromised master distributes policy but cannot exfiltrate keys; recorded as T15. The residual (a manifest that agrees with a malicious commit) is publisher trust, the same limit D23 carries. Whether master policy should instead be a *floor* a project or user cannot loosen — a "locked keys" mechanism with its own decision — is left open for the spec owner |

**Open questions for review:**
1. ~~Should `http_fetch` support an explicit `http://` for local dev servers via a named allowlist
   entry, or require HTTPS unconditionally?~~ **Resolved — a *named, exact* allowlist entry.**
   `https` is required unless the exact host (no wildcard) appears in `allowed_domains`, so a local
   dev server is reachable without weakening the default; a wildcard entry grants `https` only.
   Matches §5.5's guard row. Implemented (D17, which also records the separate, stricter handling of
   §5.5's approval row).
2. ~~Should job prompts be able to opt into the interactive policy when imp is attached to a TTY, or always fail closed?~~ **Resolved — always fail closed.** A job's prompt is not a person at a keyboard, even when the scheduler happens to be ticking inside a REPL that has one. Its gate is built with `interactive = false` and no approval UI at all, so a tool the policy would `ask` about is decided by `policy.noninteractive` (`deny` by default) and a job cannot promote it to `auto`. A prompt that appeared at an unrelated moment would be approved by whoever was mid-keystroke, which is exactly the consent that means nothing. Deny rules and allowlists still run first, so an allowlisted command runs unattended; the guard is not consulted either, because it only ever resolves a prompt and there is none. See D19.
3. ~~Is `edit_file`'s exact-match semantics sufficient, or is a patch-based tool wanted for large
   edits?~~ **Resolved — both are provided.** `edit_file` stays for a single surgical replacement;
   `apply_patch` handles multi-site and multi-file edits in one atomic call. See §5.5 and D13.
4. ~~Should memory be scoped per-workspace (proposed) or global with a namespace parameter?~~
   **Resolved — per-workspace.** The `namespace` column is a hash of the canonical workspace root, so
   no parameter is needed on either tool and facts cannot leak between projects. Matches §5.5.
   Implemented.

---

## Appendix A — Tool schema on the wire

`ToolSchema` is a domain value — `{name, description, parameters}`. It is **not** what goes on the
wire. Each entry of the request's `tools` array is wrapped in the function envelope, and a compliant
provider answers a flat entry with `400 invalid request`:

```json
{
  "type": "function",
  "function": {
    "name": "run_command",
    "description": "Run a shell command in the workspace. Requires approval unless the command matches an allowlisted pattern. Output is captured and truncated.",
    "parameters": {
      "type": "object",
      "properties": {
        "command":    { "type": "string", "description": "Shell command line to execute." },
        "cwd":        { "type": "string", "description": "Working directory, relative to a workspace root." },
        "timeout_ms": { "type": "integer", "minimum": 1, "maximum": 3600000, "default": 120000 },
        "stdin":      { "type": "string", "description": "Optional stdin for the process." }
      },
      "required": ["command"],
      "additionalProperties": false
    }
  }
}
```

`schemars` supplies `parameters` from the Rust argument struct. Note that it also emits `$schema`,
`title`, `format`, and — for `Option<T>` — a union such as `"type": ["integer", "null"]`. The
OpenAI endpoint accepts these, but providers that translate the schema into another vendor's format
may not; if a gateway ever rejects a tool, normalize the schema at the registry boundary rather
than editing individual tools.

## Appendix B — Request/response shape used against the provider

```jsonc
// POST {base_url}/chat/completions
{
  "model": "gpt-4.1-mini",
  "stream": true,
  "stream_options": { "include_usage": true },
  "temperature": 0.2,
  "tools": [ /* Appendix A shape */ ],
  "tool_choice": "auto",
  "messages": [
    { "role": "system", "content": "You are imp…\nWorkspace: /Users/me/proj (read-write)\nMode: interactive\nTools: …\nDenied: run_command *sudo*" },
    { "role": "user", "content": "list the largest files here" },
    { "role": "assistant", "tool_calls": [
        { "id": "call_1", "type": "function",
          "function": { "name": "run_command", "arguments": "{\"command\":\"du -sh * | sort -h | tail\"}" } } ] },
    { "role": "tool", "tool_call_id": "call_1", "content": "{\"exit_code\":0,\"stdout\":\"…\"}" }
  ]
}
```

## Appendix C — Decision log

*Decision numbering across the M10/M11 branches.* M10.1 (peer transport) took **D25–D27**, M9.1 (six-target matrix) **D28**, and M10.2 (peer delegation) **D29–D31**. M10.3 (small-model ergonomics) was authored on a parallel branch that numbered its six decisions **D29–D34**, colliding with M10.2 at D29–D31, and M11 (master config node) was written as **D35–D39**. When the branches met on the integration branch `integration/pre-rename`, the M10.3 decisions were renumbered to **D32–D37** and the M11 ones to **D38–D42**, so the log below is a contiguous D1–D42 with no repeated id and every cross-reference pointing at its own entry.

| # | Decision | Rationale |
|---|---|---|
| D1 | Rust over Zig | Mature async, SQLite, and MCP ecosystem; Zig's async and DB story would dominate the schedule |
| D2 | Line REPL over full-screen TUI | Matches fx and Unix composability; less code; screen readers and pipes work |
| D3 | Approval + allowlist over sandbox-first | Sandboxing is not portable; approval policy is auditable and testable on all platforms |
| D4 | SQLite over JSONL | Cron needs queries and transactional writes; single file stays portable |
| D5 | In-process scheduler over system crontab | Jobs stay portable and inspectable; no OS-specific install/uninstall |
| D6 | MCP both directions | Serving lets other models use minion; consuming lets minion use the wider MCP ecosystem |
| D7 | Thin hand-rolled provider client | Full control over base URLs, streaming, quirks; avoids SDK lock-in |
| D8 | Read-only MCP surface by default | Exec and write over MCP are remote code execution surfaces; opt-in only |
| D9 | `init` writes only env var names and targets the user config by default | Keeps credentials out of the repo; `--project` is opt-in for shared, secret-free setups. Extended, not replaced, by D11, which adds the credentials file for the *value* while keeping this rule for the config |
| D10 | One generic `[provider.headers]` table with a `${session}` placeholder, instead of a built-in OpenCode-Go-specific header | Any gateway requirement is expressible in config; no provider-specific branches in the client. The placeholder is the only way an id can reach a header, so a stable per-conversation id cannot be accidentally re-generated per request |
| D11 | The API key is collected by prompt and stored in a separate `0600` credentials file, while the config keeps the `api_key_env` path | Removes the "find a variable name and export it yourself" step, which was the whole friction of the old flow, without putting a secret in a file people commit. A `netrc`/AWS-CLI style split: portable config, local secret. Resolution order keeps env var → file so a shell or CI job can still override |
| D12 | Terminal echo is disabled before the prompt is printed rather than by the library that reads the secret | `rpassword`-style helpers print first and disable second, which leaves a window where a pasted key is echoed into scrollback. Owning the termios guard is ~20 lines and closes it; `--no-store-token` keeps an escape hatch for anyone who does not want a secret on disk at all |
| D13 | `apply_patch` takes structured JSON operations, and sits *alongside* `edit_file` rather than replacing it | Anchored operations are unambiguous: "anchor matched 2 times" is an exact, actionable error, where a fuzzy hunk match silently edits the wrong lines. `edit_file` stays because for a one-line change it is the smaller, less failure-prone request |
| D14 | Assistant text is rendered as markdown on a terminal, per block, with the parser from `pulldown-cmark` and the renderer written in-tree | A terminal has no font sizes, so hierarchy is bold plus a colour that steps with the heading level; the scrollback rule is kept by emitting whole blocks and never revising text already on screen, which streaming token-by-token cannot do for a table. Rendering only when stdout is a terminal keeps `minion run ... > out.md` yielding markdown rather than ASCII art, and the split of layout from colour means `NO_COLOR` still gets aligned tables. The renderer is ours so the output is exactly the intended one and the dependency stays a single parser, which is what keeps the binary inside NFR-3 |
| D15 | The non-interactive rule governs tools that *change* something; `ReadOnly` is allowed unattended and `Network` counts as a change | §5.6 lists `ReadOnly → Auto` above the `!stdin.is_tty()` branch, but the implementation checked the TTY first, so with the default `noninteractive = "deny"` a read was refused whenever stdin was piped — which breaks the `minion run ... > out.md` invocation this document advertises, for `read_file` as much as for `recall`. A missing terminal says something about *consent*, and a read cannot consume consent it never asks for; what it genuinely blocks is an unattended mutation. `Network` is deliberately on the mutation side: the request leaves the machine and can be induced by untrusted content in a transcript, so `http_fetch` is gated like a write and still needs `--yes` or an allowlist entry in a pipe. Deny rules, allowlists and the classifier are untouched and still run first, so this widens nothing that was explicitly refused. Recorded because it touches the rule order that the invariants call a security property, even though it only restores what §5.6 already specified |
| D16 | A System One model may resolve a flagged `run_command` prompt, but only for categories the static classifier found *reversible*; it can narrow prompts and never widen permissions | The static classifier cannot distinguish a plain `curl` from a `git push --force`: both are one tag, so both always ask. That is safe but noisy, and noise trains a user into approving without reading. A typed-probability model separates them — `POST {base}/v1/systemone` returns calibrated numbers rather than text, so the decision is a threshold applied in our code and cannot be talked into by a model that simply asserts it is safe. The model is explicitly *not* a new authority. TypeSafe's own notes for `jev-1.13` state that adversarial content in `state` "can move the answer", and the command being judged may itself have come from injected content (T1, T14), so the boundary is inverted: the heuristic category list is a floor that ineligible commands never cross, and the model only ever operates strictly inside it. `privilege`, `remote-execution` and `destructive` are resolved before any network call, so `sudo rm -rf /` has no code path to a model verdict. Two thresholds rather than one, because a single cut discards the middle band where a sceptical model is saying something worth hearing, and every failure — timeout, `429`, withdrawn free tier, unparseable body — falls back to the existing prompt. Only the command string is sent as `state`, both because the vendor documents context rot on unrelated detail and because a minimal `state` shrinks the injection surface. Disabled by default: it is a network call carrying command text to a third party, and it is opt-in until it has been seen behaving. Specified against the `/v1/systemone` protocol rather than one vendor, since Jev, Laya, Kev, Decider, Von and OpenThai-SystemOne all speak it and a client only changes `base_url` — noting that Laya's state budget is ~320–512 tokens against Jev's 32k and its `confidence` is computed differently, so thresholds do not transfer between them. Implemented in M3.5: `minion-core::guard` holds the static category floor, the two thresholds and the `SystemOneGuard` trait; `minion-guard` is the `/v1/systemone` client (`{"state": command}` in, `{"unsafe": p}` out); the engine consults it only where a prompt would otherwise be shown, so a non-interactive run still resolves to `policy.noninteractive` |
| D17 | `[http_fetch].allowed_domains` is the reachability guard, kept separate from the approval allowlist — a deliberate deviation from §5.5's approval row, pending the spec owner's confirmation | §5.5 states the guard ("domain must match `allowed_domains`") and the approval rule ("`auto` if domain allowlisted, otherwise `ask`") as if one list drove both. Read as one list, the approval row's `otherwise ask` clause can never fire — the guard refuses a host outside the list before approval is reached — so the only operative rule would be "`auto` if allowlisted". This implementation keeps the two mechanisms apart: `allowed_domains` is the reachability boundary and the SSRF mitigation T3 names, while `[policy.allow]` is what skips the prompt, matched against the URL host because the tool reports the host as its approval subject. The result is stricter than §5.5's row, not looser: an allowlisted domain still prompts on a TTY and is still refused in a pipe (D15) until it is given a policy allow rule or `--yes`, and nothing added for reachability is silently auto-approved. The alternative — letting the guard's list also drive approval — would either reduce the SSRF boundary to a mere prompt or auto-approve every domain added to `allowed_domains`, including in a non-interactive run. Recorded as a deviation from §5.5's approval row for the spec owner to confirm or overrule. |
| D18 | A job's `schedule` is a five-field **Vixie** cron expression, translated into the parser's six-field Quartz grammar before it is parsed | The `cron` crate's grammar is Quartz-flavoured in two ways that silently change what an expression means. It puts seconds first, so a five-field string is prefixed with `0 ` — every SDD expression fires on the zeroth second of its minute, which is what a five-field cron means everywhere else, and a user who types six fields is told so rather than quietly getting a different schedule. And it numbers the days of the week `1` = Sunday, where every other cron numbers them `0`/`7` = Sunday and `1` = Monday: left alone, `0 9 * * 1` fires on Sunday, a day out from what the user wrote. The day-of-week field is therefore rewritten element by element (plain numbers, ranges, lists, and stepped forms), a bare `*` or `*/n` is left alone because a wildcard's offset is uniform so the set of days is identical in either numbering, and names (`mon`) pass through because both conventions agree on them. A range that would wrap (`6-1`) is refused instead of being silently reordered. One further deviation is documented rather than fixed: when *both* day-of-month and day-of-week are restricted, the parser ANDs them where Vixie ORs them, so `0 9 1 * 1` means "the 1st, if it is a Monday" — an expression that rare is better left explicit, and pretending otherwise would need a scheduler of our own |
| D19 | A cron run always takes the non-interactive policy, and a fire consumes its occurrence | §11's open question 2 is settled as fail-closed, and the reason is the same one D15 gives: consent requires a person who is being asked. A job prompt has no terminal even when the scheduler is ticking inside a REPL that has one, so its gate is built with `interactive = false` **and no approval UI**, which makes `policy.noninteractive` the deciding rule for anything that changes something and leaves deny rules and allowlists in front of it untouched. A job cannot promote an `ask` tool to `auto`, cannot reach the System One guard (there is no prompt for it to resolve), and cannot create or delete other jobs unless an allow rule names it — the three properties the milestone's tests pin. The fire path is the other half: the occurrence is consumed when it fires, not when it finishes, because `next_run_at` recomputed only on completion would leave a job whose run outlasts its own interval looking due on every tick, and every tick would then record an overlap. Completion recomputes from the cron expression and the completion instant, which can only move the value later, so the "on completion" sentence in §5.7 and the fire-time advance agree instead of fighting. `missed_run_policy` stays a `[cron]` setting rather than a per-job column, since the schema has none and inventing one means a migration for a knob nobody has asked to vary per job; catch-up replays occurrences without the overlap check, because a replayed occurrence never ran and there is no live run to collide with, while `max_concurrent_jobs` and `missed_run_cap` still bound the burst. A `reuse` job's session is adopted in a *separate* statement from the run's terminal status, because `jobs.session_id` is a foreign key and an adoption that fails must not roll back the record that the run finished |
| D20 | The tool catalogue the model sees is read live from a `ToolCatalog`, and `lazy` defers a server's spawn to the first turn rather than hiding its tools | §5.10 asks for two things that a registry frozen at session assembly cannot both have: the servers are "spawned at startup", and a server that failed is "retried on the next turn". A `Vec<Box<dyn Tool>>` built once can do neither — a retry that cannot re-add a tool is not a retry. So the registry keeps its registered tools and *consults* a catalogue on every read of the tool list; an MCP failure adds or removes entries in a structure that changes under the frozen `Arc<ToolRegistry>` every caller already holds. That also settles shadowing by construction rather than by convention: registered tools are enumerated first and a name already present is skipped, so a server cannot publish a name a built-in owns, and two servers cannot fight over one. `lazy` is the one place where a literal reading had to be traded for a useful one. "Spawned lazily on first use" cannot mean "hidden until a tool is called", because a tool cannot be called before it is listed and a server cannot list tools before it is spawned — the two requirements would deadlock. It therefore means the spawn is deferred from session assembly to the first turn: opening a session (or `minion mcp list`, which is an explicit ask) does not start a process, and the tools reach the catalogue from that point rather than being invisible. The alternative — lazy servers reachable only through a generic `mcp_call` — was rejected because it moves the policy decision off the tool name the per-server policy keys on. `mcp_call` itself is deferred for that reason; the flattened tools are the surface §5.10 describes as primary |
| D21 | An external tool is always `Risk::Network`, and a server's `policy` substitutes the global default *and* the non-interactive decision for its own tools | §5.5 says an external tool inherits the target's declared risk with `Network` as a floor. MCP declares risk only in `ToolAnnotations`, and the protocol's own documentation says a client must not make tool-use decisions from them — they come from a server the operator has not vouched for (T6). Since the floor is `Network`, an inherited hint could only ever *lower* the class, never raise it, so the floor is the whole rule and the annotations are ignored. The consequence is deliberate: every external call is gated at least as strictly as an outbound HTTP request, and D15 puts `Network` on the side of the line that a missing terminal refuses. The per-server policy then has to mean something for an unattended run, or "a trusted local server can be `auto`" would be false the moment a pipe is involved. It therefore substitutes both fallbacks — `policy.default` and `policy.noninteractive` — and nothing else: deny rules, allow rules and the command classifier still run first and still win, so a family marked `auto` cannot resurrect a refusal that already happened, and a family marked `deny` refuses its tools outright. This is the trust boundary §6.3 describes applied literally: each MCP client is its own entry point, and the config states its policy in one place |
| D22 | The MCP server's disabled capabilities are *listed and refused*, not absent, and the exposed shell tool is decided under the name `run_command` | §5.9's table says `agent_run_command` and `agent_write_file` are "denied unless" the flag is set, and T5's mitigation is a startup banner — both describe a capability that exists and is refused, not one that is missing. A hidden tool would leave the host unable to tell "minion cannot do this" from "I mistyped the name", and it would make the denial invisible in the audit trail. So the two tools are always in the list, and without their flag the engine carries a deny rule that refuses every call and records the refusal. The name the *gate* sees is the real tool's — `run_command`, `write_file` — because the policy engine is not a second, weaker world: an operator's existing `run_command` deny rules keep protecting the MCP surface, the classifier still sees the command it is judging, and `subject_for`/`pattern_for` still extract the right subject. The `agent_` prefix is a naming convention of this surface, not a second identity, and the audit row therefore names the tool that actually ran. The gate is non-interactive (`stdin` is the protocol pipe, so no prompt can be shown), and an enabled family substitutes the non-interactive fallback through `ToolPolicy` (D21) — deny and allow rules still run first, so turning a flag on grants nothing that was refused. `agent_ask`'s inner agent is restricted to the read-only subset of the built-in registry, filtered by `Risk::is_observation` rather than by a list of names, so the default surface cannot drift wider than the read/write line itself. The scheduler is deliberately not started here: a job created over MCP is stored and runs under the non-interactive cron gate the next time a process with a scheduler opens the database, which keeps `mcp serve` a protocol server rather than a daemon |
| D23 | `minion update` installs a **prebuilt binary from a release channel**, never compiles on the target, and verifies it with two independent checks before replacing anything | The alternative — `git pull` and `cargo build` in place — was rejected because it turns an update into a build environment problem: it needs a toolchain on every machine, it needs a writable checkout, and a failure halfway leaves a half-built tree rather than a known-good binary. A prebuilt asset is one file that either matches or does not. The channel is a GitHub-compatible releases API with prebuilt per-platform assets and a `checksums.txt`, which is the shape `Az107/space-elevator` already publishes, so there is a working precedent to mirror. Two checks, not one, because each closes a different hole: the **checksum** binds the bytes to the release, and the **commit** binds the release to a revision, so a swapped asset that comes with a swapped manifest still fails the commit check. The commit is read from `target_commitish` when it is a hex SHA, or from a `build-commit:` line in the notes otherwise; a release that declares neither is refused rather than installed on a guess, which is the fail-closed reading of §5.12's "refused" exit. The hash is SHA-256 implemented in-tree (`minion-core::sha256`) rather than taken from `sha2`: the algorithm is 60 lines, the tests pin the NIST vectors, and it keeps `Cargo.lock` free of a new external crate, which matters for a binary whose whole selling point is a small dependency surface. The replacement is a staging file plus `rename()` so the path is never a half-written file, and the previous binary is kept as `minion.old-<version>` — which is also what `--rollback` restores, so undo does not need the network. Nothing else is touched: not the config, not the database, not the credentials file, because an update replaces exactly one file and anything more would make it an installer with opinions. When the directory is not writable, minion does not elevate and does not silently skip: it stages the verified bytes where it can write and prints the two literal `sudo` commands, matching the rule that Hermes never runs `sudo`. Two smaller decisions are recorded here too. `https` is required, with plain `http` permitted only to a literal loopback host — the exception exists for a local mirror and a test fixture, and it mirrors the rule `http_fetch` already uses (D17). And the target defaults to `current_exe()` canonicalized, overridable with `$MINION_UPDATE_BINARY`, so an update can replace an installation other than the one currently running without becoming a privileged operation: whoever can set the environment already runs the code. `--check` stays read-only, which is why it exits `1` on "update available" rather than `0`: a script asking "should I update?" needs the answer in the status, not in the prose |
| D24 | The release pipeline publishes **gnu Linux binaries for amd64 and arm64** (amd64 native, arm64 cross-compiled with `gcc-aarch64-linux-gnu` on the runner) alongside the four macOS and Windows assets, derives the tag from the newest `vX.Y.Z` tag, and refuses to publish unless that tag equals `[workspace.package].version` | §9 asks for musl static on Linux, and this deviates: the binaries are gnu and dynamically linked. The reason is the dependency tree. `minion-store` bundles SQLite and rustls pulls `aws-lc-sys`, both of which compile C through `cc` and cmake, so a static musl build is a cross-toolchain problem — a musl C compiler for each architecture — rather than the one-file build musl is for a pure-Rust crate. The gnu path builds with the runner's own toolchain and only adds `gcc-aarch64-linux-gnu` for the arm64 link, which is the option the milestone named; musl is deferred until it can be exercised end to end rather than guessed at. The cost is a glibc floor: the job is pinned to the `ubuntu-22.04` runner (glibc 2.35) instead of `ubuntu-latest`, so the floor is stable rather than moving when GitHub rotates the image, and the failure mode is bounded — a binary that cannot start fails the `--version` commit check inside `minion update` and the install is refused, not half-applied. The Linux pair is published gnu; the four macOS and Windows assets are built by the same pipeline on the runners D28 describes, and the six names together — `minion-{linux,darwin,windows}-{amd64,arm64}` — are what `minion update` can consume. The tag is derived from the newest tag rather than from the manifest because that is the rule the `Az107/space-elevator` precedent publishes with, but the manifest is what the binary reports and `minion update` compares that to the tag, so the two must agree or the workflow fails: a release whose tag and embedded version disagree would make `minion update` offer the same release forever. |
| D25 | MCP gains a **Streamable HTTP** transport for both halves, and a client server is `command` (stdio) *xor* `url` (HTTP) | Two minion instances on the same tailnet need to talk to each other, and today both halves are stdio-only: `transport-child-process` for the client, `mcp serve` owning stdin/stdout for the server. `rmcp` 3.5.0 already ships both the Streamable HTTP client (`transport-streamable-http-client-reqwest`) and server (`transport-streamable-http-server`), so the transport is adopted, not invented — no second protocol, no hand-rolled framing. The two client paths are mutually exclusive on purpose and checked at config load: a `command` is spawned over stdio, a `url` is spoken to over HTTP, and "both" or "neither" is a startup error rather than a server that quietly picks one; `args` beside a `url`, or a scheme that is not `http(s)`, is refused for the same reason. The server half is a config value (`transport = "http"`) rather than a flag because it changes what resource the process owns — a socket where stdio was a pipe — but R5's exclusion still holds: neither path can reach the REPL. Everything above `McpClient` is untouched, so listing, `tool_allow`, the per-server policy, the transition notices and the next-turn retry are the stdio behaviour verbatim; a test proves a down HTTP peer degrades exactly as a child that failed to spawn does |
| D26 | HTTP auth is a **bearer token**, never the source address, and there is **no TLS** — deliberately | Inside a tailnet the traffic is already encrypted by WireGuard, so TLS would add a dependency on the Tailscale daemon and on certificates to a binary whose whole selling point is a minimal dependency surface, and it would buy nothing against the threat that matters here. The real authentication is the token: any process on a tailnet node can open that port, so the source IP proves nothing about who is calling, and the `Host` header is no better — off a loopback bind the `Host` allowlist is switched off precisely because a MagicDNS name cannot be derived from an `IP:port` bind, leaving the token as the boundary. The client sends `Authorization: Bearer` on every request, and the server, when a token is configured, requires it even on loopback — a token that is only checked on a tailnet is a token that can be forgotten locally. The comparison is constant-time so a token cannot be recovered a byte at a time, and the check runs *before* the MCP service, so a wrong token lists no tool and reaches no handler (the test asserts the raw `401`). The config carries only names — `token_env` (a variable name) or `token_file` (a `0600` credentials-file path) — resolved through the same `resolve_secret` the provider key uses; a source that is named but yields nothing is an error, not a silent anonymous request, so a typo cannot masquerade as a server that rejects you. An `https://` URL is still accepted by the client, so a future decision can add certificates without a config change |
| D27 | The HTTP bind is **fail-closed**: default `127.0.0.1:8788`; wildcard and globally routable addresses are always refused, and any other non-loopback bind needs a token or the server refuses to start | The policy runs at config load, before `TcpListener::bind`, so a bad bind is a startup error rather than a socket that is already listening when the mistake is noticed. `0.0.0.0` and `::` are refused unconditionally because they are not an address — they are every interface, the public one included, and no tailnet bind wants them. A globally routable address is refused because minion's wide-area transport is the tailnet, not the public internet, and that is a different decision than this milestone makes. Everything else that is not loopback — a tailnet `100.64.0.0/10` or `fd7a:115c:a1e0::/48` address, a LAN, a link-local one — is refused *unless* a token source is configured, because off loopback the token is the only thing minion has (D26). Loopback needs nothing: it is a local process. The default is loopback so the zero-config case is the safe one, and the private-address list is the one `http_fetch`'s `block_private_ips` already uses, so the two agree on where the public boundary is |
| D28 | The six-target release matrix builds each target on a runner that can also **execute** it — macOS on macOS runners (`macos-15-intel` for x86_64, `macos-15` for arm64), Windows/arm64 on the native `windows-11-arm` runner rather than cross-compiled with `cargo-xwin`, Linux/arm64 cross-linked and run under `qemu-user-static` — so every binary's two stamps are checked before it is published | Three constraints decide it. macOS cannot be cross-linked from Linux: the linker and SDK are Apple's and not redistributable, so the two `*-apple-darwin` targets are built on macOS runners, which is also where they can be run. Windows/arm64 is a real choice — cross-compile an arm64 PE on an x64 runner with `cargo-xwin`, or build natively on the GA `windows-11-arm` image — and the milestone's own requirement picks for it: the asset must be executed to verify its stamps, and a Windows x64 host cannot run an arm64 PE (Windows on ARM emulates x64, not the reverse), so the cross path would ship an arm64 binary that no runner in the matrix can check. `cargo-xwin` would also pull a third-party tool and a downloaded MSVC CRT/SDK into a pipeline whose point is a small, legible surface, and the C in the tree (`aws-lc-sys` through rustls, and the SQLite bundled by `minion-store`) compiles against the native runner's own arm64 MSVC toolchain with no extra setup. Linux/arm64 stays the one cross link — the distro aarch64 gcc is also the C compiler those crates need — and `qemu-user-static` is installed so it too is run rather than trusted. The other cost is a C toolchain per runner, which the GitHub images already carry: NASM and Perl for `aws-lc-sys` on Windows, Xcode CLT for the bundled SQLite on macOS, so the job adds an install step only for the Linux cross. The Windows assets are the bytes of the built `.exe` under the suffix-free published name, because the updater replaces `current_exe()` and the installed file keeps its real name; the stamp check runs the `.exe` itself, whose bytes are the asset. Runner labels are pinned rather than `-latest`, for the same reason the Ubuntu one is (D24): the image must not move under a published binary; the six asset names are the client contract a unit test pins (`minion-core::update::asset_for`). |
| D29 | A peer reaches the model as **one flattened tool per peer**, `peer__<name>_ask`, never as a generic `delegate(target = "…")` | The approval engine decides on a tool *name*: deny rules, allow rules and the per-server/per-peer `ToolPolicy` all key on it. A single generic tool with a `target` parameter would move that decision off the name and into an argument, which is exactly the ambiguity that `mcp_call` was deferred over (D20) and that the flattened `mcp__<server>__<tool>` tools exist to remove. Flattening per peer keeps three things true at once: an operator writes a rule that names one peer (or `peer__*` names them all), the audit row names the tool that actually ran, and the naming style is the MCP client's own — a namespace plus a fixed leaf, here `peer__<name>_ask` rather than `mcp__<name>__agent_ask`, because a peer contributes *one* capability (a brief) rather than a server's catalogue. A peer is still an MCP server underneath (D25): same transport, same handshake, same client, same bearer-token resolution. The alternative — modelling a peer as an ordinary `[mcp.client.servers.*]` entry and letting `agent_ask` reach the model as `mcp__peer__agent_ask` — was rejected because it would offer the peer's whole catalogue (listing sessions, reading transcripts, the cron tools) instead of the one bounded delegation the milestone is about, and because it would lose the per-peer result cap and the two-argument brief that keep the handoff small |
| D30 | A delegation sends a **brief, not a trajectory**, and the answer is capped (~8 KB) and treated as **data** | The handoff-tax measurement (arXiv 2608.24358) is the evidence: escalating with the full trajectory recovers less than half of the quality gap, and the escalation does *better* the less of the weak model's history it carries — so the tool takes `{ brief, context? }` and sends exactly that, with `context` a short optional prefix and no part of the local transcript. The other half of the argument is mechanical: the model that receives the answer is local and small (a 2B model has a 4–8K context), so a long answer back is what breaks it; hence `result_cap_bytes` with an ~8 KB default, cutting on a character boundary and marking the cut. The peer's text enters the transcript as a *tool result* and nowhere else — never as a system prompt, never executed — because a peer is a remote endpoint whose output is untrusted input (T14), and the tool's own description says so. `max_tokens` is forwarded to the peer's `agent_ask` (which now accepts it) so a caller can bound the answer at the source, and a caller that tries to pass a `tools` argument is refused rather than ignored: the peer's surface is the peer's read-only subset (D22), and the caller does not get to widen it |
| D31 | The **depth cap is 1** and it is the read/write line, not a counter; a peer's tokens are folded into the turn's usage | Two small models passing a brief back and forth is the failure mode — an unbounded escalation with no answer at the end. The cap is not a hop counter a caller could reset or forget to thread through: a delegated turn runs under the peer's `agent_ask`, whose inner registry is the read-only subset of the built-ins (D22), and a delegation tool is `Risk::Network`, so it is never in that subset. `mcp serve` also deliberately does not assemble `[peers.*]` (it already ignores `[mcp.client.servers.*]`), so the reachability hole is closed at the source as well as by the risk filter; `mcp_serve`'s tests pin the property so a future widening of the inner surface fails loudly. Cost rides the same decision: the peer's `agent_ask` already reports `usage`, so the delegation tool puts it in the tool result's metadata and the agent loop folds it into the turn's usage — which is what `/cost` reads — through `ToolOutput::reported_usage`, advisory and fail-open exactly as R8 requires (a peer that reports nothing simply does not count, and no turn is blocked on a missing value) |
| D32 | `[tools] only`/`hide` narrow the surface the model is **offered and can resolve**, and are applied *after* shadowing, so hiding is a strict subtraction that touches no permission | §5 with 11 built-ins plus an MCP server's tools is a menu a 2–4B model orders badly from, and the fix could have been a permission change — "hide `run_command` and it is also allowed" — which is exactly the kind of coupling that turns a convenience into a hole. The selection is therefore defined as narrowing only: a rejected name is removed from `schemas()` (so it is not advertised, and the prompt's tool digest follows) **and** from `get()` (so a model that calls it anyway is answered as if the name were misspelled). It is applied last, over the merged registered-plus-catalog list, so hiding a built-in cannot be undone by a same-named catalog tool — which keeps D20's "registered tools win" intact rather than creating a second shadowing rule. Because the filter can only ever remove entries, there is no code path from `hide` to an allowance; the approval engine, the classifier and the risk classes are untouched, and the tool that is reached (if it is reached) is gated exactly as before. `hide` beats `only` for a name in both lists, matching the way deny rules beat allow rules, so a mistake fails closed. The alternative — a fresh registry re-registered by name — would have had to reproduce shadowing and would drift from the MCP catalogue that changes under it |
| D33 | A turn that ends `finish_reason: length` while carrying tool calls is **discarded wholesale** and re-asked, never parsed | §5.2's tool-call assembly concatenates argument fragments before one JSON parse; when the token limit cuts generation mid-argument the result is half an object, and the two obvious responses both lie to the model. Parsing it produces an `invalid_json` tool error that blames the model for a truncation the loop caused and gives it nothing to correct, while silently running the *complete* calls of a batch that also contains a broken one would execute a turn the model was still composing. Both are avoided by dropping the whole batch: no assistant `tool_calls` message is committed (so no id is ever left unanswered and the stored transcript stays valid), any text is kept, and a `system` notice says what happened and asks for a shorter answer — one tool call, minimal arguments. The loop then asks again, which costs one iteration and lands inside `max_iterations`. Discarding the batch rather than only the broken call is the conservative choice for the model this exists for: a 2–4B model that overflowed the limit while emitting several calls is not a model whose other calls can be trusted as complete |
| D34 | Repeated tool calls are answered from a per-turn cache, and any state-changing call clears it first | The pathology is concrete: a weak model calls `grep` or `read_file` with the same arguments three and four times, and each repeat costs an iteration and a provider round-trip for an answer already in the transcript. Within one turn the loop keys a small map on `name` plus *canonical* arguments (re-serialized through `serde_json`, so reformatting does not defeat it) and answers a hit from the first result, marking the event as a repeat. The hazard a cache introduces is staleness, and it is closed by the risk model the project already has: before a call that can change the machine runs (`!Risk::is_observation()`, D15's read/write line), the cache is cleared, so a read is never answered from before a write. The asymmetry is deliberate and safe — an over-eager clear only costs a re-run, a missing clear would return false data — and the same rule means two consecutive identical reads dedupe while `read; write; read` re-reads. The dedup is per-turn rather than per-session on purpose: the next user message is a new intent, and a session-lifetime cache would answer from minutes-old state |
| D35 | `agent.max_tool_calls_per_turn` bounds a turn, and hitting it stops with a `tool_budget` reason that is distinct from `iteration_limit` | `max_iterations` bounds provider round-trips, which is not the same thing: a model can make several calls per round-trip, so a loop that repeats tools burns the session while still leaving iterations on the clock. The budget counts every call the model asks for, repeats included — it bounds the *loop*, not its cost — and `0` keeps today's unlimited behaviour, because a capable model should not be walled off by a default. When it is reached mid-batch the remaining calls are still answered with a budget-error `tool` message, not dropped: the assistant message holding those `tool_calls` is already committed, and leaving an id unanswered makes the stored transcript invalid for the provider, which is the one invariant the loop cannot break. A `system` notice explains the stop and the turn ends with `StopReason::ToolBudget`, kept separate from `IterationLimit` so a log can tell "it would not stop calling tools" from "it would not stop talking" — the two need different fixes |
| D36 | `agent.small_model` appends six short, numbered, imperative rules to the system prompt, and nothing else changes | The observation it encodes is that a 2–4B model imitates "never"/"always" far better than the hedged prose a large model reads correctly: "work in small, verifiable steps" is advice, "Call at most one tool per turn" is an instruction it can pattern-match. The rules are chosen so each maps to a real loop behaviour rather than to taste — one call per turn keeps batches parseable and makes D33's truncation less likely, "never read a file you have already read" and "never repeat a call" name the repetition D34 absorbs, and "stop as soon as you can answer" is what ends a turn at all. They are appended, not prepended, so they are the freshest thing in context, and they are strictly additive: an operator's `system_prompt_file` persona is untouched and the digest of workspace, mode, tools and denies is unchanged. The flag is off by default because handing a capable model a list of rules it does not need is noise at best, and because a prompt is a hint the loop already enforces — the rules persuade, D33–D35 and the gate decide |
| D37 | `[provider] strict_tool_arguments` is an opt-in quirk that adds `"strict": true` inside each function envelope, and a backend that ignores it sees today's request | The backends named in the milestone reach "arguments that validate" three ways — OpenAI-style `strict` function calling, Ollama's structured outputs, a llama.cpp built with `--jinja` and a grammar — and only the first is a *request-body* field; the other two are server configuration. So the client can offer exactly one thing without pretending otherwise: the `strict` flag the first reads, which Ollama's structured-output path also accepts when an operator has enabled it, while a llama.cpp operator sets `--jinja` or a grammar on the server and this flag is the request-side counterpart that makes it useful. It lives inside the function object (`{"function": {..., "strict": true}}`), matching Appendix A's envelope, and is omitted entirely when off, so a backend that rejects unknown fields (the reason `stream_options` is already conditional) sees an unchanged request. It is a request to the backend, not a check of our own: the arguments are still parsed and validated by the tool layer either way, so enabling it can only reduce malformed calls, never widen what runs |
| D38 | The master is a **git repository plus a pull-side `minion config sync`**, never a service the master runs | The requirement is to configure many nodes without depending on the network *between* them, and a pull is the only shape that satisfies it: the master never has to reach a node, so a machine behind NAT, on another network, or not yet on the tailnet (the 1050 Ti today) converges the moment it can reach the channel — and the channel, a Forgejo repo, already exists. The alternative, an HTTP endpoint served by the master (a live `GET /config` or a push channel), was evaluated and refused on three counts: it needs a process that is always on, against the no-daemon principle and this milestone's own non-goals; it needs every node to be reachable *from* the master, which is exactly the connectivity problem the pull exists to avoid; and it would re-implement, worse, what git already provides — history, transport auth, audit, and an offline cache. So "master" is a convention plus a repo, not a running thing, and `sync` is an explicit act a node performs rather than something done to it |
| D39 | The synced layer is a **machine-level layer between the project file and the user config**, materialized as a machine-local `synced.toml` beside the user config | The new layer has to slot into the existing precedence, not replace it, and the existing rule is *more specific wins*. A project `minion.toml` describes one workspace, so it is more specific than any machine-wide policy and stays above the synced layer; the user config is also machine-level but is a personal preference, and an admin policy outranks a preference, so the synced layer sits above it. The result — flags → env → project → synced → user → defaults — reuses the existing specificity rule instead of inventing a second one, and a machine that has never synced has no `synced.toml` and behaves exactly as today. `sync` writes only that file plus a sibling state file (commit, ref, per-file hashes); it is never committed and never part of `--project`, and `Config::load` merges it like any other layer, so no database is involved and `minion run` stays offline. Two consequences are recorded rather than hidden: a cloned repo's committed `minion.toml` can override master policy, and a local user cannot — if master policy should be a floor instead of a layer, that is a distinct "locked keys" mechanism and is left open (§11 R10), not smuggled in here |
| D40 | Overlays are **one file per node** (`nodes/<name>.toml`), the node's name is resolved from `--node` → `[configsync].node` → `hostname -s` → `tailscale status`, and `--dry-run`/`config show --origin` are part of the design, not extras | A single `[node."<name>"]` table in one file makes every node's edit a conflict in that file and makes "add a node" a change to shared content; a directory keeps one node's policy to one file, makes adding a node a file add, and lets a reviewer read exactly one node. The name has to resolve deterministically and be *visible*: an explicit `--node`/config wins, `hostname -s` is the Unix-native default, and `tailscale status --json` is the fallback so a node matches its MagicDNS name without config — and whichever rule fired is printed. An unknown name applies the base alone with a warning rather than failing, so a freshly provisioned machine converges before its entry exists; a *listed* node whose overlay will not parse is a hard error, because a policy that cannot be honoured must not be silently dropped. `--dry-run` and `--origin` are mandatory because a sync that cannot show its diff and its provenance is undebuggable magic — the point of a config layer is that an operator can answer "why is this value what it is" from the tool itself |
| D41 | The bundle is verified **before** it is applied with the same two checks as `minion update` — a per-file sha256 against a committed manifest, and the commit the ref resolves to — and any mismatch refuses the bundle, fail-closed | Whoever controls the channel controls the effective policy of every node, so applying an unverified bundle is remote policy injection, and the contract `minion update` already uses (D23) is the right one to borrow verbatim. The checksum binds the bytes to the manifest and the commit binds the manifest to a revision, so swapping the files *and* the manifest still fails the commit check; the pair is needed even though git hashes its own objects, because git proves what it fetched and says nothing about a hand-edited cache or a bundle read another way. A refusal exits `4` and writes nothing, and the write is temp-file-plus-rename, so an interrupted sync cannot leave a half-applied layer. The honest limit is stated, not papered over: the manifest is committed in the repo, so a malicious publisher rewrites it alongside the files and passes both checks — the residual is publisher trust, the same acknowledged limit D23 carries, recorded as T15 and R10. What bounds the blast radius is that the channel carries only *names*, never a secret value (§5.1): a compromised master distributes policy but cannot exfiltrate keys |
| D42 | The master carries **names, never secret values**, and the non-goals are explicit: no secret centralization, no node discovery, no always-on service, no sync timer | The invariant that makes the design safe is §5.1's: a config records *where* a secret comes from, never the secret. If the master could carry values it would become the single place where every node's keys live — the exact centralization this milestone is told not to build, and a single point of compromise for the fleet. So the channel distributes `token_file` paths and `token_env` names; values are provisioned per node by whatever the operator already uses (environment, a credentials file, or the separate token-manager system), which keeps this milestone out of the secrets business. Node discovery and self-enrolment are refused because a node appears in the repo because a human put it there — scanning a tailnet and trusting whatever answers is a different threat model. And because the whole value of the design is that the master is *not* a process, anything that would require one — an endpoint, a listener, a background sync loop — is out of scope by construction; a node that wants periodic sync wires its own cron to call `minion config sync`, so minion still starts no daemon (the spirit of D5) |
| D43 | An audit row carries the tool decision **and its result**, written once, and redaction lives in the log sink rather than at each call site | §7 says the trail records "every tool decision and outcome", which one row per call can only do if the row is finished after the tool runs. A refusal is a finished decision the moment it is made, so it is written at `check` with `outcome = denied`; an allow is not, so `RecordingGate` holds it and `ToolGate::record_outcome` completes it with `ok`/`error` and the wall-clock duration. The alternative — writing the allow row eagerly — either loses the outcome or forces a second row and doubles the table for every call. The context (`session_id`, `turn_id`) is stamped per turn by the agent, so a decision can be grouped back to its conversation without threading a session handle through the policy engine, which has no business knowing about conversations. Redaction is the sink's job for the same reason the audit is the gate's: a guarantee that depends on every `tracing!` call remembering to be careful is not a guarantee. `RedactingMakeWriter` masks the resolved key, the configured env var's value, secret-shaped header values and any `Bearer` token before a byte reaches stderr or the log file, and the `--version`/`--quiet` surface is generated from `build.rs` so a released binary states its commit and configured features (NFR-9, §9) |
| D44 | `minion doctor` treats a reachable-but-`/models`-less backend as up, and the NFR targets are measured rather than assumed | `/models` is optional in the OpenAI-compatible world, so "the provider did not answer" has to mean a transport failure or an auth rejection, not a 404 from an endpoint that is plainly serving. A doctor that failed on a missing `/models` would be wrong about a working backend, which is worse than being silent about a capability it cannot see. The probe is therefore reachability plus credentials — the two things that actually stop a user — and it runs last, after config and database, so a broken setup is reported before a socket is opened and exits `3`. The NFR work follows the same principle: `NFR.md` records the measured figure next to each target, with the command that produced it. Where a target was only just met (NFR-1's cold start, which includes the harness's own `fork`/`exec`) the number says so instead of the target being quietly loosened, and musl's C-toolchain requirement is documented rather than hidden behind a target that was not reproduced |
| D45 | A **lean** switch (`--lean`) fills a small-model profile over M10.3's knobs, and the loop gains a token budget, a tool-result cap, argument repair, one bounded nudge and provider wire compatibility | M10.3 gave a weak model a trimmable surface, a tool-call budget, repeat dedup and strict-argument hints, but three holes remained. First, context was still measured in **messages** (`history_window`), and a single `read_file` result of 20 000 lines could overflow a 4k window before the count was reached — and tool results accumulate *between* iterations, so a load-time-only trim cannot see them. The budget is therefore enforced inside `Agent::run`, before every provider call, dropping whole oldest turns and never splitting a `tool_calls`/result pair (the same invariant as the message window); the estimator is a dependency-free character heuristic, and a prompt-plus-schemas cost larger than the budget fails the turn with an explanation instead of sending a request that will be rejected. Second, a local backend is often slow, non-conforming or strict: the stream idle timeout was a hardcoded 60s that a CPU prefill can exceed before the first token, so it is configurable; a chunk with object-shaped `arguments` or an unparseable frame used to end the turn, so it is tolerated (and a stream that yields *no* events is now an error rather than a silent empty completion); `$schema`/`title`/`format` and `["T","null"]` unions are stripped because grammar-based decoders compile the schema and choke on them, keeping `description`/`enum`/`required`/bounds; `[provider.extra_body]` admits backend-specific sampling while core request keys are rejected; and a non-streaming endpoint is driven by synthesizing the same events. Third, a weak model's *output* is imperfect: a fenced or trailing-comma argument string is repaired after a normal parse fails (never before), a missing tool-call id gets a unique `call_{index}` fallback, an unknown tool error lists the names that would have resolved, and a reply with neither text nor a call is given exactly one bounded second chance before the turn ends. `--lean` is a CLI convenience that fills these plus `small_model`, the budget, the caps and the compatibility quirks — applied after the config file, exactly as every global flag, and filling each key only where it is still at its default, so it is a profile and not a second code path. None of it touches the gate: the tool surface is still narrowed by D32's `[tools]`, the budget refuses calls by D30's rule, and a small model gets no policy exemption |

| D46 | The project, the binary and the crate family are renamed `minion` -> `imp`, and the rename migrates state instead of abandoning it | The name was aspirational and wrong: `minion` describes a subordinate, and this is the harness that drives a model, not something a larger agent commands. The rename is done in one commit across all three vocabularies because splitting them is what breaks things: the `--version` line is the *client contract* of the release gate (`grep -q "^imp ${VERSION#v} "`) and of `imp update`'s commit check, so the program name, the workspace crate names, the binary name, the asset names and the workflow's grep can only change together. Twelve environment variables move `MINION_*` -> `IMP_*`; a stale `MINION_MODEL` is simply not read, which is the one behaviour change a user can notice and the reason the release notes name the new names rather than both. The offline state is the single place where a wrong rename loses data, so `imp` **migrates instead of starting empty**: on the first run it renames the legacy `~/.config/minion` and `$XDG_STATE_HOME/minion` directories to their `imp` counterparts (a directory `rename()` is atomic, keeps the `0600` mode of the credentials file and moves the database with everything else), and inside the state directory it renames `minion.db` (plus any `-wal`/`-shm`/`-journal` sidecar) to `imp.db`. The migration only ever runs when the destination does not already exist: with both directories present nothing is touched and the manual procedure is printed instead, because the one thing worse than a missing database is a half-merged one. It is not fatal on failure either - a `rename()` that cannot proceed leaves the old files exactly where they were and says so, rather than starting on an empty database in silence. A project `minion.toml` cannot be migrated from inside the binary (the binary does not know which directories hold one), so `imp.toml` wins when both are present and a bare `minion.toml` is still read with a deprecation warning; the rename is a `git mv` the user does when they like. The auto-update channel changes as one unit too: `repo = "Az107/imp"` and `asset_prefix = "imp"` in the same commit as the `imp-<os>-<arch>` assets. An installed `minion` binary carries the old channel in its bytes and looks for `minion-*` assets, so after the switch it finds none and **refuses** the update (fail-closed, D23) rather than installing something wrong - with a handful of machines the documented transition is one manual reinstall, not a compatibility release that would have to be maintained forever. The decision log itself is a historical record: entries D1-D45 keep the words they were written with, and this entry is the one place the rename is described. |
| D47 | Terminal presentation is one `[ui]` section behind one in-tree ANSI helper, and the only motion is a single opt-in activity line that is erased, never scrolled | The palette, the glyph set and the "is colour allowed" decision were scattered across `markdown.rs`, `render.rs` and `approval.rs`, which is how a `NO_COLOR` promise drifts: each new line could forget one of `NO_COLOR`, `--no-color` or `--json`. They now come from one module and one `Theme`, so the rule is stated once — 16-colour SGR only (no truecolor dependency, so NFR-3 and a serial console both survive), colour never the sole signal because every coloured mark also carries a word or glyph (NFR-8), and `--json` ignores the section entirely. `[ui]` is cosmetic and deep-merged like every other section: `color = auto|always|never`, `icons = auto|ascii|none`, and `spinner`, on by default but drawn only on a terminal, because motion is useful feedback and a pipe, `--json` or `--quiet` never sees it. The tool-activity line finally implements §5.11's own example — `▸ name args · risk`, then `  ↳ 12ms summary` — by carrying the risk class and the wall-clock duration on the `AgentEvent` the loop already emits; `duration_ms` is an `Option` so "never ran" (a repeat answer, a budget refusal) is distinct from a genuine 0 ms, and the approval outcome (`approved (once)`) is printed by the prompt itself rather than plumbed back through the gate, which keeps the engine's decision path untouched; on a terminal the questionnaire is then erased and collapsed to that one decision line, so the scrollback keeps the decision and not the prompt. The one thing that brushes D2 is the spinner, and it is deliberately the smallest possible thing: a single stderr line owned with a carriage return, serialised against the renderer by a mutex, stopped and erased before the first real event, and never shown off a terminal or under `--json`/`--quiet`. The terminal is still a scrollback, not a canvas: nothing already printed is revised, and the scrollback rule is preserved exactly. |
| D48 | A recognised read-only shell command may run without approval, opt-in via `[policy.read_only].enabled` and the runtime `/auto` command; the classifier is a conservative whitelist that refuses shell syntax it cannot model | `run_command` is `Risk::Execute`, so every `ls`, `git status` or `grep` is treated exactly like an `rm` — it prompts on a terminal and is refused in a pipe. The alternative — loosening the tool's risk class — would widen every command at once, so the decision is made per invocation and the tool stays `Execute`. The shortcut sits between the classifier and the `ReadOnly` rule, after deny rules, allow rules and the classifier, and only when the classifier flagged nothing, so it can shorten the path to a prompt and never bypass a refusal — the property the System One guard also has (D16). It deliberately runs before the non-interactive branch, so a read-only command survives a pipe and a cron run exactly as `read_file` does (D15), and `--deny` does not suppress it, because `--deny` moves the fallback and a read is not what a missing terminal protects. The classifier is a whitelist, not a smarter denylist: it first refuses any command using shell syntax it cannot model (`;`, `&&`, `|`, redirection, `$(…)`, backticks, quotes, a leading `VAR=value`), because `ls; rm -rf x` must not read as `ls`, and only a single unadorned command whose verb is in the table — or a read-only `git` subcommand, or a `find` without a writing predicate — is exempt. A command it does not recognise is not refused, merely put back on the ordinary path, so an incomplete table is a missing convenience, never a hole. It is off by default for the same reason `http_fetch`'s reachability list does not drive approval (D17): nothing added is silently auto-approved. `/auto` turns it on for the process, and the switch is a shared `Arc` so the next `check` sees the change without rebuilding the engine; a cron gate builds its own switch from config, so an interactive toggle never reaches a scheduled job. `--yes` and the shortcut are different: the former consents to everything, the classifier only to what it recognises. It is not a sandbox (R2) — aliases, shell functions and `GIT_PAGER`/`GIT_EXTERNAL_DIFF` can still cause side effects, so the classifier stops the *model* from issuing mutating commands, it does not confine a hostile environment. `/auto full`, delegating the decision to the System One model, is reserved and refused for now |
| D49 | The context window is **discovered from the endpoint** when it is not configured: `provider.context_window` wins, otherwise `/models` (vLLM), `/api/v0/models` (LM Studio), `/props` (llama.cpp) and `/api/ps` (Ollama) are asked, the last three only for a loopback address | The OpenAI-compatible API does not publish the window, and the window is the number that decides whether a turn fits: a model loaded at 4k rejects the second request of a turn that read a large file, and it does so as an opaque 400 that reads like a tool failure. Asking the operator to restate a number the server already knows is the worse default, and a wrong guess is expensive in both directions — too small and imp truncates work that would have fitted, too large and the turn dies. The probe is therefore opportunistic and never authoritative: an explicit setting skips it entirely, every request is bounded to two seconds, and an endpoint that answers nothing leaves the budget unset, which is exactly the behaviour of the version that had no probe. The origin-level endpoints are local-server conveniences, so they are only tried when the base URL is loopback; `/models` is asked of anyone because it is the endpoint imp already calls for `/model`. Discovering a window also *turns budgeting on* when `agent.context_tokens` is unset — with generation room reserved, because a reasoning model can overflow while producing its answer — and the effective budget, the source and the model's `tool_use` capability are reported by `/context` and `imp doctor`, which is what makes a silent failure legible after the fact |
| D50 | Compaction runs **at the turn boundary, before the budget is exceeded**, replacing everything before `agent.compact_keep_turns` turns with one `[summary]` message; the summary is never allowed to cost more than what it replaces, a failure falls back to trimming, and the stored transcript keeps every message | Three constraints decided this. First, a summary must be written before the request it protects is sent, because writing it needs room of its own: compacting only once the budget is already exceeded makes the summarization request itself the overflow. Second, a true background compaction — a side request fired while a turn is generating — is counter-productive on the backends this feature exists for: LM Studio and `llama-server` serve one request at a time, so the side request queues behind the turn and doubles its latency. Doing it after the turn ends, while the user reads the answer, keeps the benefit and removes the concurrency. Third, R3 requires that summarization never becomes data loss, so the summary is a *view*: the in-memory history shrinks, SQLite keeps the full transcript, and a resume therefore re-derives the same state rather than trusting a rewritten log. Two guards keep it honest — a floor below which a summary cannot pay for itself, and a post-check that discards any summary larger than the range it replaces, so compaction can never make the context bigger. The role a summary travels as is `system` by default (it is context, not something the user said) with `[provider.quirks].summary_as_user` for chat templates that accept only one system message, and `/compact off` disables the whole path per R3 |
