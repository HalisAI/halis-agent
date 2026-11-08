use super::{Tool, ToolCtx, ToolKind, display_path, resolve_path};
use crate::error::{CoreError, Result};
use crate::message::ToolOutput;
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::Path;

const MAX_LINES: usize = 2000;
const MAX_LINE_CHARS: usize = 2000;
const MAX_TOTAL_CHARS: usize = 60_000;
const MAX_LS_ENTRIES: usize = 500;

pub struct ReadFileTool;

#[derive(Deserialize)]
struct ReadArgs {
    path: String,
    #[serde(default)]
    offset: Option<usize>,
    #[serde(default)]
    limit: Option<usize>,
}

#[async_trait::async_trait]
impl Tool for ReadFileTool {
    fn name(&self) -> &'static str {
        "read_file"
    }

    fn description(&self) -> &'static str {
        "Read a text file with 1-based line numbers. Supports offset/limit pagination; \
         defaults to the first 2000 lines."
    }

    fn kind(&self) -> ToolKind {
        ToolKind::ReadOnly
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "File path (absolute or relative to cwd)" },
                "offset": { "type": "integer", "description": "Line number to start from (1-based)" },
                "limit": { "type": "integer", "description": "Maximum number of lines to return" },
            },
            "required": ["path"],
        })
    }

    async fn execute(&self, args: Value, ctx: &ToolCtx) -> Result<ToolOutput> {
        let args: ReadArgs = serde_json::from_value(args)
            .map_err(|e| CoreError::Tool("read_file".into(), e.to_string()))?;
        let path = resolve_path(&ctx.cwd, &args.path);
        let meta = std::fs::metadata(&path)
            .map_err(|e| CoreError::Tool("read_file".into(), format!("{}: {e}", args.path)))?;
        if meta.is_dir() {
            let entries: Vec<String> = std::fs::read_dir(&path)
                .map_err(|e| CoreError::Tool("read_file".into(), e.to_string()))?
                .filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect();
            return Ok(ToolOutput::ok(format!(
                "{} is a directory with {} entries:\n{}",
                args.path,
                entries.len(),
                entries.join("\n")
            )));
        }

        let bytes = std::fs::read(&path)
            .map_err(|e| CoreError::Tool("read_file".into(), format!("{}: {e}", args.path)))?;
        if bytes.contains(&0) {
            return Ok(ToolOutput::ok(format!(
                "{} is a binary file ({} bytes)",
                args.path,
                bytes.len()
            )));
        }
        let content = String::from_utf8_lossy(&bytes);
        let lines: Vec<&str> = content.lines().collect();
        let offset = args.offset.unwrap_or(1).max(1);
        let skip = (offset - 1).min(lines.len());
        let limit = args.limit.unwrap_or(MAX_LINES).min(MAX_LINES);
        let window: Vec<(&str, usize)> = lines[skip..]
            .iter()
            .take(limit)
            .copied()
            .zip(skip + 1..)
            .collect();

        let mut out = String::new();
        let mut total = 0;
        for (line, no) in &window {
            let line: String = line.chars().take(MAX_LINE_CHARS).collect();
            out.push_str(&format!("{no:>6}\t{line}\n"));
            total += line.chars().count();
            if total > MAX_TOTAL_CHARS {
                break;
            }
        }
        let shown = window.len();
        let remaining = lines.len() - skip - shown;
        if shown < lines.len() - skip {
            out.push_str(&format!(
                "[{remaining} more lines; pass offset={} to continue]",
                skip + shown + 1
            ));
        }
        if out.is_empty() {
            out.push_str("[empty file]");
        }
        Ok(ToolOutput::ok(out))
    }
}

pub struct WriteFileTool;

#[derive(Deserialize)]
struct WriteArgs {
    path: String,
    content: String,
}

#[async_trait::async_trait]
impl Tool for WriteFileTool {
    fn name(&self) -> &'static str {
        "write_file"
    }

    fn description(&self) -> &'static str {
        "Create or overwrite a file with the given content. Parent directories are created."
    }

    fn kind(&self) -> ToolKind {
        ToolKind::Mutating
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "File path to write" },
                "content": { "type": "string", "description": "Full file content" },
            },
            "required": ["path", "content"],
        })
    }

    async fn execute(&self, args: Value, ctx: &ToolCtx) -> Result<ToolOutput> {
        let args: WriteArgs = serde_json::from_value(args)
            .map_err(|e| CoreError::Tool("write_file".into(), e.to_string()))?;
        let path = resolve_path(&ctx.cwd, &args.path);
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)
                .map_err(|e| CoreError::Tool("write_file".into(), format!("{}: {e}", args.path)))?;
        }
        std::fs::write(&path, &args.content)
            .map_err(|e| CoreError::Tool("write_file".into(), format!("{}: {e}", args.path)))?;
        let lines = args
            .content
            .lines()
            .count()
            .max(if args.content.is_empty() { 0 } else { 1 });
        Ok(ToolOutput::ok(format!(
            "wrote {lines} lines ({}) to {}",
            args.content.len(),
            display_path(&ctx.cwd, &path)
        )))
    }
}

pub struct EditFileTool;

#[derive(Deserialize)]
struct EditArgs {
    path: String,
    old_string: String,
    new_string: String,
    #[serde(default)]
    replace_all: bool,
}

#[async_trait::async_trait]
impl Tool for EditFileTool {
    fn name(&self) -> &'static str {
        "edit_file"
    }

    fn description(&self) -> &'static str {
        "Replace an exact substring in a file. old_string must match exactly and be unique \
         unless replace_all is set. Provide a few surrounding lines to make it unique."
    }

    fn kind(&self) -> ToolKind {
        ToolKind::Mutating
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "File path to edit" },
                "old_string": { "type": "string", "description": "Exact text to replace" },
                "new_string": { "type": "string", "description": "Replacement text" },
                "replace_all": { "type": "boolean", "description": "Replace every occurrence" },
            },
            "required": ["path", "old_string", "new_string"],
        })
    }

    async fn execute(&self, args: Value, ctx: &ToolCtx) -> Result<ToolOutput> {
        let args: EditArgs = serde_json::from_value(args)
            .map_err(|e| CoreError::Tool("edit_file".into(), e.to_string()))?;
        let path = resolve_path(&ctx.cwd, &args.path);
        if args.old_string.is_empty() {
            return Ok(ToolOutput::err("old_string must not be empty"));
        }
        let content = std::fs::read_to_string(&path)
            .map_err(|e| CoreError::Tool("edit_file".into(), format!("{}: {e}", args.path)))?;
        let count = content.matches(&args.old_string).count();
        match count {
            0 => Ok(ToolOutput::err(format!(
                "old_string was not found in {}; read the file and retry with the exact text",
                args.path
            ))),
            1 => {
                let updated = content.replacen(&args.old_string, &args.new_string, 1);
                persist(&path, &updated, &args.path)?;
                Ok(ToolOutput::ok(format!(
                    "replaced 1 occurrence in {}",
                    display_path(&ctx.cwd, &path)
                )))
            }
            n if args.replace_all => {
                let updated = content.replace(&args.old_string, &args.new_string);
                persist(&path, &updated, &args.path)?;
                Ok(ToolOutput::ok(format!(
                    "replaced {n} occurrences in {}",
                    display_path(&ctx.cwd, &path)
                )))
            }
            n => Ok(ToolOutput::err(format!(
                "old_string matches {n} times in {}; add context to make it unique or set replace_all",
                args.path
            ))),
        }
    }
}

fn persist(path: &Path, content: &str, label: &str) -> Result<()> {
    std::fs::write(path, content)
        .map_err(|e| CoreError::Tool("edit_file".into(), format!("{label}: {e}")))
}

pub struct LsTool;

#[derive(Deserialize)]
struct LsArgs {
    #[serde(default)]
    path: Option<String>,
}

#[async_trait::async_trait]
impl Tool for LsTool {
    fn name(&self) -> &'static str {
        "ls"
    }

    fn description(&self) -> &'static str {
        "List a directory's entries with file types. Directories get a trailing slash."
    }

    fn kind(&self) -> ToolKind {
        ToolKind::ReadOnly
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Directory path (default: cwd)" },
            },
        })
    }

    async fn execute(&self, args: Value, ctx: &ToolCtx) -> Result<ToolOutput> {
        let args: LsArgs = serde_json::from_value(args)
            .map_err(|e| CoreError::Tool("ls".into(), e.to_string()))?;
        let path = match &args.path {
            Some(p) => resolve_path(&ctx.cwd, p),
            None => ctx.cwd.clone(),
        };
        let entries = std::fs::read_dir(&path)
            .map_err(|e| CoreError::Tool("ls".into(), format!("{}: {e}", path.display())))?;
        let mut rows: Vec<String> = Vec::new();
        for entry in entries.filter_map(|e| e.ok()) {
            let name = entry.file_name().to_string_lossy().into_owned();
            let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
            rows.push(if is_dir { format!("{name}/") } else { name });
        }
        rows.sort();
        let total = rows.len();
        let shown: Vec<String> = rows.into_iter().take(MAX_LS_ENTRIES).collect();
        let mut out = shown.join("\n");
        if total > MAX_LS_ENTRIES {
            out.push_str(&format!("\n[{} more entries]", total - MAX_LS_ENTRIES));
        }
        if out.is_empty() {
            out = "[empty directory]".to_string();
        }
        Ok(ToolOutput::ok(out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio_util::sync::CancellationToken;

    fn ctx(dir: &Path) -> ToolCtx {
        ToolCtx {
            cwd: dir.to_path_buf(),
            timeout: Duration::from_secs(30),
            cancel: CancellationToken::new(),
        }
    }

    #[tokio::test]
    async fn write_read_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let c = ctx(tmp.path());
        let out = WriteFileTool
            .execute(
                json!({ "path": "nested/a.txt", "content": "one\ntwo\nthree\n" }),
                &c,
            )
            .await
            .unwrap();
        assert!(out.ok);
        assert!(tmp.path().join("nested/a.txt").exists());

        let out = ReadFileTool
            .execute(json!({ "path": "nested/a.txt" }), &c)
            .await
            .unwrap();
        assert!(out.content.contains("     1\tone"));
        assert!(out.content.contains("     3\tthree"));

        let out = ReadFileTool
            .execute(
                json!({ "path": "nested/a.txt", "offset": 2, "limit": 1 }),
                &c,
            )
            .await
            .unwrap();
        assert!(out.content.contains("two"));
        assert!(!out.content.contains("one\n"));
        assert!(out.content.contains("[1 more lines"));
    }

    #[tokio::test]
    async fn edit_requires_unique_match() {
        let tmp = tempfile::tempdir().unwrap();
        let c = ctx(tmp.path());
        WriteFileTool
            .execute(
                json!({ "path": "a.txt", "content": "alpha beta alpha\n" }),
                &c,
            )
            .await
            .unwrap();

        let out = EditFileTool
            .execute(
                json!({ "path": "a.txt", "old_string": "alpha", "new_string": "x" }),
                &c,
            )
            .await
            .unwrap();
        assert!(!out.ok);
        assert!(out.content.contains("2 times"));

        let out = EditFileTool
            .execute(
                json!({ "path": "a.txt", "old_string": "alpha", "new_string": "x", "replace_all": true }),
                &c,
            )
            .await
            .unwrap();
        assert!(out.ok);
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("a.txt")).unwrap(),
            "x beta x\n"
        );

        let out = EditFileTool
            .execute(
                json!({ "path": "a.txt", "old_string": "gamma", "new_string": "x" }),
                &c,
            )
            .await
            .unwrap();
        assert!(!out.ok);
        assert!(out.content.contains("not found"));
    }

    #[tokio::test]
    async fn lists_directories() {
        let tmp = tempfile::tempdir().unwrap();
        let c = ctx(tmp.path());
        std::fs::write(tmp.path().join("f.txt"), "x").unwrap();
        std::fs::create_dir(tmp.path().join("d")).unwrap();
        let out = LsTool.execute(json!({}), &c).await.unwrap();
        assert!(out.content.contains("d/"));
        assert!(out.content.contains("f.txt"));
    }
}
