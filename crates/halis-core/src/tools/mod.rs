pub mod bash;
pub mod fs;
pub mod search;

use crate::error::{CoreError, Result};
use crate::message::ToolOutput;
use crate::provider::ToolSchema;
use async_trait::async_trait;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

pub const MAX_BASH_OUTPUT_CHARS: usize = 24_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolKind {
    ReadOnly,
    Mutating,
}

#[derive(Debug, Clone)]
pub struct ToolCtx {
    pub cwd: PathBuf,
    pub timeout: Duration,
    pub cancel: CancellationToken,
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &'static str;
    fn description(&self) -> &'static str;
    fn kind(&self) -> ToolKind;
    fn parameters(&self) -> Value;
    async fn execute(&self, args: Value, ctx: &ToolCtx) -> Result<ToolOutput>;
}

pub struct Registry {
    tools: Vec<std::sync::Arc<dyn Tool>>,
}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}

impl Registry {
    pub fn new() -> Self {
        Self {
            tools: vec![
                std::sync::Arc::new(bash::BashTool),
                std::sync::Arc::new(fs::ReadFileTool),
                std::sync::Arc::new(fs::WriteFileTool),
                std::sync::Arc::new(fs::EditFileTool),
                std::sync::Arc::new(fs::LsTool),
                std::sync::Arc::new(search::GlobTool),
                std::sync::Arc::new(search::GrepTool),
            ],
        }
    }

    pub fn list(&self) -> Vec<&dyn Tool> {
        self.tools.iter().map(|t| t.as_ref()).collect()
    }

    pub fn find(&self, name: &str) -> Option<&dyn Tool> {
        self.tools
            .iter()
            .map(|t| t.as_ref())
            .find(|t| t.name() == name)
    }

    pub fn schemas(&self) -> Vec<ToolSchema> {
        self.tools
            .iter()
            .map(|t| ToolSchema {
                name: t.name().to_string(),
                description: t.description().to_string(),
                parameters: t.parameters(),
            })
            .collect()
    }

    pub async fn run(&self, name: &str, args: &Value, ctx: &ToolCtx) -> Result<ToolOutput> {
        let tool = self
            .find(name)
            .ok_or_else(|| CoreError::Tool(name.to_string(), "unknown tool".into()))?;
        tool.execute(args.clone(), ctx).await
    }
}

pub(crate) fn resolve_path(cwd: &Path, raw: &str) -> PathBuf {
    let raw = raw.trim();
    if raw.starts_with('~')
        && let Some(home) = dirs::home_dir()
    {
        if raw == "~" {
            return home;
        }
        if let Some(rest) = raw.strip_prefix("~/") {
            return home.join(rest);
        }
    }
    let p = Path::new(raw);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        cwd.join(p)
    }
}

pub(crate) fn display_path(cwd: &Path, p: &Path) -> String {
    p.strip_prefix(cwd)
        .map(|r| r.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|_| p.to_string_lossy().into_owned())
}
