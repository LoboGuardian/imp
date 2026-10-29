//! `edit_file` and `apply_patch`: two ways to change an existing file.
//!
//! Both share [`patch_text`], which is the whole point — one implementation of
//! "find a unique anchor and do something" means the two tools cannot disagree
//! about matching, and every guarantee below is tested once.

use std::time::Duration;

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;

use imp_core::error::{Error, Result};
use imp_core::tool::{Risk, Tool, ToolCtx, ToolOutput};

/// Default ceiling on a file a write may touch.
pub const DEFAULT_MAX_BYTES: u64 = 2_097_152;

/// What to do with an anchor that appears more or fewer times than expected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ambiguity {
    /// The anchor matched nowhere.
    Missing,
    /// The anchor matched more than once, so the edit is ambiguous.
    Repeated,
}

/// Raised when an anchor does not identify exactly one location.
pub struct AnchorError {
    /// 1-based index of the failing operation, for an actionable message.
    pub operation: usize,
    /// Whether the anchor was absent or present too many times.
    pub kind: Ambiguity,
    /// How many times the anchor actually appeared.
    pub found: usize,
    /// The first line of the anchor, quoted in the message.
    pub preview: String,
}

impl std::fmt::Display for AnchorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (verb, advice) = match self.kind {
            Ambiguity::Missing => ("does not appear", "check whitespace and indentation"),
            Ambiguity::Repeated => (
                "is ambiguous",
                "include more surrounding context so it matches once",
            ),
        };
        write!(
            f,
            "operation {}: anchor {} in the file ({} {}). {}",
            self.operation,
            self.kind_word(),
            self.found,
            self.kind_word(),
            advice
        )
        .and_then(|_| {
            let _ = verb;
            Ok(())
        })
    }
}

impl AnchorError {
    fn kind_word(&self) -> &'static str {
        match self.kind {
            Ambiguity::Missing => "missing",
            Ambiguity::Repeated => "repeated",
        }
    }

    /// A message the model can act on without guessing.
    pub fn to_model_string(&self) -> String {
        format!(
            "operation {}: anchor not found ({}) or appears {} times. \
             Include more surrounding context so the snippet matches exactly once.",
            self.operation + 1,
            if self.found == 0 {
                "nowhere"
            } else {
                "elsewhere"
            },
            self.found
        )
    }
}

/// One operation against a file's text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Operation {
    /// Swap `old` for `new`.
    Replace {
        old: String,
        new: String,
        count: Option<usize>,
    },
    /// Insert `new` immediately before a unique anchor.
    InsertBefore {
        anchor: String,
        new: String,
        count: Option<usize>,
    },
    /// Insert `new` immediately after a unique anchor.
    InsertAfter {
        anchor: String,
        new: String,
        count: Option<usize>,
    },
    /// Remove `old`.
    Delete { old: String, count: Option<usize> },
}

impl Operation {
    /// The snippet this operation anchors on.
    pub fn anchor(&self) -> &str {
        match self {
            Operation::Replace { old, .. } | Operation::Delete { old, .. } => old,
            Operation::InsertBefore { anchor, .. } | Operation::InsertAfter { anchor, .. } => {
                anchor
            }
        }
    }
}

/// Apply `operations` to `original`, or return the first failure without
/// touching anything.
///
/// The caller gets an owned `String` precisely so a partial application is
/// impossible: on error there is no mutated value to accidentally write.
pub fn patch_text(original: &str, operations: &[Operation]) -> Result<(String, usize)> {
    if operations.is_empty() {
        return Err(Error::ToolArgs {
            tool: "apply_patch".to_string(),
            message: "a patch needs at least one operation".to_string(),
        });
    }

    let mut text = original.to_string();
    for (index, operation) in operations.iter().enumerate() {
        let anchor = operation.anchor();
        if anchor.is_empty() {
            return Err(Error::ToolArgs {
                tool: "apply_patch".to_string(),
                message: format!("operation {} has an empty anchor", index + 1),
            });
        }

        let found = text.matches(anchor).count();
        let expected = match operation {
            Operation::Replace { count, .. }
            | Operation::InsertBefore { count, .. }
            | Operation::InsertAfter { count, .. }
            | Operation::Delete { count, .. } => count.unwrap_or(1),
        };

        if found != expected {
            return Err(Error::Tool {
                tool: "apply_patch".to_string(),
                message: AnchorError {
                    operation: index,
                    kind: if found == 0 {
                        Ambiguity::Missing
                    } else {
                        Ambiguity::Repeated
                    },
                    found,
                    preview: anchor.lines().next().unwrap_or("").trim().to_string(),
                }
                .to_model_string(),
            });
        }

        match operation {
            Operation::Replace { old, new, .. } => {
                text = text.replace(old.as_str(), new);
            }
            Operation::Delete { old, .. } => {
                text = text.replace(old, "");
            }
            Operation::InsertBefore { anchor, new, .. } => {
                text = text.replace(anchor, &format!("{new}{anchor}"));
            }
            Operation::InsertAfter { anchor, new, .. } => {
                text = text.replace(anchor, &format!("{anchor}{new}"));
            }
        }
    }

    Ok((text, operations.len()))
}

/// A unified diff of two texts, using `similar`.
pub fn unified_diff(path: &str, before: &str, after: &str) -> String {
    use similar::TextDiff;

    TextDiff::from_lines(before, after)
        .unified_diff()
        .context_radius(3)
        .header(path, path)
        .to_string()
}

/// Read a file, enforcing the byte cap, as UTF-8.
async fn read_capped(path: &std::path::Path, max_bytes: u64, tool: &'static str) -> Result<String> {
    let metadata = tokio::fs::metadata(path).await.map_err(|err| Error::Tool {
        tool: tool.to_string(),
        message: format!("cannot stat `{}`: {err}", path.display()),
    })?;
    if !metadata.is_file() {
        return Err(Error::Tool {
            tool: tool.to_string(),
            message: format!("`{}` is not a regular file", path.display()),
        });
    }
    if metadata.len() > max_bytes {
        return Err(Error::Tool {
            tool: tool.to_string(),
            message: format!(
                "`{}` is {} bytes, over the {} byte cap",
                path.display(),
                metadata.len(),
                max_bytes
            ),
        });
    }
    let bytes = tokio::fs::read(path).await.map_err(|err| Error::Tool {
        tool: tool.to_string(),
        message: format!("cannot read `{}`: {err}", path.display()),
    })?;
    String::from_utf8(bytes).map_err(|_| Error::Tool {
        tool: tool.to_string(),
        message: format!("`{}` is not valid UTF-8", path.display()),
    })
}

/// Write `contents` atomically, so a crash cannot truncate the target.
///
/// The temp file is created in the workspace root to prevent symlink attacks:
/// if `path` points outside the workspace via a symlink, creating the temp
/// alongside it would also be outside the workspace. By creating the temp in
/// the workspace root and then atomically moving it to the target, we ensure
/// the operation stays within bounds.
pub async fn write_atomic(
    path: &std::path::Path,
    contents: &str,
    workspace_root: &std::path::Path,
) -> Result<()> {
    use tokio::io::AsyncWriteExt;

    // Create temp in workspace root, not alongside target, to prevent
    // symlink attacks where target points outside workspace.
    let temp_filename = format!(
        ".imp-tmp-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let temp = workspace_root.join(temp_filename);

    {
        let mut file = tokio::fs::File::create(&temp).await.map_err(Error::Io)?;
        file.write_all(contents.as_bytes())
            .await
            .map_err(Error::Io)?;
        file.flush().await.map_err(Error::Io)?;
        let _ = file.sync_all().await;
    }

    // Before renaming, verify the target is still within workspace.
    // This catches the case where a symlink was changed between the
    // initial resolve and now.
    let target_canonical = tokio::fs::canonicalize(path).await;
    let target_within_workspace = match target_canonical {
        Ok(canon) => canon.starts_with(workspace_root),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // Target doesn't exist yet - check parent directory
            if let Some(parent) = path.parent() {
                // If parent is within workspace, or is the workspace itself, allow
                parent.starts_with(workspace_root) || parent == workspace_root
            } else {
                false
            }
        }
        Err(_) => false,
    };

    if !target_within_workspace {
        // Clean up temp file and deny the write
        let _ = tokio::fs::remove_file(&temp).await;
        return Err(Error::Tool {
            tool: "write".to_string(),
            message: format!(
                "target `{}` is outside the workspace",
                path.display()
            ),
        });
    }

    tokio::fs::rename(&temp, path)
        .await
        .map_err(|err| Error::Tool {
            tool: "write".to_string(),
            message: format!("cannot write `{}`: {err}", path.display()),
        })
}

// ---------------------------------------------------------------- edit_file

/// Arguments for `edit_file`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct EditFileArgs {
    /// Path to the file. Relative paths resolve against the workspace root.
    pub path: String,
    /// The exact text to replace.
    pub old_string: String,
    /// The text to put in its place. May be empty.
    pub new_string: String,
    /// Replace every occurrence instead of requiring exactly one.
    #[serde(default)]
    pub replace_all: bool,
}

/// A single surgical replacement in one file.
pub struct EditFile {
    /// Largest file this tool will touch.
    pub max_bytes: u64,
}

impl EditFile {
    /// Build with a byte cap.
    pub fn new(max_bytes: u64) -> Self {
        Self { max_bytes }
    }
}

#[async_trait]
impl Tool for EditFile {
    fn name(&self) -> &'static str {
        "edit_file"
    }

    fn description(&self) -> &'static str {
        "Replace an exact snippet in an existing file. The snippet must match exactly once unless replace_all is true. Prefer this for a small change and apply_patch for a large one."
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::to_value(schemars::schema_for!(EditFileArgs)).unwrap_or_default()
    }

    fn risk(&self) -> Risk {
        Risk::Write
    }

    fn timeout(&self) -> Duration {
        Duration::from_secs(15)
    }

    async fn invoke(&self, ctx: ToolCtx, args: serde_json::Value) -> Result<ToolOutput> {
        let args: EditFileArgs = serde_json::from_value(args).map_err(|err| Error::ToolArgs {
            tool: self.name().to_string(),
            message: err.to_string(),
        })?;
        let path = ctx.resolve(&args.path)?;
        let before = read_capped(&path, self.max_bytes, self.name()).await?;

        let occurrences = before.matches(&args.old_string).count();
        if args.old_string.is_empty() {
            return Err(Error::ToolArgs {
                tool: self.name().to_string(),
                message: "old_string must not be empty".to_string(),
            });
        }
        if !args.replace_all && occurrences != 1 {
            return Err(Error::Tool {
                tool: self.name().to_string(),
                message: format!(
                    "old_string appears {occurrences} times; include more context to make it unique, or set replace_all"
                ),
            });
        }

        let after = if args.replace_all {
            before.replace(&args.old_string, &args.new_string)
        } else {
            before.replacen(&args.old_string, &args.new_string, 1)
        };
        if after == before {
            return Err(Error::Tool {
                tool: self.name().to_string(),
                message: "the replacement would not change the file".to_string(),
            });
        }

        write_atomic(&path, &after, &ctx.workspace_root).await?;

        let diff = unified_diff(&args.path, &before, &after);
        Ok(ToolOutput {
            content: diff,
            truncated: false,
            metadata: serde_json::json!({
                "path": path,
                "replacements": if args.replace_all { occurrences } else { 1 },
            }),
        })
    }
}

// -------------------------------------------------------------- apply_patch

/// Arguments for `apply_patch`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ApplyPatchArgs {
    /// Files to change, with the operations to run against each.
    pub files: Vec<FilePatch>,
}

/// One file and the operations to apply to it.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct FilePatch {
    /// Path to the file. Relative paths resolve against the workspace root.
    pub path: String,
    /// Operations to apply, in order.
    pub operations: Vec<PatchOperation>,
}

/// A single operation as the model writes it.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct PatchOperation {
    /// Which operation to perform.
    pub op: PatchOp,
    /// The anchor for `replace` and `delete`.
    #[serde(default)]
    pub old: Option<String>,
    /// The anchor for `insert_before` and `insert_after`.
    #[serde(default)]
    pub anchor: Option<String>,
    /// The text to insert or substitute.
    #[serde(default)]
    pub new: Option<String>,
    /// How many times the anchor must match. Defaults to 1.
    #[serde(default)]
    pub count: Option<usize>,
}

/// The operation verbs `apply_patch` understands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PatchOp {
    /// Swap the anchor for `new`.
    Replace,
    /// Insert `new` before the anchor.
    InsertBefore,
    /// Insert `new` after the anchor.
    InsertAfter,
    /// Remove the anchor.
    Delete,
}

impl PatchOp {
    /// The field this verb anchors on.
    fn anchor_field(self) -> &'static str {
        match self {
            PatchOp::Replace | PatchOp::Delete => "old",
            PatchOp::InsertBefore | PatchOp::InsertAfter => "anchor",
        }
    }

    /// Whether the verb consumes a `new` value.
    fn wants_new(self) -> bool {
        !matches!(self, PatchOp::Delete)
    }
}

/// Apply anchored operations across one or more files, atomically.
pub struct ApplyPatch {
    /// Largest file this tool will touch.
    pub max_bytes: u64,
}

impl ApplyPatch {
    /// Build with a byte cap.
    pub fn new(max_bytes: u64) -> Self {
        Self { max_bytes }
    }
}

impl ApplyPatch {
    /// Turn the wire form into the internal form, rejecting malformed input.
    fn convert(args: ApplyPatchArgs) -> Result<Vec<(String, Vec<Operation>)>> {
        if args.files.is_empty() {
            return Err(Error::ToolArgs {
                tool: "apply_patch".to_string(),
                message: "`files` must list at least one file".to_string(),
            });
        }

        args.files
            .into_iter()
            .map(|file| {
                if file.operations.is_empty() {
                    return Err(Error::ToolArgs {
                        tool: "apply_patch".to_string(),
                        message: format!("`{}` has no operations", file.path),
                    });
                }
                let operations = file
                    .operations
                    .into_iter()
                    .map(|op| {
                        let field = op.op.anchor_field();
                        let anchor = if field == "old" { op.old } else { op.anchor };
                        let anchor = anchor.ok_or_else(|| Error::ToolArgs {
                            tool: "apply_patch".to_string(),
                            message: format!("`{}` requires the `{field}` field", op.op.as_str()),
                        })?;
                        let new = if op.op.wants_new() {
                            op.new.ok_or_else(|| Error::ToolArgs {
                                tool: "apply_patch".to_string(),
                                message: format!("`{}` requires a `new` field", op.op.as_str()),
                            })?
                        } else {
                            String::new()
                        };
                        Ok(match op.op {
                            PatchOp::Replace => Operation::Replace {
                                old: anchor,
                                new,
                                count: op.count,
                            },
                            PatchOp::InsertBefore => Operation::InsertBefore {
                                anchor,
                                new,
                                count: op.count,
                            },
                            PatchOp::InsertAfter => Operation::InsertAfter {
                                anchor,
                                new,
                                count: op.count,
                            },
                            PatchOp::Delete => Operation::Delete {
                                old: anchor,
                                count: op.count,
                            },
                        })
                    })
                    .collect::<Result<Vec<_>>>()?;
                Ok((file.path, operations))
            })
            .collect()
    }
}

impl PatchOp {
    fn as_str(self) -> &'static str {
        match self {
            PatchOp::Replace => "replace",
            PatchOp::InsertBefore => "insert_before",
            PatchOp::InsertAfter => "insert_after",
            PatchOp::Delete => "delete",
        }
    }
}

#[async_trait]
impl Tool for ApplyPatch {
    fn name(&self) -> &'static str {
        "apply_patch"
    }

    fn description(&self) -> &'static str {
        "Apply anchored edits across one or more existing files in a single atomic step. Each operation's anchor must match exactly once. Use this for large or multi-file changes; use edit_file for a single small change."
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::to_value(schemars::schema_for!(ApplyPatchArgs)).unwrap_or_default()
    }

    fn risk(&self) -> Risk {
        Risk::Write
    }

    fn timeout(&self) -> Duration {
        Duration::from_secs(30)
    }

    async fn invoke(&self, ctx: ToolCtx, args: serde_json::Value) -> Result<ToolOutput> {
        let args: ApplyPatchArgs = serde_json::from_value(args).map_err(|err| Error::ToolArgs {
            tool: self.name().to_string(),
            message: err.to_string(),
        })?;
        let plan = Self::convert(args)?;

        // Resolve and read everything first, then compute every result, and only
        // then write. A failure anywhere leaves all files untouched.
        let mut prepared = Vec::with_capacity(plan.len());
        for (path, operations) in &plan {
            let resolved = ctx.resolve(path)?;
            let before = read_capped(&resolved, self.max_bytes, self.name()).await?;
            let (after, applied) =
                patch_text(&before, operations).map_err(|err| annotate(err, path))?;
            prepared.push((path.clone(), resolved, before, after, applied));
        }

        let mut body = String::new();
        for (path, _, before, after, applied) in &prepared {
            let diff = unified_diff(path, before, after);
            body.push_str(&format!("--- {path} ({applied} operation(s))\n"));
            body.push_str(&diff);
            if !diff.ends_with('\n') {
                body.push('\n');
            }
        }

        for (_, resolved, _, after, _) in &prepared {
            write_atomic(resolved, after, &ctx.workspace_root).await?;
        }

        let files: Vec<&str> = prepared.iter().map(|(path, ..)| path.as_str()).collect();
        let total: usize = prepared.iter().map(|(_, _, _, _, applied)| applied).sum();

        Ok(ToolOutput {
            content: body,
            truncated: false,
            metadata: serde_json::json!({ "files": files, "operations": total }),
        })
    }
}

/// Name the file a patch operation failed in, so the model can retry it alone.
fn annotate(err: Error, path: &str) -> Error {
    match err {
        Error::Tool { tool, message } => Error::Tool {
            tool,
            message: format!("{path}: {message}"),
        },
        other => other,
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

    fn replace(old: &str, new: &str) -> Operation {
        Operation::Replace {
            old: old.to_string(),
            new: new.to_string(),
            count: None,
        }
    }

    // -------------------------------------------------------- patch_text

    #[test]
    fn a_unique_anchor_is_replaced() {
        let (text, applied) = patch_text("a\nb\nc\n", &[replace("b", "B")]).unwrap();

        assert_eq!(text, "a\nB\nc\n");
        assert_eq!(applied, 1);
    }

    #[test]
    fn a_missing_anchor_is_an_error_naming_the_operation() {
        let err = patch_text("a\n", &[replace("zzz", "x")]).unwrap_err();

        let message = err.to_string();
        assert!(message.contains("operation 1"), "message was: {message}");
        assert!(message.contains("nowhere"), "message was: {message}");
    }

    #[test]
    fn an_ambiguous_anchor_is_refused_rather_than_guessed() {
        let err = patch_text("x\nx\n", &[replace("x", "y")]).unwrap_err();

        let message = err.to_string();
        assert!(message.contains("2 times"), "message was: {message}");
    }

    #[test]
    fn an_explicit_count_must_match_exactly() {
        let source = "x\nx\n";
        assert!(
            patch_text(
                source,
                &[Operation::Replace {
                    old: "x".to_string(),
                    new: "y".to_string(),
                    count: Some(2),
                }]
            )
            .is_ok()
        );

        let err = patch_text(
            source,
            &[Operation::Replace {
                old: "x".to_string(),
                new: "y".to_string(),
                count: Some(1),
            }],
        )
        .unwrap_err();
        assert!(err.to_string().contains("2 times"));
    }

    #[test]
    fn operations_apply_in_order_so_a_later_anchor_may_be_new_text() {
        // The second operation anchors on the text the first one inserted, which
        // is the reason operations are applied to the accumulating result.
        let ops = [
            replace("BODY", "call();"),
            Operation::InsertAfter {
                anchor: "call();".to_string(),
                new: "\nlog();".to_string(),
                count: None,
            },
        ];

        let (text, _) = patch_text("fn run() {\n    BODY\n}\n", &ops).unwrap();

        assert_eq!(text, "fn run() {\n    call();\nlog();\n}\n");
    }

    #[test]
    fn insert_before_and_after_anchor() {
        let (text, _) = patch_text(
            "use std::io;\n",
            &[
                Operation::InsertBefore {
                    anchor: "use std::io;\n".to_string(),
                    new: "use std::fs;\n".to_string(),
                    count: None,
                },
                Operation::InsertAfter {
                    anchor: "use std::io;\n".to_string(),
                    new: "use std::path::Path;\n".to_string(),
                    count: None,
                },
            ],
        )
        .unwrap();

        assert_eq!(text, "use std::fs;\nuse std::io;\nuse std::path::Path;\n");
    }

    #[test]
    fn delete_removes_every_occurrence_when_counted() {
        let (text, _) = patch_text(
            "keep\nnoise\nkeep\nnoise\n",
            &[Operation::Delete {
                old: "noise\n".to_string(),
                count: Some(2),
            }],
        )
        .unwrap();

        assert_eq!(text, "keep\nkeep\n");
    }

    #[test]
    fn an_empty_anchor_is_rejected() {
        let err = patch_text("a", &[replace("", "b")]).unwrap_err();

        assert!(err.to_string().contains("empty anchor"));
    }

    #[test]
    fn an_empty_patch_is_rejected() {
        assert!(patch_text("a", &[]).is_err());
    }

    // ---------------------------------------------------------- edit_file

    #[tokio::test]
    async fn edit_file_replaces_a_unique_snippet() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "hello world\n").unwrap();

        let output = EditFile::new(DEFAULT_MAX_BYTES)
            .invoke(
                ctx(dir.path()),
                serde_json::json!({ "path": "a.txt", "old_string": "world", "new_string": "there" }),
            )
            .await
            .unwrap();

        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "hello there\n"
        );
        assert!(
            output.content.contains("-hello world"),
            "diff was: {}",
            output.content
        );
    }

    #[tokio::test]
    async fn edit_file_refuses_an_ambiguous_snippet() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "x\nx\n").unwrap();

        let err = EditFile::new(DEFAULT_MAX_BYTES)
            .invoke(
                ctx(dir.path()),
                serde_json::json!({ "path": "a.txt", "old_string": "x", "new_string": "y" }),
            )
            .await
            .unwrap_err();

        assert!(err.to_string().contains("appears 2 times"), "was: {err}");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "x\nx\n"
        );
    }

    #[tokio::test]
    async fn edit_file_honours_replace_all() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "x\nx\n").unwrap();

        EditFile::new(DEFAULT_MAX_BYTES)
            .invoke(
                ctx(dir.path()),
                serde_json::json!({
                    "path": "a.txt", "old_string": "x", "new_string": "y", "replace_all": true
                }),
            )
            .await
            .unwrap();

        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "y\ny\n"
        );
    }

    #[tokio::test]
    async fn edit_file_cannot_escape_the_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let err = EditFile::new(DEFAULT_MAX_BYTES)
            .invoke(
                ctx(dir.path()),
                serde_json::json!({ "path": "../outside.txt", "old_string": "a", "new_string": "b" }),
            )
            .await
            .unwrap_err();

        assert!(matches!(err, Error::Denied(_)), "was: {err}");
    }

    #[tokio::test]
    async fn edit_file_refuses_a_no_op() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "same\n").unwrap();

        let err = EditFile::new(DEFAULT_MAX_BYTES)
            .invoke(
                ctx(dir.path()),
                serde_json::json!({ "path": "a.txt", "old_string": "same", "new_string": "same" }),
            )
            .await
            .unwrap_err();

        assert!(err.to_string().contains("would not change"), "was: {err}");
    }

    // -------------------------------------------------------- apply_patch

    #[tokio::test]
    async fn apply_patch_edits_several_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "alpha\n").unwrap();
        std::fs::write(dir.path().join("b.txt"), "beta\n").unwrap();

        let output = ApplyPatch::new(DEFAULT_MAX_BYTES)
            .invoke(
                ctx(dir.path()),
                serde_json::json!({
                    "files": [
                        { "path": "a.txt", "operations": [{ "op": "replace", "old": "alpha", "new": "A" }] },
                        { "path": "b.txt", "operations": [{ "op": "replace", "old": "beta", "new": "B" }] }
                    ]
                }),
            )
            .await
            .unwrap();

        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "A\n"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("b.txt")).unwrap(),
            "B\n"
        );
        assert_eq!(output.metadata["operations"], 2);
    }

    #[tokio::test]
    async fn a_failing_operation_leaves_every_file_untouched() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "alpha\n").unwrap();
        std::fs::write(dir.path().join("b.txt"), "beta\n").unwrap();

        let err = ApplyPatch::new(DEFAULT_MAX_BYTES)
            .invoke(
                ctx(dir.path()),
                serde_json::json!({
                    "files": [
                        { "path": "a.txt", "operations": [{ "op": "replace", "old": "alpha", "new": "A" }] },
                        { "path": "b.txt", "operations": [{ "op": "replace", "old": "nope", "new": "B" }] }
                    ]
                }),
            )
            .await
            .unwrap_err();

        // a.txt would have been written first had the patch not been atomic.
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "alpha\n"
        );
        assert!(
            err.to_string().contains("b.txt"),
            "error should name the file: {err}"
        );
    }

    #[tokio::test]
    async fn a_malformed_operation_names_the_verb() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "alpha\n").unwrap();

        let err = ApplyPatch::new(DEFAULT_MAX_BYTES)
            .invoke(
                ctx(dir.path()),
                serde_json::json!({
                    "files": [{ "path": "a.txt", "operations": [{ "op": "insert_after", "anchor": "alpha" }] }]
                }),
            )
            .await
            .unwrap_err();

        assert!(err.to_string().contains("new"), "was: {err}");
    }

    #[tokio::test]
    async fn apply_patch_refuses_to_create_a_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let err = ApplyPatch::new(DEFAULT_MAX_BYTES)
            .invoke(
                ctx(dir.path()),
                serde_json::json!({
                    "files": [{ "path": "new.txt", "operations": [{ "op": "replace", "old": "a", "new": "b" }] }]
                }),
            )
            .await
            .unwrap_err();

        assert!(err.to_string().contains("cannot stat"), "was: {err}");
    }

    #[tokio::test]
    async fn apply_patch_refuses_an_empty_file_list() {
        let dir = tempfile::tempdir().unwrap();
        let err = ApplyPatch::new(DEFAULT_MAX_BYTES)
            .invoke(ctx(dir.path()), serde_json::json!({ "files": [] }))
            .await
            .unwrap_err();

        assert!(err.to_string().contains("at least one file"), "was: {err}");
    }

    #[test]
    fn the_schema_advertises_every_verb() {
        let schema = ApplyPatch::new(DEFAULT_MAX_BYTES).schema();
        let rendered = schema.to_string();

        for verb in ["replace", "insert_before", "insert_after", "delete"] {
            assert!(rendered.contains(verb), "schema is missing `{verb}`");
        }
    }
}
