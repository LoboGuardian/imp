# AGENTS.md

## Status

Rust workspace implementing `SDD.md`. Milestones **M0 through M6 are complete**: config and layered
credential resolution, an OpenAI-compatible streaming provider with `${session}` header support, the
agent loop, the read/write/patch/exec tool set, the approval engine, a SQLite store with resumable
sessions, keyword memory, `http_fetch` with its SSRF guard, `imp init`, the optional System One
guard for the approval gate, the in-process cron scheduler with job CRUD, run history and catch-up,
the **MCP client** that consumes external servers as gated tools, and the **MCP server** that
publishes the agent to another model. M5 landed on branch `m5-mcp-client`, cut from `m4-cron`; M6
landed on `m6-mcp-server`, cut from `m5-mcp-client`.

**M3 is done.** `remember`/`recall` live in `imp-store/src/memory.rs`,
`imp-tools/src/memory.rs` and `imp-core/src/memory.rs`; `http_fetch` and its guard are
`imp-tools/src/http_fetch.rs`, and the `[http_fetch]` config section is
`HttpFetchConfig` in `imp-core/src/config.rs`. The milestone table in `SDD.md` reads `done`.

**M3.5 is done.** The guard's policy — eligibility floor, thresholds, the `SystemOneGuard` trait —
is `imp-core/src/guard.rs`, the engine consults it from `PolicyEngine::check`
(`imp-core/src/policy.rs`, step 5b), and the `/v1/systemone` client is the `imp-guard` crate.
`[guard]` is `GuardConfig` in `imp-core/src/config.rs`. Still disabled by default.

**M4 is done.** The scheduler is `imp-cron` (`scheduler.rs`, `schedule.rs`, `jobs.rs`); the job
and run types plus the `JobStore` boundary are `imp-core/src/job.rs` and the `Clock` trait is
`imp-core/src/clock.rs`; the SQLite side is `imp-store/src/jobs.rs`; the `cron_*` tools are
`imp-tools/src/cron.rs`; the subcommand and the service that runs with a session are
`imp-cli/src/cron.rs`, with the real agent runner and the cron gate in
`imp-cli/src/setup.rs`. `[cron]` is `CronConfig` in `imp-core/src/config.rs`. The `jobs`,
`job_runs` and `audit_log` tables were created by the baseline migration and now have writers.

**M5 is done.** The client is `imp-mcp`: `client.rs` wraps one `rmcp` stdio connection, `servers.rs`
owns the configured set and implements `imp_core::tool::ToolCatalog`, and `tool.rs` is the external
tool's `Tool` implementation. `[mcp.client.servers.*]` is `McpConfig` / `McpClientConfig` /
`McpServerConfig` in `imp-core/src/config.rs`; the live catalogue seam is `ToolCatalog` and the
registry in `imp-core/src/tool.rs`; the per-server approval fallback is `ToolPolicy` in
`imp-core/src/policy.rs`; the subcommand is `imp-cli/src/mcp.rs` and the wiring is in
`setup.rs`, `run.rs` and `repl.rs`. `src/bin/mcp_stub_server.rs` is a real MCP server used as the test
fixture, so the tests exercise a process boundary rather than a mock.

**M6 is done.** The server is `imp-cli::mcp_serve`: `Runtime` assembles the shared pieces (store,
read-only tool subset, the non-interactive gate), `ImpServer` is the `rmcp` `ServerHandler`, and
the exposed tools are ordinary `imp_core::tool::Tool`s dispatched through the same gate. `[mcp.server]`
is `McpServerSection` in `imp-core/src/config.rs`; the subcommand is `McpAction::Serve` in
`cli.rs`, dispatched from `mcp.rs`. `agent_run_command`/`agent_write_file` are `Exposed` wrappers over
`RunCommand`/`WriteFile`, so the surface adds a name and nothing else. The golden surface lives in
`crates/imp-cli/src/snapshots/imp__mcp_serve__tests__the_default_surface_is_a_golden.snap`.

`imp-mcp`'s manifest lists real dependencies (`rmcp`), and it now has code behind them. Both halves
of MCP exist: `imp-mcp` is the client (§5.10), `imp-cli::mcp_serve` is the server (§5.9).

**M7 is done.** The CLI surface of §5.12 is complete: `imp config show|path` (`config_cmd.rs`),
`imp doctor` (`doctor.rs`), and the global `--quiet/-q`, `--resume <session>`, `--max-iterations`
flags, with the documented exit codes (`main.rs::exit_code`, asserted by a unit test). The audit
trail now carries the *result* of each call, not just the decision: `RecordingGate` holds an allowed
row until `ToolGate::record_outcome` finishes it, so one row carries decision + outcome + duration,
plus the session and turn (`imp-core/src/policy.rs`, `imp-store/src/approvals.rs`).
`/cost` aggregates per conversation through the new `session_usage` table (schema v2). A log sink
(`imp-cli/src/logging.rs`) masks credentials and `Bearer` tokens before anything is written
(NFR-9). Packaging is `build.rs` (git SHA + features in `--version`), `install.sh`, and a `Makefile`
with `make dist`. The NFR numbers are measured in `NFR.md`.

`default_registry` takes a second argument, an `Arc<Store>`, because the memory and cron tools need
one, and a third, a `CronContext`, carrying the clock and default timezone the `cron_*` tools use.
This is why `imp-tools` depends on `imp-store` and `imp-cron`; the store is opened *before*
the registry in `setup::build` so the system prompt can still be built from the finished tool list.

Assistant text is rendered as markdown on a terminal (`crates/imp-cli/src/markdown.rs`):
headings, emphasis, code, lists, quotes, rules, and pipe tables. It is rendered per block, so
nothing already on screen is ever revised.

**M10.3 is done** (branch `m10-small-models`, cut from `feat/update-command` at 77e054c). The
small-model loop ergonomics live in `imp-core/src/agent.rs` (truncated-call recovery, the per-turn
repeat cache, the tool budget, `StopReason::ToolBudget`, `AgentEvent::Notice`),
`imp-core/src/tool.rs` (`ToolSelection` and the filtered `ToolRegistry`),
`imp-core/src/config.rs` (`[tools]`, `agent.max_tool_calls_per_turn`, `agent.small_model`,
`provider.strict_tool_arguments`), `imp-cli/src/setup.rs` (where the selection is applied and the
numbered rules are appended) and `imp-provider/src/openai.rs` (`to_wire_strict`). See the
"Small-model loop rules (M10.3)" section below before touching any of it.

**D45 is on branch `small-model-additions`** (cut from `main` after PR #4 merged). It adds `--lean`
and the loop/provider hardening: `imp-core/src/tokens.rs` (the estimator),
`imp-core/src/args.rs` (argument repair), `sanitize_schema` in `imp-core/src/provider.rs`,
the budget/cap/nudge paths in `imp-core/src/agent.rs`, and the wire changes in
`imp-provider/src/openai.rs` (`stream_idle_timeout_secs`, `extra_body`, quirks, non-streaming).
See the D45 bullets in "Small-model loop rules" before changing any of it.

**D49/D50 are on branch `feat/context-window-compaction`**, cut from `main` after v0.1.3. The context
window is discovered from the endpoint where the backend publishes it (`imp-provider/src/context.rs`:
vLLM `/models`, LM Studio `/api/v0/models`, llama.cpp `/props`, Ollama `/api/ps`), `doctor` and the new
`/context` command report it, and a history that nears its budget is compacted into one `[summary]`
message instead of being dropped (`imp-core/src/compact.rs` for the pure rule set, `agent.rs`'s
`compact_if_needed` for the trigger, `imp-cli/src/summarize.rs` for the request that writes it). See
"Context and compaction rules" below before touching any of it.

## Commands

```sh
cargo +1.89.0 build                # whole workspace
cargo +1.89.0 test --workspace     # all tests; offline, no network, no API key needed
cargo +1.89.0 test -p imp-core   # one crate
cargo +1.89.0 test -p imp-cron   # the scheduler: virtual clock, no sleeps, no network
cargo +1.89.0 test -p imp-mcp    # the MCP client: spawns the stub server, no network
cargo +1.89.0 test -p imp-cli --bin imp mcp_serve   # the MCP server, over an in-memory pipe
cargo +1.89.0 test -p imp-core agent::tests::runs_a_tool_and_feeds_the_result_back   # one test
cargo +1.89.0 clippy --all-targets -- -D warnings    # lint gate; currently clean
cargo +1.89.0 fmt --all
./target/debug/imp --help
./target/debug/imp --version                       # version, git SHA, features
./target/debug/imp doctor                          # env / config / db / provider
./target/debug/imp config show                     # effective config, redacted
make dist                                             # per-target release tarballs + .sha256
./install.sh --from target/release/imp --prefix ~/.local
cargo +1.89.0 build --release --locked --target aarch64-unknown-linux-musl  # see NFR.md for the musl CC
```

`cargo test -p imp-mcp` needs no network, but it *does* spawn processes: the tests run
`target/debug/mcp-stub-server`, the fixture binary in that crate, over a real stdio pipe. That is the
point — the plumbing (spawning, listing, filtering, surviving a dead server) cannot be tested against
an in-process mock. A snapshot went stale? `INSTA_UPDATE=always cargo +1.89.0 test -p imp-mcp`
rewrites `tests/snapshots/*.snap`, and the diff is the review.

**Invoke cargo as `cargo +1.89.0`.** The installed `default` toolchain on this machine is 1.88.0,
which is below the workspace's `rust-version = "1.89"` and cannot build it at all — cargo refuses
before compiling a crate. 1.89.0 is installed as a side toolchain rather than as `default`, so
other projects keep the version they expect. If `rustup default 1.89.0` is ever set, the `+1.89.0`
prefix becomes redundant but stays harmless.

Toolchain floor is **Rust 1.89 / edition 2024** — the code uses let-chains
(`if let Some(x) = a && let Ok(y) = b`), which need edition 2024.

## Non-obvious structure

- Package `imp-cli` produces a binary named **`imp`** via an explicit `[[bin]]`. Adding a
  second bin, or relying on the package name, will surprise you.
- `imp-core` has **no terminal, network, or database dependencies** on purpose: the agent loop
  is tested against a scripted `MockProvider` in `crates/imp-core/src/agent.rs`. Keep it that
  way — push I/O into `imp-provider`, `imp-tools`, `imp-guard`, or `imp-cli`.
- Assistant text goes to **stdout**; tool activity, logs, and errors go to **stderr**. This is what
  makes `imp run ... > answer.txt` clean. Don't print diagnostics to stdout.
- **Piped output is never rendered.** `run::style_for` checks `IsTerminal` *before* honouring
  `--markdown`, so `imp run ... > out.md` still yields markdown source. If you make the flag win,
  every user piping output gets ASCII tables.
- **Layout and colour are separate.** `NO_COLOR` and `--no-color` drop the escapes but keep table
  borders and list markers, because column alignment carries meaning and colour does not.
- Config overlays are deep-merged on `toml::Value` trees, so a project `imp.toml` only restates
  the keys it changes. **Unknown keys are ignored on purpose** (forward compatibility) — do not add
  `deny_unknown_fields`.
- Every session gets a `session_id` (`imp_core::new_session_id()`, a v7 UUID). It is expanded
  into any configured header containing `${session}`. Some gateways require this: OpenCode Go
  rejects requests without `x-opencode-session`. The id must stay **stable for the whole
  conversation** — regenerating it per request would break the prompt-cache routing it exists for.
- `imp init` deliberately runs *before* config loading in `main.rs`, so a broken or missing
  config can still be repaired by it. Keep that ordering.
- **Markdown rendering is a presentation concern and lives in `imp-cli`, not `imp-core`.**
  `imp-core` still emits raw `TextDelta`. The parser is `pulldown-cmark`; the renderer is ours.

## Invariants that must not be relaxed

- **Every `Write` and `Execute` tool must go through the gate.** `default_registry()` now registers
  `read_file`, `edit_file`, `apply_patch`, `write_file`, `run_command`, `remember`, `recall`, and
  `http_fetch`, and `setup::build_gate` attaches a `PolicyEngine` to the agent. A registry built
  without a gate is only safe for read-only use — if you add a tool, confirm the gate is wired, not
  that the tool "seems harmless". `http_fetch` is `Risk::Network`, which is gated with the writes
  (D15), so a non-TTY refuses it too. **An external MCP tool is no different**: it is an ordinary
  `Tool`, dispatched by the same `Agent::dispatch`, so there is no second path to a server and nothing
  to wire separately.
- **A catalogue tool never shadows a registered one.** The registry enumerates registered tools first
  and skips a name already taken, so `mcp__…` cannot displace a built-in whatever a server calls
  itself. Don't "fix" that by letting a catalogue override a name — the ordering *is* the guarantee.
- **`tool_allow` is fail-closed.** An empty list allows nothing; `["*"]` is how an operator says
  everything. It is applied before the catalogue is built, so a tool that is not listed is absent from
  the schemas *and* unresolvable by name.
- **`[tools] only`/`hide` narrow the surface, never the permissions (M10.3).** The registry drops a
  rejected name from `schemas()` *and* from `get()`, and applies the selection *last*, over the
  merged registered-plus-catalog list, so hiding a built-in cannot be undone by a same-named MCP
  tool. Because it only removes entries, there is no path from `hide` to an allowance: the gate is
  untouched, and `hide` beating `only` keeps the negative rule fail-closed. Do not "optimise" this by
  re-registering a fresh registry by name — that would have to reproduce D20's shadowing rule.
- Approval is fail-closed: non-TTY never prompts and defaults to `deny`; deny rules always beat
  allowlists.
- **The MCP server is read-only by default.** `expose_exec`/`expose_write` stay `false`;
  `run_command` is never *callable* over MCP by default. The two tools are still listed — the denial
  is a policy decision the audit trail records, not a missing tool (D22) — and `agent_ask`'s inner
  agent only ever gets the read-only subset of the registry. `imp mcp serve` never starts a REPL
  (R5): both own stdin/stdout.
- Cron job runs always use the non-interactive policy and cannot prompt for approval. The gate is
  `setup::build_cron_gate` — no `ApprovalUi`, `interactive = false`, no guard. Don't route a job
  through the session's gate.
- Path guard: `ToolCtx::resolve` canonicalizes and confines to `workspace_root`. Use it for every
  path-taking tool; do not reimplement it.
- **The config file must never contain a secret.** `Plan::to_toml` has no field that can hold the
  API key — the token is collected separately and written to the `0600` credentials file. Keep it
  that way; `--project` configs get committed.
- Credential resolution is env var first, then credentials file (`Config::api_key`). Both keys are
  `String`, not `Option`, because empty must *override* the default rather than fall back to it — a
  keyless backend sets **both** to `""`.
- The `Authorization` value is wrapped in `ApiKey`, which has a redacting `Debug`. Never log it.
- Terminal echo is disabled **before** the secret prompt is printed (see Toolchain gotchas).

## Toolchain gotchas

- **`tools[]` must be the function envelope.** Each entry is
  `{"type":"function","function":{"name","description","parameters"}}`, not the flat `ToolSchema`
  struct. Serializing `ToolSchema` directly 400s on any compliant provider. A permissive test double
  accepts the flat form happily, so assert the envelope in a test — this bug shipped once because
  every test used a stub that ignored the request body.
- **Config tests must not read the real user config.** Use `Config::load_with(user, explicit, cwd)`
  and pass `None` for the user path. `Config::load` picks up
  `~/Library/Application Support/imp/config.toml`, so a developer's own `imp init` breaks tests
  on any machine that has one.
- **A gateway saying "upstream request failed" is rejecting the request body.** Bisect the fields
  against the live endpoint instead of guessing: capture the exact JSON, replay it, then remove one
  key at a time. Cloudflare also blocks python `urllib`'s default signature with
  `403 / error code 1010`, so a diagnostic script must send a normal `User-Agent`.
- **reqwest 0.13**: TLS feature is `rustls`, not `rustls-tls`. We build it with
  `default-features = false`.
- **Provider streams use `async_stream::stream!`, not `try_stream!`.** `?` inside `tokio::select!`
  does not compile under `try_stream!`; the stream yields `Result<ChatEvent>` explicitly.
- **schemars 1.x**: tool argument schemas are derived with `schema_for!` and converted via
  `serde_json::to_value`. That requires the `derive` feature. It emits `$schema`, `title`, `format`,
  and `"type": ["integer","null"]` for `Option<T>` — accepted by OpenAI, not by every gateway.
- Retries happen **only before the first stream event** (in `OpenAiProvider::open`), so a retry can
  never duplicate text or re-run a tool. Don't move retry logic downstream.
- **`${session}` cannot be written inside a `println!`/`format!` format string.** `{session}` is
  parsed as a format argument and the compiler suggests implementing `Display` on your type. Pass it
  as `{}` with a variable, or escape the braces as `${{session}}`.
- **Do not use `rpassword` for the secret prompt.** It prints the prompt and *then* disables echo,
  which leaves a window where a pasted key is echoed into terminal scrollback. `ask_secret` in
  `init.rs` owns the termios guard so echo is off before anything is printed.
- **`Options::empty()` in pulldown-cmark disables GFM tables.** The default is `Options::all()`, so
  name the extensions you want explicitly. A table that renders as a paragraph of pipes is this.
- **pulldown-cmark's `TableHead` has no `TableRow` wrapper.** Body rows do. Assign the header on
  `End(TagEnd::TableHead)` or you get a blank heading line and column widths driven by the body.
- **Match-arm order in `collect_blocks` is load-bearing.** `Start(Strong)` is both an inline mark
  and a `Tag`, so the inline arm must come *before* the generic `Start(tag)` arm. Reversed, match
  tries arms in order, `start_block` claims the emphasis, returns `None`, and the style is dropped.
- **`collect_inlines` takes an explicit `consume_end`.** It stops at the *first* `End` it sees. A
  block caller wants that consumed; a list-item caller must not, because the event it stops on is
  the item's own `End` and the enclosing list is what has to see it. Get this wrong and the next
  item is silently swallowed. `transparent` covers the same problem one level down for links and
  images, whose `End` must not end the surrounding sentence.
- `std::env::set_var` is `unsafe` on edition 2024 and racy under `cargo test`'s threads. To test
  env-dependent behaviour, extract a pure function taking the value as a parameter, or set the
  variable on the *process* from the shell in a manual test.

## Policy rules

- **Rule order in `PolicyEngine::check` is the security property.** Deny → allow → classifier →
  read-only command (opt-in) → ReadOnly → non-interactive → default. Reordering lets a later rule skip
  an earlier refusal.
- **The read-only-command shortcut is opt-in and fail-closed (D48).** With `[policy.read_only].enabled`
  (default off) or after the REPL's `/auto`, a `run_command` the classifier in `imp-core/src/read_only.rs`
  recognises as an observation runs without a prompt. It is rule 3b: after deny, allow and the
  classifier, and only when the classifier flagged nothing, so it can shorten the path to a prompt but
  never bypass a refusal. It sits before the non-interactive branch (a read is free unattended, like
  `ReadOnly`, D15) and `--deny` does not suppress it. The classifier is a whitelist that refuses any
  shell syntax it cannot model (`;`, `&&`, `|`, `>`, `$(…)`, backticks, quotes, a leading `VAR=value`);
  an unrecognised command is not refused, merely put back on the ordinary path. `/auto` flips one
  `Arc<AutoCommands>` shared with the gate; a cron gate builds its own from config. It is not a sandbox,
  and `/auto full` (System One) is reserved.
- **`ReadOnly` is checked *before* the non-interactive rule; `Network` is not.** D15, and it
  deliberately does not match `Risk::requires_consent`. A piped `imp run ... > out.md` can read
  and recall, but a write or an `http_fetch` still needs `--yes` or an allowlist entry. Use
  `Risk::is_observation()` for the read/write line rather than matching `ReadOnly` — it keeps the
  side that `Network` sits on stated once. If `Network` ever moves to the read side, that helper is
  the only thing that needs changing, and it needs a new decision-log entry.
- **`policy.default = "auto"` does not silence the classifier.** `rm -rf`, `sudo`, and `curl | sh`
  always prompt. That is the whole reason the classifier exists.
- **Allowlist patterns are anchored at the start and exact unless they end in `*`.** `pattern = "echo"`
  matches only the bare command `echo`; `pattern = "echo *"` matches any echo. It fails closed, so
  a mistake is a refusal, never a leak.
- **`--yes` replaces the *default decision*, not the engine.** Deny rules and the classifier still
  apply, and a non-TTY still cannot prompt.
- **Persisted allows store the verb**, not the whole command, so approving one `cargo build` does
  not approve every `cargo` invocation.
- The approval prompt matches exact input only. First-character matching used to turn `sure` into
  session scope; keep the strict match.

## Guard rules (M3.5)

- **The guard may only narrow a prompt.** It is consulted at the one place a prompt would be shown
  (`PolicyEngine::check`, step 5b), after deny rules, allowlists, the classifier and the
  non-interactive decision, so a silent allow there can only shorten the path to a prompt. If you
  move the call, the property to preserve is that every refusal still refuses and every earlier
  decision still decides.
- **A non-interactive run never reaches the guard, and must not.** There is no prompt to resolve off
  a terminal, so the request is not made and `policy.noninteractive` still decides. The call sits
  after the `!self.interactive` branch on purpose: before it, a model allow would let a `curl`
  through a pipe that D15 deliberately denies.
- **`privilege`, `remote-execution` and `destructive` are a floor, not a filter.**
  `imp_core::guard::is_eligible` runs before any socket is opened, and one ineligible tag poisons
  the whole command — `curl … && sudo …` is ineligible. A command the classifier did not flag is not
  eligible either: the guard resolves a *flagged* prompt and nothing else. Don't "improve" the floor
  by letting the model opine on an ineligible command.
- **`state` is the command string alone.** `{"state": "<command>"}` is the entire request body; the
  transcript, tool output and file contents are never included (FR-46). A test asserts the payload
  has exactly one key.
- **A verdict is a number and the thresholds live in our code.** The reply is `{"unsafe": <0..1>}`,
  so a model cannot assert itself into an allow in prose. Non-JSON, a missing or non-numeric `unsafe`,
  a non-finite value, or one outside `[0, 1]` is an error.
- **Every failure is a prompt, never an allow.** Network, timeout, non-2xx, unreadable body: the
  guard returns `Err` and the engine falls through to the ordinary prompt (FR-45). Do not "fall back
  to the default decision" here — the failure path *is* the prompt.
- **The middle band is not a refusal.** A verdict above `allow_threshold` prompts whether it lands
  between the thresholds or above `deny_threshold`; the band only changes what the audit records
  (`guard_uncertain` vs `guard_deny`). FR-47 keeps a human in the middle by design.
- **Every verdict is audited**, failures included (`guard_error`), through `ApprovalStore::audit`,
  which is best effort and never fails a turn.
- **`[guard]` is off by default, wired in `imp-cli::setup::build_gate`.** `imp-core` has no
  network dependency; `imp-guard` is the only crate that opens the socket, behind the
  `SystemOneGuard` trait. Keep the client there — a `reqwest::Client` in `imp-core` breaks the
  rule that lets the loop be tested against a mock provider.

## Small-model loop rules (M10.3)

The point of this milestone is that a 2–4B local model can drive a turn without the loop turning its
weakness into a parse error or a runaway. The knobs are `[tools] only`/`hide`,
`agent.max_tool_calls_per_turn`, `agent.small_model`, and `[provider] strict_tool_arguments`; the
behaviour is `StopReason::ToolBudget`, `AgentEvent::Notice`, and the per-turn repeat cache in
`imp-core/src/agent.rs`. Traps that cost time:

- **The `[tools]` filter is applied in `ToolRegistry::all()`, last, not at registration.** It must
  run *after* `attach_catalog` and after the shadowing pass, or it would miss MCP tools and could let
  a server reintroduce a built-in name the operator hid. `hide` beats `only`. This is narrowing only:
  it must never touch `Risk` or the gate — a test asserts a hidden `run_command` is gone, not softened.
- **A `finish_reason: length` tool-call batch is never committed to history.** The loop drops it
  before `Message::assistant_with_tool_calls`. Committing it would leave `tool_calls` ids unanswered,
  so the *persisted* session would be rejected by the provider on resume — the one invariant the loop
  cannot break. The notice goes in as a `system` message and the loop asks again.
- **The dedup cache must be cleared *before* a state-changing call runs, and the current result
  inserted after it.** Clearing after would drop the entry that was just inserted, so two identical
  `grep`s would both re-run. "State-changing" is `!Risk::is_observation()` — the same read/write line
  D15 draws, so a `ReadOnly` read is cached and a `Network`/`Write`/`Execute` call voids the cache.
- **A budget-refused call is still answered.** When `max_tool_calls_per_turn` is hit mid-batch the
  remaining calls get a `tool` message carrying the budget error, not silence. Dropping them would
  leave the assistant message's ids dangling and corrupt the stored transcript.
- **`StopReason` and `AgentEvent` gained variants (`ToolBudget`, `Notice`).** Both matches are
  exhaustive; add the arm in `render.rs` (prose and `--json`) and anywhere else that matches on them.
  `AgentOptions` gained `max_tool_calls_per_turn`, which is set at every construction site, tests
  included — the compiler is the checklist.
- **`ToolSchema::to_wire` now delegates to `to_wire_strict(false)`.** Keep the envelope test: the flat
  form still 400s on a compliant provider, and `strict` must sit *inside* the `function` object.
- **The small-model rules are appended, so a test asserts the prompt `ends_with` them.** If you move
  them earlier "for emphasis", that test is the one telling you a resumed prompt's tail changed.

**D45 additions.** `--lean` fills a profile over the knobs above; the loop gained a token budget, a
tool-result cap, argument repair, one bounded nudge and provider wire compatibility. Traps:

- **`--lean` is a global flag, not a config layer.** It is applied in `main.rs` after the file and the
  other flags, and `Config::apply_lean_profile` fills each key *only where it is still at its default*
  (an explicit `context_tokens`/`max_tokens`/idle timeout is kept). Do not reintroduce a TOML base
  layer; the precedence is "flag beats file", like every other global.
- **The token budget is enforced inside `Agent::run`, before every provider call.** `trim_to_budget`
  drops whole oldest turns and lands the cut on a `user` boundary — the same invariant as
  `apply_history_window`: never an assistant `tool_calls` without its results. A load-time-only trim
  cannot see the tool results that accumulate mid-turn. If the system prompt plus schemas alone exceed
  the budget, the turn fails with an explanation instead of sending a request that will be rejected.
  `imp_core::tokens` is a heuristic and must stay dependency-free (no tokenizer in `imp-core`).
- **`repair_arguments` runs only after a normal parse fails.** `imp_core::args::repair_json` strips
  fences and trailing commas and must return `None` when nothing changed, so the original parse error
  survives. Never pre-emptively rewrite well-formed JSON.
- **`nudge_on_empty` fabricates exactly one turn** — one per user turn, subject to `max_iterations`,
  inserted as a marked `user` message. It is the only place the loop speaks on the user's behalf.
- **`sanitize_schema` is lossless for tool calling.** It removes `$schema`/`title`/`format`, collapses
  `["T","null"]` to `T` and closes an object, keeping `description`/`enum`/`required`/bounds. Applied
  in `OpenAiProvider::body`, gated by `provider.quirks.sanitize_schemas`.
- **`[provider.extra_body]` cannot shadow a core request key** (`model`, `messages`, `stream`,
  `tools`, `tool_choice`, `stream_options`); `Config::validate` rejects a collision. Sampling only.
- **A non-conforming stream is tolerated, an empty one is not.** An object-shaped `arguments`, or one
  unparseable frame, must not end the turn; a stream that yields no events at all must be an error.
  The idle timeout is configurable (`provider.stream_idle_timeout_secs`) because local prefill is slow.

## Cron rules (M4)

- **A cron run uses a *different* gate, and it must stay different.** `setup::build_cron_gate`
  builds a `PolicyEngine` with `interactive = false` and **no `ApprovalUi`**, and deliberately no
  guard. That is what makes a job unable to promote an `ask` tool to `auto`, unable to reach the
  System One model (there is no prompt to resolve), and unable to add or delete jobs unless an allow
  rule names it. Never wire the session's interactive gate into `JobAgentRunner`; a prompt that
  appeared at an arbitrary moment would be approved by whoever happened to be mid-keystroke. See
  D19 and §11's resolved question 2.
- **A fire consumes its occurrence.** `next_run_at` is advanced in the same transaction that inserts
  the `job_runs` row, and recomputed again on completion from the completion instant. Advancing only
  on completion would make a job whose run outlasts its own interval look due on every tick, and
  every tick would then write an `overlap` row.
- **The `job_runs` row exists before the prompt is dispatched** (NFR-6). `Recorder` in the scheduler
  tests asserts it; if you move the dispatch earlier, that test is the one that should fail.
- **`skipped` and `overlap` are different statuses for different reasons.** `skipped` is a catch-up
  policy decision, `overlap` is a live run colliding with its own next occurrence. `queued` is an
  accepted run waiting for a slot. Don't collapse them — `/cron` and the audit trail read them.
- **The cron expression is a five-field Vixie expression, and `imp_cron::schedule::expression`
  is the only place that is true.** The `cron` crate is six-field Quartz with `1` = Sunday, so the
  parser prefixes `0 ` and rewrites the day-of-week field. Any new caller must go through that
  function, not `Schedule::from_str`; a direct call would silently fire a day late. See D18.
- **`jobs.session_id` is a foreign key.** A `reuse` job's session must exist before it is adopted,
  which is why the runner creates the session and the scheduler adopts it in a statement separate
  from the run's terminal status: an adoption that fails must not roll back the record that the run
  finished. `cron_end_to_end.rs` has a test for exactly that.
- **Job timestamps are written with `imp_core::job::stamp`**, fixed-width RFC 3339 with
  milliseconds and a `Z`. `due_jobs` compares `next_run_at <= ?` as *text*, so mixing formats would
  make the comparison a lexicographic guess. Use `stamp`/`parse_stamp`, never `to_rfc3339()`.
- **Catch-up replays occurrences without the overlap check.** A replayed occurrence never ran, so
  there is no live run to collide with; `max_concurrent_jobs` and `missed_run_cap` are what bound the
  burst. Adding the overlap check there would silently drop `run_all` occurrences after the first.
- **The scheduler lives and dies with the process** (D5). `cron::start` runs `reconcile()` — which
  closes runs a dead process left `running` — then `catch_up()`, then the tick loop. Jobs do not run
  while imp is closed (R6), and no daemon is left behind.
- **`--cwd` is both a global flag and a `cron add` flag**, so the subcommand's own definition wins
  inside `imp cron`. The job's `cwd` is canonicalized before it is stored: a relative path would
  otherwise be resolved against whatever directory the *scheduler* happened to run in.
- `imp-cron` has **no network and no database dependency**: the store is behind `imp_core::JobStore`
  and the prompt behind `imp_cron::JobRunner`. Keep it that way — it is what lets the firing rules
  be tested against an in-memory fake and a `ManualClock`.

## MCP client rules (M5)

- **An external tool is a `Tool`, and that is the whole security story.** It goes through
  `ToolRegistry` and `Agent::dispatch` like `run_command`, so it passes `ToolGate::check` before it is
  invoked. If you ever add a way to reach a server that is not a registered tool, you have added a
  path around the gate — `mcp_call` was left unimplemented for exactly that reason (see the known
  gaps).
- **External tools are always `Risk::Network`, and the annotations are ignored.** MCP states risk in
  `ToolAnnotations`, which are hints from a server the operator has not vouched for; the protocol's own
  docs say a client must not make tool-use decisions from them. With `Network` as the floor, honouring
  a hint could only ever *lower* the class. Don't "use `readOnlyHint` to skip the prompt" — that hands
  the decision to the thing being decided about (D21, T6).
- **The catalogue is read live, from a frozen registry.** `ToolRegistry` holds registered tools plus
  any number of `ToolCatalog`s, and `all()` is consulted on every `get`/`schemas`/`risks`. That is what
  lets a server that came up mid-session be offered on the *next turn* without rebuilding anything the
  session already holds. `crate::tool::tests` pins the dedup order; `McpServers` is the only
  implementation today.
- **`Tool::name()` is `&'static str`, so runtime names are interned, not leaked per reconnect.**
  `McpServers` keeps one `Box::leak` per distinct name and description (`Interner`). A server that
  flaps must not allocate a fresh copy every turn. Changing `Tool::name` to `&str` would remove the
  leak, and would touch every tool — not worth it for this.
- **`refresh(OnStart::Eager)` while assembling, `refresh(OnStart::All)` per turn.** The first skips
  `lazy` servers (that is what `lazy` means: don't spawn a process for a session that never takes a
  turn); the second is the retry §5.10 promises and must run before the turn's first provider call, or
  a server that came back is not in the catalogue the model is offered. `setup::Session::refresh_mcp`
  is the entry point; `JobAgentRunner::execute` calls it too, because a cron run is a turn.
- **Notices are emitted on transitions only.** `refresh` compares the new state against the previous
  one and stays quiet when nothing changed, so a server that is down for ten turns produces one
  `system` message, not ten. Keep the comparison in the notice path — the WARN log is separate and
  repeats on every attempt on purpose.
- **A `system` notice goes into the transcript, after the prompt.** Index 0 of `history` is the system
  prompt and must stay there; notices are appended after it (`setup::build`, `run.rs`, `repl.rs`).
- **The REPL's second `persist_since` takes an explicit index.** It used to be `turn_start + 1`, which
  was correct only while exactly one message was pushed before the turn. A notice makes that two, and
  the off-by-one duplicated the user's prompt in the database. `reply_start` is now captured from
  `history.len()`.
- **`imp` closes its MCP connections on the way out.** `Session::shutdown` → `McpServers::shutdown`
  → `McpClient::close`, called from `run::one_shot` and `repl::interactive`. Without it a REPL exit
  can leave a server process behind, because the transport's kill runs from a spawned task that a
  shutting-down runtime may never schedule. The short sleep in `shutdown` is there for the same reason.
- **`rmcp` is quieted at the default verbosity.** `init_logging` appends `,rmcp=warn` when `-v` was not
  passed; the library logs an INFO line per service init, cancellation and shutdown, and stderr is the
  channel the REPL uses for tool activity. `-v` leaves it alone so a protocol problem is still
  debuggable.
- **`imp-cli::setup::recording` wraps every gate in a `RecordingGate`.** §7 asks for an audit row
  per tool decision, and the engine only wrote one where a guard verdict was involved — so an allowed
  `read_file`, or any external call at all, left no trace. It is a decorator on purpose: the rule order
  in `PolicyEngine::check` is the security property, and wrapping records without touching it.

## MCP server rules (M6)

- **`mcp serve` is a terminal mode.** It owns stdin/stdout for the protocol and returns `ExitCode`
  when the peer closes; there is no path from it to `repl::interactive`. That *is* R5 — not a flag
  that has to be checked, but a control flow that cannot reach the REPL.
- **The surface is a `ToolRegistry`, and the gate is the same engine.** `ImpServer::dispatch`
  resolves a tool, calls `gate.check(policy_name(name), tool.risk(), args, subject)` and then
  `invoke`, exactly as `Agent::dispatch` does with a model in front of it. If you add a tool to the
  surface, give it a risk class and a schema; do not add a call path that skips `dispatch`.
- **`policy_name` maps `agent_run_command` → `run_command` and `agent_write_file` → `write_file`.**
  This is load-bearing, not cosmetic: the engine's classifier, `subject_for` and `pattern_for` all key
  on the *real* tool name, and an operator's existing `run_command` deny rules must protect the MCP
  surface. The `agent_` prefix is a naming convention of this surface, not a second identity (D22).
  The audit row names the tool that actually ran.
- **A disabled capability is listed and refused.** Without `expose_exec`/`expose_write`, the gate
  carries a deny rule (`run_command`/`write_file` → `*`) rather than the tool being absent. The
  refusal reaches the caller as `CallToolResult::error`, which is a tool failure the host can read —
  not `Err(McpError)`, which MCP clients render opaquely.
- **The gate is built with `interactive = false` and an enabled family gets `ToolPolicy::Auto`.**
  stdin is the protocol pipe, so no prompt can ever be shown; a flag is the operator's consent, and it
  substitutes the non-interactive fallback for that one family (D21). Deny and allow rules still run
  first, so turning a flag on grants nothing that was already refused. Never make the server's gate
  interactive, and never build it from the session's `ApprovalUi`.
- **`agent_ask`'s tool set is filtered by `Risk::is_observation`, not by a list of names.**
  `default_registry` is built and then reduced to its read-only tools, so the inner surface cannot
  drift wider than the read/write line itself. `allow_tools` narrows further, and an unknown name is
  dropped (fail-closed, like `tool_allow`).
- **A host-supplied `session_id` is honoured.** An unknown id creates a conversation under that id, so
  a caller can keep one across calls without a round trip. `history[0]` is always the system prompt;
  the fresh/resumed split decides whether the whole transcript or just the turn is persisted.
- **`config-redacted` redacts by header *name*.** The repo invariant is that a config never contains a
  secret, but `[provider].headers` is hand-writable and may carry an `Authorization` value. Anything
  whose name contains `authorization`/`auth`/`token`/`key`/`secret`/`cookie`/`password`/`credential`
  becomes `<redacted>`; the env var *name* and file *path* stay, because they are not secrets.
- **The scheduler is not started by `mcp serve`.** A job created over MCP is stored and runs under the
  non-interactive cron gate the next time a process with a scheduler opens the database. `mcp serve` is
  a protocol server, not a daemon; starting the scheduler inside it is a separate decision.
- **The tests drive a real MCP connection over an in-memory pipe.** `serve_server` must be
  `tokio::spawn`ed, never awaited, before the client is built: it completes the handshake before
  returning, and it cannot handshake with a client that does not exist yet — awaiting it deadlocks.
  The fake provider is injected through `Runtime::build`'s `ProviderFactory`, so no network is needed.

## Audit and redaction rules (M7)

- **One row per tool call, written once.** `RecordingGate` writes a refusal as it happens
  (`outcome = denied`), but holds an *allowed* decision until `ToolGate::record_outcome` reports how
  the call ended. The row therefore carries `decision` **and** `outcome`/`duration_ms`; writing the
  allow row at `check` time would either lose the result or duplicate the row. `Agent::run` stamps
  `begin_turn(session_id, turn_id)` once per turn, and `Agent::dispatch` times the invoke — if you
  add a tool-call path, it must go through `dispatch` or its audit row never gets an outcome.
- **`record_outcome` only finishes the row for the tool that was allowed.** A pending row for
  `read_file` is not credited to a `write_file` outcome; it is dropped rather than mislabelled.
- **The audit write is best-effort and never fails the turn** (`ApprovalStore::audit` swallows the
  error). Do not make the audit a precondition for running a tool.
- **Redaction is a property of the sink, not the call site.** `logging::RedactingMakeWriter` scrubs
  every line: the resolved API key, the configured env var's *value*, secret-shaped header values,
  and the token after any `Bearer`. A new `tracing` call cannot leak by forgetting to be careful —
  but a **new sink** must go through the same writer, or it will.
- **`logging.file` is scrubbed too.** The writer tees to both stderr and the file, redacting once.
- **A bare `[redacted]` marker is the only thing written**; never log the raw value and redact later.

## Packaging rules (M7)

- **`--version` is assembled in `version.rs` from stamps `build.rs` writes.** It prints
  `<version> (<git sha> <commit date>) [features: cron,guard,mcp,update]`, and `imp update` parses
  exactly that line back out of a downloaded binary, so the shape is a contract, not cosmetics: M7
  shipped a second shape (`(<sha>; features: …)`) and merging the two milestones left one.
  Never hard-code the SHA; a source tarball with no `.git` reports `unknown` on purpose.
- **`build.rs` must list every path that moves with a commit.** `.git/HEAD` holds a ref *name* on a
  branch, not a commit, so watching it alone leaves the baked SHA stale on every incremental build —
  the release then names the wrong commit. It watches `.git/HEAD`, `.git/refs` (a directory: cargo
  re-scans it) and `.git/packed-refs`, plus `IMP_GIT_SHA`/`IMP_GIT_DATE`/`GITHUB_SHA` so the
  release workflow can inject the commit. `cli_surface.rs`'s
  `version_reports_the_git_sha_and_enabled_features` compares against `git rev-parse --short=12 HEAD`
  so a stale SHA fails the suite instead of shipping.
- **The `mcp` and `cron` cargo features are compile-time flags reported by `--version`, but they do
  not yet remove code**: both default on, and today a `--no-default-features` build still contains
  both subsystems. That is a documented gap (see "Known gaps"), not a lie: the feature list states
  what the build was *configured* with. Do not claim a reduced build until the cfg-gating is real.
- **Config and database paths are created `0600`/`0700`.** `write_private_file` and `Store::open`
  tighten permissions *before* content is written; `doctor` warns when a private file is group- or
  world-readable. Keep the secret out of the config file, always.
- **`make dist` is the release layout** `install.sh` expects: `dist/imp-<version>-<target>.tar.gz`
  plus a `.sha256`. The Linux musl targets need a musl C compiler — see `NFR.md` for the no-root
  recipe used on this host.

## Context and compaction rules (D49/D50)

The window and the summary are two halves of one problem: a context that is too small is only
survivable if imp knows how small it is, and history that no longer fits is only usable if it is
summarized rather than dropped. Traps that cost time:

- **An explicit `provider.context_window` skips the probe entirely.** Precedence is config, then the
  endpoint, then nothing. `imp-provider::context` asks `/models` (vLLM), `/api/v0/models` (LM Studio),
  `/props` (llama.cpp) and `/api/ps` (Ollama), and the last three only for a **loopback** base URL —
  the origin-level endpoints are local-server conveniences, and a hosted gateway should never see a
  `/props`. Every probe is bounded to two seconds and swallows its failures: discovery degrades to
  configuration, it never fails a session.
- **Discovery turns budgeting on, and reserves generation room.** With `agent.context_tokens` unset,
  `setup::effective_budget` derives `window - max(max_tokens, 512) - 5%`. The generation part is not
  padding: a reasoning model spends its window *producing* the answer, so reserving only for the
  prompt leaves a mid-generation overflow that looks like an empty reply. Do not "simplify" the
  reserve to a fixed margin.
- **Compaction is triggered by `compact_threshold`, not by the hard limit, and the trigger is
  authoritative.** `Agent::compact_if_needed` runs `compact::plan_forced` once the threshold is
  crossed; using `plan` there would re-test the budget and never fire, because the threshold sits
  below it. `plan` is for `/compact`-style "does it fit" callers only.
- **A summary is never allowed to cost more than what it replaces.** Two guards: `MIN_COMPACT_TOKENS`
  avoids the guaranteed-losing trade of summarizing a few short messages, and the post-check discards
  any summary whose estimate is not smaller than the replaced range. Without the second one,
  compaction can grow the context — verified live: 187 → 224 tokens with `keep_turns = 1`.
- **`trim_to_budget` protects the summary.** A summary sits at index 1, exactly where trimming starts
  dropping, so `trim_to_budget` treats a summary at index 1 as fixed and drains from index 2. Reversed,
  the first trim after a compaction throws away the only record of everything already dropped.
- **Compaction shifts indices, and every caller has to know.** `TurnOutcome.compacted` is the net
  number of messages removed. Three places persist from an index captured before the turn —
  `repl.rs`'s `reply_start`, `run.rs`'s `turn_start`, and the `start` in the cron and MCP-server
  runners — and each subtracts it. Forgetting one writes the turn twice or drops it. The unit test
  asserts the contract as `history.len() == before + 1 - compacted`.
- **The stored transcript is never rewritten (R3).** Compaction replaces messages in the in-memory
  history only; SQLite keeps every row, so a resume re-derives the state instead of trusting a
  rewritten log. That is why `messages.is_summary` has no writer: persisting a summary in position
  would need a way to mark the rows it supersedes, and an append-only log cannot express that without
  a schema change.
- **The summary role is a provider quirk, not a decision the loop makes.** Default `system` with a
  `[summary]` marker; `[provider.quirks].summary_as_user` exists for chat templates that accept one
  system message at the start. The role is chosen where the message is built, so flipping the quirk
  never needs a migration.
- **Compaction runs at the turn boundary because local backends serialize requests.** LM Studio and
  `llama-server` serve one request at a time, so a "background" summarization fired mid-turn queues
  behind the live generation and doubles its latency. Boundary compaction gets the same benefit —
  the user is reading, not waiting — with no concurrency to get wrong.
- **A failed summary is a notice, never a failed turn.** The loop falls back to `trim_to_budget`, which
  is exactly the pre-compaction behaviour. `imp-core` does the same with no summarizer installed, so
  the loop's contract does not mention compaction at all.

## Terminal presentation rules (D47)

- **Every escape and every glyph comes from `imp-cli/src/style.rs`.** The palette is 16-colour SGR (`style::code`), the glyph table is `style::glyph`, and the runtime decision is `Theme`. `NO_COLOR`, `--no-color`, `[ui].color` and `--json` are resolved in one function (`style::colour`); a new sink that writes its own escape has opted out of all four. Keep `--json` a hard no.
- **Colour is never the only signal.** `Theme::risk` appends `!` to a consent-requiring class when colour is off, and every glyph has an ASCII fallback; `[ui].icons = "none"` must leave the output readable. Do not add a mark whose meaning exists only as a hue.
- **The theme is built per stream.** Conversation output (stdout) uses `style::stdout_theme`, tool activity and approvals (stderr) use `style::stderr_theme`; `run::style_for` takes both the flag and the config so `[ui].color` is honoured. `--no-color` and `NO_COLOR` beat `[ui].color = "always"`, and `style::stdout_theme_default`/`stderr_theme_default` exist for `imp init`, which runs before a config is loaded.
- **Tool lines carry risk and duration.** `AgentEvent::ToolStarted` has `risk`; `ToolFinished` has `duration_ms: Option<u64>` — `None` means the call never ran (a repeat answer or a budget refusal) and is the only case that omits the time. Add the fields at every construction site; the matches are exhaustive.
- **The spinner is on by default and erase-only.** `render::Activity` owns one stderr mark via `\r`, serialised against the renderer by a mutex and stopped by the first event. It must stay gated on `[ui].spinner` **and** a TTY **and** not `--json`/`--quiet`. It never revises printed output — the scrollback rule of D2 still holds.
- **A resumed conversation prints its history, and the approval prompt collapses.** `--resume`/`--continue`/`session resume` print the non-system, non-tool turns on start unless `--no-history`/`--quiet`, shaped like the live REPL (`›` prompts, bare replies, tool calls indented); the approval questionnaire is erased and replaced by one `▸ tool · approved (once)` line once answered, on a terminal only (a redirected stderr keeps the full prompt). The erase counts newline-terminated lines and whether the answer ended in `\n` — do not assume the user pressed Enter.
- **`[ui]` is cosmetic.** `imp_core::config::UiConfig` has no security meaning; the gate, `doctor` and the system prompt do not read it. A new `[ui]` key must not become a policy switch.

## Memory rules

- **`recall` quotes every search term.** Model-written text reaches FTS5's `MATCH` directly, and
  FTS5 reads a bareword `OR`/`AND`/`NOT` as an operator. Unquoted, a query like `alpha OR beta`
  silently *widens* from "both words" to "either word" and returns rows the caller never asked for.
  `search_terms` reduces each word to alphanumerics and wraps it in quotes; the phrases are then
  implicitly `AND`ed. Don't "simplify" that away.
- **A `Transaction` must be committed explicitly** — it rolls back on drop. Omitting the `commit()`
  after an upsert makes `remember` report success while `recall` matches nothing, which is exactly
  the silent failure the FTS triggers exist to prevent. `crates/imp-cli/tests/memory_gate.rs`
  and the store tests catch this; if you touch the write path, expect them to fail first.
- **The namespace is a hash, not a path.** `namespace_for` uses FNV-1a rather than a digest
  dependency, and `the_label_is_sixteen_hex_digits` pins the algorithm. A test asserts known
  constants on purpose: if the hash changes, every workspace's memory becomes unreachable.
  Canonicalize the root before hashing, or `/ws` and `/ws/` become two namespaces.
- **A no-match `recall` is a plain answer, not a tool error.** The model asks vague questions; an
  error there reads as a broken tool. Same for a query with nothing searchable in it.
- `remember` is `Risk::Write` even though it cannot touch the workspace. The class describes what
  changes, and that is what puts it behind the gate. `crates/imp-cli/tests/memory_gate.rs`
  asserts a denied `remember` leaves the database untouched.

## HTTP rules

- **The domain allowlist and the approval allowlist are two different lists.** `[http_fetch]
  .allowed_domains` is the SSRF boundary: a host that does not match is refused by the tool itself,
  whatever policy says. `[policy.allow]` is what pre-approves, and `http_fetch` reports the URL
  *host* as its `approval_subject`, so an allow rule names a domain
  (`{ tool = "http_fetch", pattern = "docs.rs" }`). An allowlisted domain still prompts on a TTY and
  is still refused in a pipe until it has a policy allow rule; neither list widens the other. This
  is a deliberate deviation from §5.5's "`auto` if domain allowlisted" approval row — see D17, which
  records it for the spec owner to confirm.
- **`http` is granted only by an *exact* entry, never by a wildcard.** `https` is the default; a
  plain `http://` URL is accepted only when an entry with no `*` names the host exactly
  (`allowed_domains = ["localhost"]`), because §5.5 and §11.1 both say "explicitly allowlisted".
  `local*` reaches `localhost` over https, not over http: a glob is a reachability rule, not a
  waiver of the scheme. A test would not have caught this from an exact-entry case alone, which is
  why `a_wildcard_entry_grants_https_but_never_plain_http` exists.
- **Hosts are compared lowercased and with the IPv6 brackets stripped**, so an entry `::1` names
  `http://[::1]/…`, which is how `Url::host_str` renders it. Both `host_allowed` and
  `host_named_exactly` go through the same normalisation, so the two agree on what a host is.
- **The guard fails closed.** An empty `allowed_domains` reaches nothing. `block_private_ips`
  defaults on and refuses private, loopback, link-local, unique-local, CGNAT, unspecified and
  multicast ranges, the cloud-metadata `169.254.169.254` included. Loopback is refused *even when the
  host is allowlisted*, because the two lists narrow independently.
- **Redirects are followed by hand, one hop at a time**, with `Policy::none()` on the client, because
  each hop has to be re-checked. A hop to a host outside the allowlist fails as
  `redirect refused: …` rather than being followed. `301/302/303` rewrite a non-GET to `GET` and drop
  the body; `307/308` preserve both.
- **DNS is resolved before the request and every returned address is checked.** This is a pre-flight
  check, not a connection-time one, so a resolver that answers differently between the check and the
  connect (DNS rebinding) is not closed by it. Closing that needs a custom `reqwest::dns::Resolve`;
  it is a known limit, not an oversight.
- **`http_fetch` is why `imp-tools` depends on `reqwest`.** It is the same crate and version
  `imp-provider` already links, so it adds one edge to `Cargo.lock`, not a second HTTP stack.
- The body is read up to `max_bytes` (clamped per call) and non-UTF-8 is decoded lossily. The result
  the model sees is JSON — `status`, `url`, `redirects`, `headers`, `body`, `truncated` — matching
  §5.5; the tool `metadata` mirrors the status and the kept byte count.
- Model-supplied headers are dropped when they do not parse or are client-owned (`host`,
  `content-length`, `connection`, `transfer-encoding`).

## Store rules

- **Migrations are a forward-only list.** Append a new `&str` to `MIGRATIONS`; never edit an applied
  one. The loop must run *that entry's* SQL, not the baseline.
- **A `query_map` closure may only return `rusqlite::Result`.** Decode columns into a small local
  struct and convert after `collect()`, or the `?` on a domain error fails to compile.
- **All store I/O goes through `Store::blocking`** (a `spawn_blocking` hop). Calling rusqlite
  inline would stall the async runtime for the duration of the query.
- **A turn is persisted as one transaction.** Never append messages one at a time across statements:
  a tool result without its assistant call is an invalid transcript.
- **Trimming must never split a turn.** `apply_history_window` keeps index 0 and advances the cut
  off a `tool` role. The invariant to test is "every `tool_calls` entry is answered", not "there
  are no tool messages" — a paired result is legitimate.
- `memory_fts` is an external-content FTS table; its triggers are what make `recall` work in M3.
  Don't drop them.

## Known gaps (don't mistake these for bugs)

- The CLI surface of §5.12 is now complete: `run`, `init`, `config show|path`, `doctor`, `session`,
  `cron`, `mcp list` / `mcp tools` / `mcp serve`, and the default REPL, plus the global flags
  (`--quiet/-q`, `--resume`, `--max-iterations`). What is *not* built is a daemon that runs cron jobs
  while no session is open — cron is in-process by design (D5/R6).
- **The `mcp`/`cron` cargo features do not gate code yet.** They exist and `--version` reports them,
  but a `--no-default-features` build still compiles both subsystems; the cfg-gating across
  `setup.rs`/`run.rs`/`repl.rs` is deferred. Treat the feature list as "how the build was
  configured", not "what was compiled out".
- **`imp doctor` needs the provider to answer to be fully green.** `/models` is optional in the
  OpenAI-compatible world, so a non-auth 4xx counts as reachable; an endpoint that answers nothing
  useful on `/models` is still reported as up. The probe is a reachability and credential check, not
  a capability check.
- **`mcp_call` is not implemented.** §5.5 lists a generic `mcp_call(server, tool, arguments)` escape
  hatch. The per-server approval policy keys on a tool *name*, so a generic tool would either have to
  be special-cased to read `args.server` for its decision, or would let a server marked `deny` be
  reached anyway. The flattened `mcp__<server>__<tool>` tools are the surface that ships; a caller who
  wants the compact form needs a decision about how it is gated first (D20).
- **A `lazy` server's tools are not in `/tools` until the first turn.** Laziness defers the spawn to
  the first turn, and tools cannot be discovered without a connection, so a REPL shows only the
  built-in tools until someone asks for something. `imp mcp list`/`mcp tools <server>` start it
  immediately.
- **The per-server policy is only enforced on the flattened tools.** A server whose `approval` is
  `deny` refuses `mcp__<server>__*`; nothing else can reach it, because `mcp_call` does not exist. If
  that tool is ever added, it has to honour the same family policy or the `deny` becomes decorative.
- **The cron tick loop is verified by unit tests on a virtual clock and by one manual end-to-end
  run, not by an automated integration test against a real clock.** `scheduler.rs` drives every
  firing rule through `tick()`/`catch_up()` directly with a `ManualClock`, and
  `cron_end_to_end.rs` exercises the real SQLite store, but no test waits for a wall clock to pass a
  minute boundary. The manual run that was performed is described under "Manual smoke test".
- **`/cron` in the REPL and the scheduler's start/stop wiring are manually verified.** The
  subcommand (`imp cron add|list|remove`) is the tested surface; the REPL path has no automated
  test, the same limitation as the interactive approval prompt.
- **`--yes`/`--deny` do not move the MCP surface.** The disabled families are deny *rules*, which are
  rule 1 and beat the default a flag would set, and the read-only tools are allowed before the
  fallback is ever consulted. The only thing that widens the surface is `[mcp.server].expose_exec` /
  `expose_write`. Passing `--yes` to `mcp serve` is therefore inert rather than dangerous, but it is
  also not a way to enable anything.
- **`mcp serve` does not run the scheduler.** A job created over MCP (`cron_add`) is stored in the
  session database and runs the next time a process that *does* start a scheduler — `imp run`, the
  REPL, or a future `doctor`/daemon — opens it. The server is a protocol endpoint, not a daemon (D22).
- **`mcp serve` ignores `[mcp.client.servers.*]`.** The server's inner agent is the read-only subset
  of the built-in registry, and external tools are `Risk::Network`, so they would never be offered
  anyway; the client half is not assembled. A future widening of `agent_ask` beyond read-only has to
  decide whether external tools join it.
- **`agent_ask`'s inner agent is read-only, always.** `expose_write`/`expose_exec` add the *direct*
  tools `agent_write_file`/`agent_run_command`; they do not widen what `agent_ask` can do. That
  matches §5.9's "read-only tool subset", and it means a host that wants a write must call the write
  tool explicitly rather than ask the agent to do it.
- **M3.5's guard policy is tested, its interactive wiring is not.** `imp-core` unit-tests the
  floor, the thresholds and the engine (with a fake guard) and `imp-guard` runs the real HTTP
  client against a fake `/v1/systemone` server inside the real engine, but no test drives a real
  keystroke through `imp` with the guard enabled — the same limitation as the approval prompt.
- **The `/v1/systemone` response contract is ours.** The SDD says the model returns calibrated
  numbers; it does not say which field carries one. This implementation reads a top-level `unsafe` in
  `[0, 1]` and treats anything else as unreadable, which prompts. If a real vendor shape differs, only
  `parse_verdict` in `crates/imp-guard/src/lib.rs` changes.
- The guard is a **noise filter, not a boundary**: it resolves prompts strictly inside the boundary
  the static rules drew, and it is off by default.
- **The interactive approval keystroke path is not machine-tested.** The prompt renders correctly and
  `parse_choice` plus all four engine outcomes are unit-tested, but a pty harness kept
  desynchronising, so no test drives a real keypress end to end. Treat it as unverified.
- Tool calls execute **sequentially**; bounded concurrency is tracked as `FR-8` in `agent.rs`.
- `usage` is advisory and may be zero; no turn is blocked on it.
- History trimming (`history_window`) is applied when a transcript is reloaded; compaction is not, and
  a resumed session compacts again on its first turn. Compaction is a view over history, never a
  rewrite of the stored transcript, so resuming costs one summarization request rather than risking a
  corrupted log.
- **A single oversized turn cannot be compacted**, only dropped or capped: keeping the newest turn
  verbatim is what keeps a summary from replacing the conversation the model is in the middle of.
  `agent.tool_result_chars` is the control that bounds one huge tool result; compaction handles the
  history around it.
- The provider is only smoke-tested against a stub server. There is no recorded provider fixture
  suite yet (SDD §8).
- **The markdown renderer is verified by unit tests and one manual pty pass, not fixtures.** Column
  widths are measured with `chars().count()`, so CJK and emoji will be misaligned in tables. A
  `unicode-width` dependency would fix it and would cost binary size; it is a deliberate deferral,
  not an oversight.

## `SDD.md` is the source of truth

Normative, not prose. When implementing, follow it rather than substituting defaults.

- **Appendix C (decision log) is binding.** D1–D8 are settled: Rust (not Zig), line REPL (not
  full-screen TUI), approval+allowlist (not sandbox-first), SQLite, in-process cron, MCP in both
  directions, thin hand-rolled provider client, read-only-by-default MCP surface. To deviate, add a
  new decision-log entry — don't silently diverge.
- **§11 open questions are unresolved.** Four items are marked *for review*. Don't pick answers
  without confirming first.
- **Keep IDs stable.** `G*`, `FR-*`, `NFR-*`, `T*`, `R*` are cross-referenced across sections.
  Renumbering breaks references; append instead.
- §10 milestones are the roadmap. §2 requirements are the acceptance criteria for `imp-core` and
  the tools layer; §5 is the implementation detail.

## Non-goals for v1 — do not build these

No subagents or recursive task spawning, no embeddings/vector search, no full-screen TUI panes, no
browser automation, no multi-user server or auth, no local inference server. Exactly six tool
families (§5.5): `run_command`, `cron_*`, `read_file`/`write_file`/`edit_file`, `http_fetch`,
`mcp_call`, `remember`/`recall`. Don't add tools beyond these without a decision-log entry.

## Manual smoke test

The end-to-end check is a throwaway Python stub that speaks SSE like a compatible provider: a
`read_file` tool call on turn 1, prose on turn 2, `GET /v1/models` for `init --check` and discovery,
and every request's headers appended to a log. It is intentionally **not** committed as a test
dependency. Point `--base-url` at such a stub rather than at a real provider.

The stub doubles as a check on the parts unit tests cannot reach: inspect its header log to confirm
`${session}` produced one stable id across a conversation, and that `Authorization` carries the key
from the credentials file.

**Interactive `init` needs a real pty.** The token prompt reads `/dev/tty` with echo disabled, so it
cannot be driven by piping stdin — that path takes the non-interactive branch. Drive it with
`pty.fork()` and write one answer per prompt; feeding all lines at once desynchronises the prompts
and produces a confusing failure. Always rebuild the binary (`cargo build`) before a manual test:
`cargo test` does not refresh `target/debug/imp`.

### Cron end to end

The cron path needs a process that lives long enough for a real minute boundary, so it is driven with
the REPL and piped stdin rather than a pty. With the same SSE stub as above:

```sh
# 1. a job that fires on the next minute boundary
imp --cwd /tmp/e2e --db /tmp/e2e/imp.db \
  cron add --schedule '* * * * *' --prompt 'say hello' --name e2e --timezone UTC

# 2. let the scheduler tick past one occurrence, then leave
( sleep 70; echo '/quit' ) | imp --cwd /tmp/e2e --db /tmp/e2e/imp.db

# 3. one occurrence passes while imp is closed, then catch up
sleep 65
( sleep 12; echo '/quit' ) | imp --cwd /tmp/e2e --db /tmp/e2e/imp.db
```

What to check, and what was checked:

- The stub's request log gains one entry per run, with the first user message reading
  `[cron:e2e] say hello` — the synthetic tag.
- `jobs.runs_count` counts the runs; `next_run_at` has moved past now; `last_status` is `ok`.
- `job_runs` has a row per run with a `finished_at` and an `exit_summary`, and none left `running`.
- Step 3 prints `… cron catch-up: 1 fired` on stderr, and the second run's row appears with a
  `started_at` at startup time rather than at the missed minute — that is `run_once` replaying one
  occurrence.
- Each run got its own session (`session_mode = new`), whose first message is the system prompt and
  whose system prompt says `Mode: non-interactive (approval is unavailable; …)`.

`python3` and `sqlite3` are enough to inspect the database; the throwaway helper used for this is a
three-line `sqlite3.connect` plus a `SELECT * FROM jobs` / `SELECT * FROM job_runs`.

### MCP client end to end

The client's unit tests spawn the stub server themselves, so this recipe is for the *binary* path: a
real `imp`, a real server process, a real gate and a real audit row.

```sh
# 1. the fixture server is a normal cargo bin
cargo build                       # produces target/debug/mcp-stub-server

# 2. a config that consumes it, plus one server that cannot start
cat > /tmp/m5/imp.toml <<'TOML'
[provider]
base_url = "http://127.0.0.1:8099/v1"
api_key_env = ""
api_key_file = ""
model = "stub-model"
[cron]
enabled = false
[mcp.client.servers.stub]
command = "/abs/path/target/debug/mcp-stub-server"
tool_allow = ["*"]
approval = "ask"
[mcp.client.servers.ghost]
command = "/nonexistent/mcp-server"
tool_allow = ["*"]
TOML

# 3. what the model would be offered, and what tool_allow hides
imp --config /tmp/m5/imp.toml mcp list
imp --config /tmp/m5/imp.toml mcp tools stub

# 4. a turn: the stub provider (the SSE stub above) asks for mcp__stub__echo
imp --config /tmp/m5/imp.toml --db /tmp/m5/m5.db run "call the echo tool" < /dev/null
```

What to check, and what was checked:

- `mcp tools stub` prints every tool the server lists, each with its flattened name, and marks the ones
  `tool_allow` hides (`·` rather than `▸`). The `inputSchema` is the server's, byte for byte — the stub
  carries an invented `x-stub-marker` and it survives.
- With `approval = "ask"` and no allowlist, a piped run **refuses** the call: the transcript gains
  `{"error":"denied by policy: …needs approval but nothing can answer a prompt…"}` and `audit_log` has
  a `deny` row for `mcp__stub__echo`.
- Add `[[policy.allow]] tool = "mcp__stub__echo" pattern = "*"` and the same run returns `pong` from
  the server, with an `allow` row in `audit_log`.
- Change the server to `approval = "auto"` and it runs unattended with no allowlist at all — the
  per-server policy substituting the global non-interactive `deny`.
- The unreachable `ghost` server costs only its tools: the turn completes, and exactly one `system`
  message in the transcript explains why. A second turn adds no second copy.
- `ps` shows no `mcp-stub-server` after any of those commands: the connections are closed on the way
  out.

### MCP server end to end

The automated test drives a real MCP client against a real server over an in-memory pipe
(`cargo test -p imp-cli --bin imp mcp_serve`), which is the surface to trust. This recipe is for
the *binary* path: a real `imp mcp serve` process, its own stdin/stdout, the SSE stub as the model.

```sh
# 1. a config whose provider is the SSE stub, with the default read-only surface
cat > /tmp/m6/imp.toml <<'TOML'
[provider]
base_url = "http://127.0.0.1:8099/v1"
api_key_env = ""
api_key_file = ""
model = "stub-model"
[cron]
enabled = false
[mcp.server]
expose_exec = false
expose_write = false
expose_cron_write = true
TOML

# 2. a throwaway MCP client: JSON-RPC over the child's stdin/stdout
python3 - <<'PY'
import json, subprocess, sys
p = subprocess.Popen(
    ["imp", "--config", "/tmp/m6/imp.toml", "--db", "/tmp/m6/m6.db", "mcp", "serve"],
    stdin=subprocess.PIPE, stdout=subprocess.PIPE)
def rpc(id, method, params=None):
    p.stdin.write((json.dumps({"jsonrpc":"2.0","id":id,"method":method,
                               "params": params or {}}) + "\n").encode()); p.stdin.flush()
    return json.loads(p.stdout.readline())
rpc(1, "initialize", {"protocolVersion":"2025-06-18","capabilities":{},
                      "clientInfo":{"name":"probe","version":"0"}})
p.stdin.write(b'{"jsonrpc":"2.0","method":"notifications/initialized"}\n'); p.stdin.flush()
tools = rpc(2, "tools/list")["result"]["tools"]
print([t["name"] for t in tools])
print(rpc(3, "tools/call", {"name":"agent_ask","arguments":{"prompt":"say hello"}}))
print(rpc(4, "tools/call", {"name":"agent_run_command","arguments":{"command":"echo hi"}}))
p.stdin.close()
PY
```

What to check, and what was checked:

- `imp mcp serve` prints the banner on **stderr** before the protocol starts: one line per
  `expose_*` flag, `surface: read-only` while both exec and write are off. Nothing but protocol ever
  reaches stdout, so the client's JSON parse never sees a log line.
- `tools/list` returns the nine tools in surface order (the golden snapshot), `agent_run_command` and
  `agent_write_file` among them.
- `tools/call agent_ask` returns `{"session_id":…,"stop":"completed","text":…}` — the stub provider's
  answer — and a `sessions` row appears in the database under that id.
- `tools/call agent_run_command` returns `isError: true` with `refused by a deny rule` in the content,
  and `audit_log` gains a `deny` row for `run_command`. Set `expose_exec = true`, restart, and the
  same call returns the command's output with an `allow` row.
- `resources/list` and `resources/read imp://config-redacted` work; the config JSON carries no
  secret even if `[provider.headers]` names one.
- `ps` shows no child process after the client closes stdin: the server exits when the peer closes.

`python3` and `sqlite3` are enough to inspect the database (`SELECT * FROM sessions`, `SELECT * FROM
audit_log`).
