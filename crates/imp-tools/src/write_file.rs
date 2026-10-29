//! `write_file`: create a file or replace one wholesale.

use std::time::Duration;

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;

use imp_core::error::{Error, Result};
use imp_core::tool::{Risk, Tool, ToolCtx, ToolOutput};

use crate::patch::write_atomic;

/// Arguments for `write_file`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct WriteFileArgs {
    /// Path to the file. Relative paths resolve against the workspace root.
    pub path: String,
    /// The complete new contents of the file.
    pub content: String,
    /// Create missing parent directories.
    #[serde(default)]
    pub create_dirs: bool,
    /// Replace the file if it already exists. Without this, an existing file is
    /// an error rather than something to clobber.
    #[serde(default)]
    pub overwrite: bool,
}

/// Write a file atomically, refusing to clobber unless asked.
pub struct WriteFile {
    /// Largest file this tool will write.
    pub max_bytes: u64,
}

impl WriteFile {
    /// Build with a byte cap.
    pub fn new(max_bytes: u64) -> Self {
        Self { max_bytes }
    }
}

#[async_trait]
impl Tool for WriteFile {
    fn name(&self) -> &'static str {
        "write_file"
    }

    fn description(&self) -> &'static str {
        "Write a file, replacing it entirely. Refuses to overwrite an existing file unless overwrite is true. Use apply_patch for a change to part of a file."
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::to_value(schemars::schema_for!(WriteFileArgs)).unwrap_or_default()
    }

    fn risk(&self) -> Risk {
        Risk::Write
    }

    fn timeout(&self) -> Duration {
        Duration::from_secs(15)
    }

    /// The path is what an allowlist should match on, not the whole file body.
    fn approval_subject(&self, args: &serde_json::Value) -> Option<String> {
        args.get("path")
            .and_then(|value| value.as_str())
            .map(str::to_string)
    }

    async fn invoke(&self, ctx: ToolCtx, args: serde_json::Value) -> Result<ToolOutput> {
        let args: WriteFileArgs = serde_json::from_value(args).map_err(|err| Error::ToolArgs {
            tool: self.name().to_string(),
            message: err.to_string(),
        })?;

        let content_bytes = args.content.len() as u64;
        if content_bytes > self.max_bytes {
            return Err(Error::Tool {
                tool: self.name().to_string(),
                message: format!(
                    "{content_bytes} bytes exceeds the {} byte cap",
                    self.max_bytes
                ),
            });
        }

        let path = ctx.resolve(&args.path)?;
        let existed = tokio::fs::try_exists(&path)
            .await
            .map_err(|err| Error::Tool {
                tool: self.name().to_string(),
                message: format!("cannot stat `{}`: {err}", args.path),
            })?;
        if existed && !args.overwrite {
            return Err(Error::Tool {
                tool: self.name().to_string(),
                message: format!(
                    "`{}` already exists; pass overwrite, or use apply_patch to change part of it",
                    args.path
                ),
            });
        }

        if args.create_dirs {
            if let Some(parent) = path.parent() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .map_err(|err| Error::Tool {
                        tool: self.name().to_string(),
                        message: format!("cannot create `{}`: {err}", parent.display()),
                    })?;
            }
        } else if let Some(parent) = path.parent()
            && !parent.exists()
        {
            return Err(Error::Tool {
                tool: self.name().to_string(),
                message: format!(
                    "the directory `{}` does not exist; pass create_dirs",
                    parent.display()
                ),
            });
        }

        write_atomic(&path, &args.content, &ctx.workspace_root).await?;

        Ok(
            ToolOutput::text(format!("wrote {} ({content_bytes} bytes)", args.path))
                .with_metadata(serde_json::json!({ "path": path, "bytes": content_bytes })),
        )
    }
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

    #[tokio::test]
    async fn creates_a_new_file() {
        let dir = tempfile::tempdir().unwrap();

        WriteFile::new(1024)
            .invoke(
                ctx(dir.path()),
                serde_json::json!({ "path": "a.txt", "content": "hello" }),
            )
            .await
            .unwrap();

        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "hello"
        );
    }

    #[tokio::test]
    async fn refuses_to_clobber_without_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "original").unwrap();

        let err = WriteFile::new(1024)
            .invoke(
                ctx(dir.path()),
                serde_json::json!({ "path": "a.txt", "content": "new" }),
            )
            .await
            .unwrap_err();

        assert!(err.to_string().contains("already exists"), "was: {err}");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "original"
        );
    }

    #[tokio::test]
    async fn overwrites_when_asked() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "original").unwrap();

        WriteFile::new(1024)
            .invoke(
                ctx(dir.path()),
                serde_json::json!({ "path": "a.txt", "content": "new", "overwrite": true }),
            )
            .await
            .unwrap();

        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "new"
        );
    }

    #[tokio::test]
    async fn requires_create_dirs_for_a_missing_directory() {
        let dir = tempfile::tempdir().unwrap();

        let err = WriteFile::new(1024)
            .invoke(
                ctx(dir.path()),
                serde_json::json!({ "path": "nested/a.txt", "content": "x" }),
            )
            .await
            .unwrap_err();

        assert!(err.to_string().contains("create_dirs"), "was: {err}");
    }

    #[tokio::test]
    async fn creates_directories_on_request() {
        let dir = tempfile::tempdir().unwrap();

        WriteFile::new(1024)
            .invoke(
                ctx(dir.path()),
                serde_json::json!({ "path": "nested/deep/a.txt", "content": "x", "create_dirs": true }),
            )
            .await
            .unwrap();

        assert_eq!(
            std::fs::read_to_string(dir.path().join("nested/deep/a.txt")).unwrap(),
            "x"
        );
    }

    #[tokio::test]
    async fn refuses_content_over_the_cap() {
        let dir = tempfile::tempdir().unwrap();

        let err = WriteFile::new(4)
            .invoke(ctx(dir.path()), serde_path_free("a.txt", "far too long"))
            .await
            .unwrap_err();

        assert!(err.to_string().contains("exceeds"), "was: {err}");
    }

    fn serde_path_free(path: &str, content: &str) -> serde_json::Value {
        serde_json::json!({ "path": path, "content": content })
    }

    #[tokio::test]
    async fn cannot_escape_the_workspace() {
        let dir = tempfile::tempdir().unwrap();

        let err = WriteFile::new(1024)
            .invoke(ctx(dir.path()), serde_path_free("../escape.txt", "x"))
            .await
            .unwrap_err();

        assert!(matches!(err, Error::Denied(_)), "was: {err}");
    }
}
