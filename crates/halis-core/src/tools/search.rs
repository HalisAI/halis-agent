use super::{Tool, ToolCtx, ToolKind, display_path, resolve_path};
use crate::error::{CoreError, Result};
use crate::message::ToolOutput;
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::Path;

const DEFAULT_LIMIT: usize = 100;
const MAX_LIMIT: usize = 500;
const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;
const MAX_LINE_CHARS: usize = 500;

pub struct GlobTool;

#[derive(Deserialize)]
struct GlobArgs {
    pattern: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
}

#[async_trait::async_trait]
impl Tool for GlobTool {
    fn name(&self) -> &'static str {
        "glob"
    }

    fn description(&self) -> &'static str {
        "Find files matching a glob pattern (e.g. \"**/*.rs\") under a directory. \
         Respects .gitignore and skips hidden files by default."
    }

    fn kind(&self) -> ToolKind {
        ToolKind::ReadOnly
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "Glob pattern, matched against paths relative to the search root" },
                "path": { "type": "string", "description": "Directory to search (default: cwd)" },
                "limit": { "type": "integer", "description": "Maximum number of results (default 100, max 500)" },
            },
            "required": ["pattern"],
        })
    }

    async fn execute(&self, args: Value, ctx: &ToolCtx) -> Result<ToolOutput> {
        let args: GlobArgs = serde_json::from_value(args)
            .map_err(|e| CoreError::Tool("glob".into(), e.to_string()))?;
        let root = match &args.path {
            Some(p) => resolve_path(&ctx.cwd, p),
            None => ctx.cwd.clone(),
        };
        let glob = globset::GlobBuilder::new(&args.pattern)
            .literal_separator(true)
            .build()
            .map_err(|e| CoreError::Tool("glob".into(), format!("bad pattern: {e}")))?
            .compile_matcher();
        let limit = args.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);

        let mut hits: Vec<String> = Vec::new();
        for entry in walk(&root) {
            let rel = match entry.strip_prefix(&root) {
                Ok(r) => r,
                Err(_) => continue,
            };
            if rel.as_os_str().is_empty() {
                continue;
            }
            let rel = rel.to_string_lossy().replace('\\', "/");
            if glob.is_match(&rel) {
                hits.push(display_path(&ctx.cwd, &entry));
                if hits.len() >= limit {
                    break;
                }
            }
        }
        hits.sort();
        hits.dedup();
        if hits.is_empty() {
            return Ok(ToolOutput::ok("no matches"));
        }
        let mut out = hits.join("\n");
        if hits.len() >= limit {
            out.push_str(&format!(
                "\n[result limit {limit} reached; refine the pattern]"
            ));
        }
        Ok(ToolOutput::ok(out))
    }
}

pub struct GrepTool;

#[derive(Deserialize)]
struct GrepArgs {
    pattern: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    glob: Option<String>,
    #[serde(default)]
    ignore_case: bool,
    #[serde(default)]
    limit: Option<usize>,
}

#[async_trait::async_trait]
impl Tool for GrepTool {
    fn name(&self) -> &'static str {
        "grep"
    }

    fn description(&self) -> &'static str {
        "Search file contents with a regex; returns path:line:text matches. \
         Respects .gitignore and skips hidden files and binaries."
    }

    fn kind(&self) -> ToolKind {
        ToolKind::ReadOnly
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "Rust regex" },
                "path": { "type": "string", "description": "File or directory to search (default: cwd)" },
                "glob": { "type": "string", "description": "Only search files matching this glob (e.g. \"*.rs\")" },
                "ignore_case": { "type": "boolean", "description": "Case-insensitive matching" },
                "limit": { "type": "integer", "description": "Maximum number of matches (default 100, max 500)" },
            },
            "required": ["pattern"],
        })
    }

    async fn execute(&self, args: Value, ctx: &ToolCtx) -> Result<ToolOutput> {
        let args: GrepArgs = serde_json::from_value(args)
            .map_err(|e| CoreError::Tool("grep".into(), e.to_string()))?;
        let re = regex::RegexBuilder::new(&args.pattern)
            .case_insensitive(args.ignore_case)
            .build()
            .map_err(|e| CoreError::Tool("grep".into(), format!("bad regex: {e}")))?;
        let root = match &args.path {
            Some(p) => resolve_path(&ctx.cwd, p),
            None => ctx.cwd.clone(),
        };
        let file_glob = match &args.glob {
            Some(g) => Some(
                globset::GlobBuilder::new(g)
                    .literal_separator(true)
                    .build()
                    .map_err(|e| CoreError::Tool("grep".into(), format!("bad glob: {e}")))?
                    .compile_matcher(),
            ),
            None => None,
        };
        let limit = args.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);

        if root.is_file() {
            let matches = scan_file(&root, &root, &re, &file_glob, limit, &ctx.cwd);
            return Ok(finish(matches, limit));
        }

        let mut all: Vec<String> = Vec::new();
        'outer: for path in walk(&root) {
            if !path.is_file() {
                continue;
            }
            if let Some(g) = &file_glob {
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                if !g.is_match(&name) {
                    continue;
                }
            }
            for m in scan_file(&path, &root, &re, &file_glob, limit, &ctx.cwd) {
                all.push(m);
                if all.len() >= limit {
                    break 'outer;
                }
            }
        }
        Ok(finish(all, limit))
    }
}

fn finish(mut matches: Vec<String>, limit: usize) -> ToolOutput {
    matches.dedup();
    if matches.is_empty() {
        return ToolOutput::ok("no matches");
    }
    let mut out = matches.join("\n");
    if matches.len() >= limit {
        out.push_str(&format!(
            "\n[match limit {limit} reached; narrow the search]"
        ));
    }
    ToolOutput::ok(out)
}

fn walk(root: &Path) -> impl Iterator<Item = std::path::PathBuf> {
    let root = root.to_path_buf();
    ignore::WalkBuilder::new(&root)
        .hidden(true)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .parents(true)
        .build()
        .filter_map(|e| e.ok())
        .map(|e| e.path().to_path_buf())
        .filter(move |p| *p != root)
}

fn scan_file(
    path: &Path,
    root: &Path,
    re: &regex::Regex,
    file_glob: &Option<globset::GlobMatcher>,
    limit: usize,
    cwd: &Path,
) -> Vec<String> {
    let _ = root;
    let _ = file_glob;
    let Ok(meta) = std::fs::metadata(path) else {
        return Vec::new();
    };
    if meta.len() > MAX_FILE_BYTES {
        return Vec::new();
    }
    let Ok(bytes) = std::fs::read(path) else {
        return Vec::new();
    };
    if bytes.contains(&0) {
        return Vec::new();
    }
    let content = String::from_utf8_lossy(&bytes);
    let display = display_path(cwd, path);
    let mut out = Vec::new();
    for (i, line) in content.lines().enumerate() {
        if re.is_match(line) {
            let text: String = line.trim().chars().take(MAX_LINE_CHARS).collect();
            out.push(format!("{display}:{}:{text}", i + 1));
            if out.len() >= limit {
                break;
            }
        }
    }
    out
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
    async fn glob_finds_nested_files() {
        let tmp = tempfile::tempdir().unwrap();
        let c = ctx(tmp.path());
        std::fs::create_dir_all(tmp.path().join("src/deep")).unwrap();
        std::fs::write(tmp.path().join("src/deep/mod.rs"), "x").unwrap();
        std::fs::write(tmp.path().join("src/main.rs"), "x").unwrap();
        std::fs::write(tmp.path().join("notes.txt"), "x").unwrap();

        let out = GlobTool
            .execute(json!({ "pattern": "**/*.rs" }), &c)
            .await
            .unwrap();
        assert!(out.content.contains("src/main.rs"));
        assert!(out.content.contains("src/deep/mod.rs"));
        assert!(!out.content.contains("notes.txt"));

        let out = GlobTool
            .execute(json!({ "pattern": "*.rs" }), &c)
            .await
            .unwrap();
        assert_eq!(out.content, "no matches");
    }

    #[tokio::test]
    async fn grep_reports_line_numbers() {
        let tmp = tempfile::tempdir().unwrap();
        let c = ctx(tmp.path());
        std::fs::write(tmp.path().join("a.txt"), "keep\nFIND me\nkeep\n").unwrap();
        std::fs::write(tmp.path().join("b.txt"), "nothing").unwrap();

        let out = GrepTool
            .execute(json!({ "pattern": "find", "ignore_case": true }), &c)
            .await
            .unwrap();
        assert!(
            out.content.contains("a.txt:2:FIND me"),
            "got: {}",
            out.content
        );
        assert!(!out.content.contains("b.txt"));

        let out = GrepTool
            .execute(json!({ "pattern": "find" }), &c)
            .await
            .unwrap();
        assert_eq!(out.content, "no matches");
    }

    #[tokio::test]
    async fn grep_respects_glob_filter() {
        let tmp = tempfile::tempdir().unwrap();
        let c = ctx(tmp.path());
        std::fs::write(tmp.path().join("a.rs"), "needle").unwrap();
        std::fs::write(tmp.path().join("a.txt"), "needle").unwrap();
        let out = GrepTool
            .execute(json!({ "pattern": "needle", "glob": "*.rs" }), &c)
            .await
            .unwrap();
        assert!(out.content.contains("a.rs"));
        assert!(!out.content.contains("a.txt"));
    }
}
