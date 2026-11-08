use super::{MAX_BASH_OUTPUT_CHARS, Tool, ToolCtx, ToolKind};
use crate::error::{CoreError, Result};
use crate::message::ToolOutput;
use crate::util::truncate_middle;
use serde::Deserialize;
use serde_json::{Value, json};
use std::process::Stdio;
use std::time::Duration;

pub struct BashTool;

#[derive(Deserialize)]
struct Args {
    command: String,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

const MAX_TIMEOUT_MS: u64 = 600_000;

#[async_trait::async_trait]
impl Tool for BashTool {
    fn name(&self) -> &'static str {
        "bash"
    }

    fn description(&self) -> &'static str {
        "Run a command in the working directory and return stdout, stderr and the exit code. \
         Prefer this for builds, tests and git. Avoid commands that wait for input."
    }

    fn kind(&self) -> ToolKind {
        ToolKind::Mutating
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "The shell command to execute",
                },
                "timeout_ms": {
                    "type": "integer",
                    "description": "Optional timeout in milliseconds (default 120000, max 600000)",
                },
            },
            "required": ["command"],
        })
    }

    async fn execute(&self, args: Value, ctx: &ToolCtx) -> Result<ToolOutput> {
        let args: Args = serde_json::from_value(args)
            .map_err(|e| CoreError::Tool("bash".into(), e.to_string()))?;
        if args.command.trim().is_empty() {
            return Ok(ToolOutput::err("empty command"));
        }
        let timeout = args
            .timeout_ms
            .map(Duration::from_millis)
            .unwrap_or(ctx.timeout)
            .min(Duration::from_millis(MAX_TIMEOUT_MS));

        let mut cmd = tokio::process::Command::new("bash");
        cmd.arg("-c")
            .arg(&args.command)
            .current_dir(&ctx.cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        {
            cmd.process_group(0);
        }
        let mut child = cmd
            .spawn()
            .map_err(|e| CoreError::Tool("bash".into(), format!("spawn failed: {e}")))?;

        let stdout_handle = child.stdout.take().expect("piped stdout");
        let stderr_handle = child.stderr.take().expect("piped stderr");
        let out_task = tokio::spawn(read_all(stdout_handle));
        let err_task = tokio::spawn(read_all(stderr_handle));

        let mut timed_out = false;
        let mut interrupted = false;
        let status = tokio::select! {
            biased;
            _ = ctx.cancel.cancelled() => {
                interrupted = true;
                let _ = child.start_kill();
                child.wait().await
            }
            _ = tokio::time::sleep(timeout) => {
                timed_out = true;
                let _ = child.start_kill();
                child.wait().await
            }
            status = child.wait() => status,
        };
        let status = status.map_err(|e| CoreError::Tool("bash".into(), e.to_string()))?;

        let stdout = out_task.await.unwrap_or_default();
        let stderr = err_task.await.unwrap_or_default();
        let stdout = String::from_utf8_lossy(&stdout);
        let stderr = String::from_utf8_lossy(&stderr);

        let mut out = String::new();
        if !stdout.is_empty() {
            out.push_str(&stdout);
        }
        if !stderr.is_empty() {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str("[stderr]\n");
            out.push_str(&stderr);
        }
        let ok = status.success() && !timed_out && !interrupted;
        if timed_out {
            out.push_str(&format!(
                "\n[command timed out after {:.1}s and was killed]",
                timeout.as_secs_f32()
            ));
        } else if interrupted {
            out.push_str("\n[command was interrupted]");
        } else if let Some(code) = status.code() {
            out.push_str(&format!("\n[exit code: {code}]"));
        } else {
            out.push_str("\n[terminated by signal]");
        }
        Ok(ToolOutput {
            ok,
            content: truncate_middle(out.trim_matches('\n'), MAX_BASH_OUTPUT_CHARS),
        })
    }
}

async fn read_all<R: tokio::io::AsyncRead + Unpin>(mut r: R) -> Vec<u8> {
    use tokio::io::AsyncReadExt;
    let mut buf = Vec::new();
    r.read_to_end(&mut buf).await.ok();
    buf
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::resolve_path;
    use std::path::{Path, PathBuf};
    use tokio_util::sync::CancellationToken;

    async fn run(command: &str) -> ToolOutput {
        let ctx = ToolCtx {
            cwd: std::env::temp_dir(),
            timeout: Duration::from_secs(30),
            cancel: CancellationToken::new(),
        };
        BashTool
            .execute(json!({ "command": command }), &ctx)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn captures_output_and_exit_code() {
        let out = run("echo hi; echo oops >&2").await;
        assert!(out.ok);
        assert!(out.content.contains("hi"));
        assert!(out.content.contains("[stderr]"));
        assert!(out.content.contains("oops"));
        assert!(out.content.contains("[exit code: 0]"));

        let out = run("exit 3").await;
        assert!(!out.ok);
        assert!(out.content.contains("[exit code: 3]"));
    }

    #[tokio::test]
    async fn kills_on_timeout() {
        let ctx = ToolCtx {
            cwd: std::env::temp_dir(),
            timeout: Duration::from_millis(200),
            cancel: CancellationToken::new(),
        };
        let out = BashTool
            .execute(json!({ "command": "sleep 5" }), &ctx)
            .await
            .unwrap();
        assert!(!out.ok);
        assert!(out.content.contains("timed out"));
    }

    #[tokio::test]
    async fn honors_cancellation() {
        let cancel = CancellationToken::new();
        let ctx = ToolCtx {
            cwd: std::env::temp_dir(),
            timeout: Duration::from_secs(30),
            cancel: cancel.clone(),
        };
        let task = tokio::spawn(async move {
            BashTool
                .execute(json!({ "command": "sleep 5" }), &ctx)
                .await
                .unwrap()
        });
        tokio::time::sleep(Duration::from_millis(150)).await;
        cancel.cancel();
        let out = task.await.unwrap();
        assert!(!out.ok);
        assert!(out.content.contains("interrupted"));
    }

    #[test]
    fn path_resolution() {
        let cwd = Path::new("/tmp/project");
        assert_eq!(
            resolve_path(cwd, "src/a.rs"),
            PathBuf::from("/tmp/project/src/a.rs")
        );
        assert_eq!(resolve_path(cwd, "/abs/a.rs"), PathBuf::from("/abs/a.rs"));
    }
}
