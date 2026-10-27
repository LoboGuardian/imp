//! Core agent loop, message model, configuration, and tool abstractions.
//!
//! This crate deliberately has no terminal, network, or database dependencies so
//! that the loop can be exercised against a scripted mock provider.

pub mod agent;
pub mod args;
pub mod classify;
pub mod clock;
pub mod compact;
pub mod config;
pub mod error;
pub mod fs;
pub mod glob;
pub mod guard;
pub mod job;
pub mod memory;
pub mod message;
pub mod policy;
pub mod provider;
pub mod read_only;
pub mod session;
pub mod sha256;
pub mod tokens;
pub mod tool;
pub mod update;

pub use agent::{Agent, AgentEvent, AgentOptions, StopReason, TurnOutcome, trim_to_budget};
pub use args::{parse_tool_arguments, repair_json};
pub use classify::classify_command;
pub use clock::{Clock, ManualClock, SystemClock};
pub use compact::{Plan, SUMMARY_MARKER, Summarizer, SummaryRole};
pub use config::{
    Config, Credentials, CronConfig, GuardConfig, HttpFetchConfig, McpClientConfig, McpConfig,
    McpServerConfig, MissedRunPolicy, ProviderQuirks, ToolsConfig, UpdateConfig,
};
pub use error::{Error, Result};
pub use fs::write_private_file;
pub use glob::glob_match;
pub use guard::{GuardBand, GuardThresholds, GuardVerdict, SystemOneGuard, is_eligible};
pub use job::{
    Job, JobRun, JobStatus, JobStore, JobUpdate, NewJob, NewJobRun, SessionMode, parse_stamp, stamp,
};
pub use memory::namespace_for;
pub use message::{Message, Role, ToolCall};
pub use policy::{
    ApprovalChoice, ApprovalRequest, ApprovalStore, ApprovalUi, PolicyEngine, RecordingGate,
    ToolGate, ToolPolicy, subject_for,
};
pub use provider::{
    ChatEvent, ChatRequest, FinishReason, Provider, ToolSchema, Usage, sanitize_schema,
};
pub use read_only::{AutoCommands, AutoMode, is_read_only_command};
pub use session::new_session_id;
pub use tokens::{estimate, estimate_message, estimate_messages, estimate_tools};
pub use tool::{Risk, Tool, ToolCatalog, ToolCtx, ToolOutput, ToolRegistry, ToolSelection};
pub use update::{
    Asset, Decision, Download, InstallReport, ReleaseInfo, UpdateSource, Updater, VersionOrder,
    compare_versions, is_commit, parse_checksums, platform_asset, url_is_permitted,
};
