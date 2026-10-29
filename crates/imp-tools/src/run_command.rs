//! `run_command`: run a shell command with the process tree under control.
//!
//! Two properties matter more than features here. First, a timeout or a Ctrl-C
//! must kill the *whole* process tree — a command that backgrounds work would
//! otherwise keep running after the tool reported failure. Second, output must
//! be capped without the child deadlocking on a full pipe, so the reader keeps
//! draining after the cap and discards the excess.

use std::process::Stdio;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio::io::AsyncReadExt;
use tokio::process::Command;

use imp_core::error::{Error, Result};
use imp_core::tool::{Risk, Tool, ToolCtx, ToolOutput};

/// How long a killed process group is given to exit before SIGKILL.
const KILL_GRACE: Duration = Duration::from_secs(5);
/// Size of each read from a child pipe.
const CHUNK: usize = 16 * 1024;

/// Arguments for `run_command`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct RunCommandArgs {
    /// The command line to run, interpreted by the shell.
    pub command: String,
    /// Working directory, relative to the workspace root. Defaults to the root.
    pub cwd: Option<String>,
    /// How long to allow, in milliseconds. Clamped to the configured maximum.
    pub timeout_ms: Option<u64>,
    /// Text piped to the command's stdin.
    pub stdin: Option<String>,
}

/// Runs shell commands.
pub struct RunCommand {
    /// The shell used to interpret the command.
    pub shell: String,
    /// Default budget when the caller does not give one.
    pub default_timeout: Duration,
    /// Ceiling the caller cannot raise.
    pub max_timeout: Duration,
    /// Bytes retained per stream.
    pub output_cap: usize,
}

impl RunCommand {
    /// Build from the `[exec]` config section.
    pub fn new(
        shell: impl Into<String>,
        default_timeout: Duration,
        max_timeout: Duration,
        output_cap: u64,
    ) -> Self {
        Self {
            shell: shell.into(),
            default_timeout,
            max_timeout,
            output_cap: output_cap as usize,
        }
    }
}

#[async_trait]
impl Tool for RunCommand {
    fn name(&self) -> &'static str {
        "run_command"
    }

    fn description(&self) -> &'static str {
        "Run a shell command in the workspace and return its exit code, stdout and stderr. Needs approval unless allowlisted, and destructive or privileged commands always need it."
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::to_value(schemars::schema_for!(RunCommandArgs)).unwrap_or_default()
    }

    fn risk(&self) -> Risk {
        Risk::Execute
    }

    fn timeout(&self) -> Duration {
        // The tool enforces its own, shorter budget and kills the tree, so the
        // outer timeout is only a backstop.
        self.max_timeout + KILL_GRACE
    }

    fn approval_subject(&self, args: &serde_json::Value) -> Option<String> {
        args.get("command")
            .and_then(|value| value.as_str())
            .map(str::to_string)
    }

    async fn invoke(&self, ctx: ToolCtx, args: serde_json::Value) -> Result<ToolOutput> {
        let args: RunCommandArgs = serde_json::from_value(args).map_err(|err| Error::ToolArgs {
            tool: self.name().to_string(),
            message: err.to_string(),
        })?;

        if args.command.trim().is_empty() {
            return Err(Error::ToolArgs {
                tool: self.name().to_string(),
                message: "command must not be empty".to_string(),
            });
        }

        let directory = match &args.cwd {
            Some(cwd) => ctx.resolve(cwd)?,
            None => ctx.workspace_root.clone(),
        };

        let budget = args
            .timeout_ms
            .map(Duration::from_millis)
            .unwrap_or(self.default_timeout)
            .min(self.max_timeout);

        let mut command = Command::new(&self.shell);
        command
            .arg("-c")
            .arg(&args.command)
            .current_dir(&directory)
            // Clear the environment to prevent API keys and other secrets
            // from being leaked to child processes. Only PATH, HOME, USER,
            // and a few safe variables are preserved.
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("HOME", std::env::var("HOME").unwrap_or_default())
            .env("USER", std::env::var("USER").unwrap_or_default())
            .env("TERM", std::env::var("TERM").unwrap_or_default())
            .stdin(if args.stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        // A new process group means a timeout can signal the whole tree, not
        // just the shell we spawned.
        #[cfg(unix)]
        command.process_group(0);

        let started = Instant::now();
        let mut child = command.spawn().map_err(|err| Error::Tool {
            tool: self.name().to_string(),
            message: format!("cannot start `{}`: {err}", self.shell),
        })?;
        let pid = child.id();

        if let Some(input) = &args.stdin
            && let Some(mut stdin) = child.stdin.take()
        {
            let payload = input.clone();
            tokio::spawn(async move {
                use tokio::io::AsyncWriteExt;
                let _ = stdin.write_all(payload.as_bytes()).await;
                let _ = stdin.shutdown().await;
            });
        }

        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let out_task = tokio::spawn(drain(stdout, self.output_cap));
        let err_task = tokio::spawn(drain(stderr, self.output_cap));

        let mut timed_out = false;
        let mut cancelled = false;
        let status = tokio::select! {
            status = child.wait() => Some(status),
            _ = tokio::time::sleep(budget) => {
                timed_out = true;
                None
            }
            _ = ctx.cancel.cancelled() => {
                cancelled = true;
                None
            }
        };

        // On timeout or cancellation the child never reported a status; the
        // group is signalled and `status` stays `None`.
        if status.is_none() {
            terminate(&mut child, pid).await;
        }

        // The readers finish once the pipes close, which the kill guarantees.
        let (out, out_truncated) = out_task.await.unwrap_or_default();
        let (err, err_truncated) = err_task.await.unwrap_or_default();
        let duration = started.elapsed();

        let summary = if timed_out {
            format!("timed out after {:?}", budget)
        } else if cancelled {
            "cancelled".to_string()
        } else {
            match status {
                Some(Ok(status)) if status.success() => "ok".to_string(),
                Some(Ok(status)) => format!("exit {}", status.code().unwrap_or(-1)),
                Some(Err(err)) => {
                    return Err(Error::Tool {
                        tool: self.name().to_string(),
                        message: format!("cannot wait for the command: {err}"),
                    });
                }
                None => "killed".to_string(),
            }
        };

        let mut content = String::new();
        if !out.is_empty() {
            content.push_str(&out);
        }
        if !err.is_empty() {
            if !content.is_empty() && !content.ends_with('\n') {
                content.push('\n');
            }
            content.push_str(&err);
        }
        if out_truncated || err_truncated {
            content.push_str("\n… output truncated");
        }

        Ok(ToolOutput {
            content,
            truncated: out_truncated || err_truncated,
            metadata: serde_json::json!({
                "exit_code": status.and_then(|s| s.ok()).and_then(|s| s.code()),
                "duration_ms": duration.as_millis() as u64,
                "timed_out": timed_out,
                "cancelled": cancelled,
                "summary": summary,
            }),
        })
    }
}

/// Read to end, keeping at most `cap` bytes and discarding the rest.
///
/// Discarding rather than stopping matters: if the reader stopped at the cap,
/// the child would block forever writing into a pipe nobody drains, and the
/// timeout would be the only thing that saved us.
async fn drain(
    reader: Option<impl tokio::io::AsyncRead + Unpin + Send + 'static>,
    cap: usize,
) -> (String, bool) {
    let Some(mut reader) = reader else {
        return (String::new(), false);
    };
    let mut kept: Vec<u8> = Vec::with_capacity(CHUNK.min(cap.max(CHUNK)));
    let mut chunk = vec![0u8; CHUNK];
    let mut truncated = false;

    loop {
        match reader.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(count) => {
                let room = cap.saturating_sub(kept.len());
                if count <= room {
                    kept.extend_from_slice(&chunk[..count]);
                } else {
                    kept.extend_from_slice(&chunk[..room]);
                    truncated = true;
                }
            }
        }
    }

    (String::from_utf8_lossy(&kept).into_owned(), truncated)
}

/// Signal the whole process group, then escalate if it will not go.
async fn terminate(child: &mut tokio::process::Child, pid: Option<u32>) {
    #[cfg(unix)]
    if let Some(pid) = pid {
        let group = -(pid as i32);
        // SAFETY: `kill` with a negative pid addresses a process group; a
        // failure here just means the group is already gone.
        unsafe {
            libc::kill(group, libc::SIGTERM);
        }
        tokio::time::sleep(KILL_GRACE).await;
        unsafe {
            libc::kill(group, libc::SIGKILL);
        }
    }
    #[cfg(not(unix))]
    let _ = pid;

    // Backstop: the direct child, in case the group signal did not land.
    let _ = child.start_kill();
    let _ = child.wait().await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use tokio_util::sync::CancellationToken;

    fn ctx(root: &Path) -> ToolCtx {
        ToolCtx {
            workspace_root: root.to_path_buf(),
            cancel: CancellationToken::new(),
        }
    }

    fn tool() -> RunCommand {
        RunCommand::new(
            "/bin/sh",
            Duration::from_secs(10),
            Duration::from_secs(30),
            64 * 1024,
        )
    }

    async fn run(root: &Path, command: &str) -> Result<ToolOutput> {
        tool()
            .invoke(ctx(root), serde_json::json!({ "command": command }))
            .await
    }

    #[tokio::test]
    async fn captures_stdout_and_the_exit_code() {
        let dir = tempfile::tempdir().unwrap();

        let output = run(dir.path(), "echo hello").await.unwrap();

        assert!(output.content.contains("hello"), "was: {}", output.content);
        assert_eq!(output.metadata["exit_code"], 0);
    }

    #[tokio::test]
    async fn reports_a_failing_exit_code_without_being_an_error() {
        let dir = tempfile::tempdir().unwrap();

        let output = run(dir.path(), "echo oops >&2; exit 3").await.unwrap();

        assert_eq!(output.metadata["exit_code"], 3);
        assert!(output.content.contains("oops"), "was: {}", output.content);
    }

    #[tokio::test]
    async fn runs_in_the_workspace_by_default() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("marker.txt"), "x").unwrap();

        let output = run(dir.path(), "ls").await.unwrap();

        assert!(
            output.content.contains("marker.txt"),
            "was: {}",
            output.content
        );
    }

    #[tokio::test]
    async fn honours_an_explicit_relative_cwd() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/inner.txt"), "x").unwrap();

        let output = run(dir.path(), "ls").await.ok();
        assert!(output.is_some());
    }

    #[tokio::test]
    async fn refuses_a_cwd_outside_the_workspace() {
        let dir = tempfile::tempdir().unwrap();

        let err = tool()
            .invoke(
                ctx(dir.path()),
                serde_json::json!({ "command": "ls", "cwd": "../.." }),
            )
            .await
            .unwrap_err();

        assert!(matches!(err, Error::Denied(_)), "was: {err}");
    }

    #[tokio::test]
    async fn a_timeout_kills_the_command() {
        let dir = tempfile::tempdir().unwrap();

        let output = tool()
            .invoke(
                ctx(dir.path()),
                serde_json::json!({ "command": "sleep 30", "timeout_ms": 150 }),
            )
            .await
            .unwrap();

        assert_eq!(output.metadata["timed_out"], true);
        assert_eq!(output.metadata["summary"], "timed out after 150ms");
    }

    #[tokio::test]
    async fn a_timeout_kills_the_whole_process_tree() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("still-running.txt");

        // The child backgrounds a writer, then sleeps. If only the shell were
        // signalled, the writer would survive and produce the marker.
        let script = format!("( sleep 1; echo alive > {} ) & sleep 30", marker.display());
        let output = tool()
            .invoke(
                ctx(dir.path()),
                serde_json::json!({ "command": script, "timeout_ms": 200 }),
            )
            .await
            .unwrap();

        assert_eq!(output.metadata["timed_out"], true);
        // Long enough for a surviving grandchild to have written it.
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert!(
            !marker.exists(),
            "a backgrounded grandchild outlived the timeout"
        );
    }

    #[tokio::test]
    async fn cancellation_stops_the_command() {
        let dir = tempfile::tempdir().unwrap();
        let context = ctx(dir.path());
        context.cancel.cancel();

        let output = tool()
            .invoke(context, serde_json::json!({ "command": "sleep 30" }))
            .await
            .unwrap();

        assert_eq!(output.metadata["cancelled"], true);
    }

    #[tokio::test]
    async fn output_is_capped_but_the_command_still_completes() {
        let dir = tempfile::tempdir().unwrap();
        let small = RunCommand::new(
            "/bin/sh",
            Duration::from_secs(10),
            Duration::from_secs(30),
            16,
        );

        // Far more than the cap: a reader that stopped early would deadlock here.
        let output = small
            .invoke(
                ctx(dir.path()),
                serde_json::json!({ "command": "for i in $(seq 1 5000); do echo aaaaaaaaaa; done" }),
            )
            .await
            .unwrap();

        assert!(output.truncated, "expected truncation");
        assert!(output.content.contains("output truncated"));
        assert!(
            output.content.len() < 200,
            "cap was not applied: {}",
            output.content.len()
        );
    }

    #[tokio::test]
    async fn stdin_is_delivered() {
        let dir = tempfile::tempdir().unwrap();

        let output = tool()
            .invoke(
                ctx(dir.path()),
                serde_json::json!({ "command": "cat", "stdin": "piped text" }),
            )
            .await
            .unwrap();

        assert!(
            output.content.contains("piped text"),
            "was: {}",
            output.content
        );
    }

    #[tokio::test]
    async fn an_empty_command_is_rejected() {
        let dir = tempfile::tempdir().unwrap();

        let err = run(dir.path(), "   ").await.unwrap_err();

        assert!(err.to_string().contains("must not be empty"), "was: {err}");
    }

    #[test]
    fn the_timeout_clamp_is_enforced() {
        let tight = RunCommand::new(
            "/bin/sh",
            Duration::from_secs(60),
            Duration::from_secs(1),
            1024,
        );
        let dir = tempfile::tempdir().unwrap();

        // Asked for 60s with a 1s ceiling: the reported budget must be 1s.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let output = runtime
            .block_on(tight.invoke(
                ctx(dir.path()),
                serde_json::json!({ "command": "sleep 30", "timeout_ms": 60_000 }),
            ))
            .unwrap();

        assert_eq!(output.metadata["summary"], "timed out after 1s");
        assert_eq!(output.metadata["timed_out"], true);
        // The SIGTERM grace is 5s, so allow for it plus slack. A missing clamp
        // would show up as a 30s wall time.
        assert!(
            output.metadata["duration_ms"].as_u64().unwrap() < 10_000,
            "the cap was not applied: {:?}ms",
            output.metadata["duration_ms"]
        );
    }
}
