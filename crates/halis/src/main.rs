use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use halis_core::agent::{Agent, AgentEvent, Approver, Decision};
use halis_core::catalog;
use halis_core::config::{ApprovalMode, Config, Effort};
use halis_core::message::ToolCall;
use halis_core::resolver::{self};
use halis_core::session::Session;
use halis_core::util;
use std::io::{IsTerminal, Write};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Copy, clap::ValueEnum)]
#[value(rename_all = "lowercase")]
enum EffortArg {
    Low,
    Medium,
    High,
}

impl From<EffortArg> for Effort {
    fn from(v: EffortArg) -> Self {
        match v {
            EffortArg::Low => Effort::Low,
            EffortArg::Medium => Effort::Medium,
            EffortArg::High => Effort::High,
        }
    }
}

#[derive(Parser)]
#[command(
    name = "halis",
    version,
    about = "Halis — a coding agent for your terminal"
)]
struct Cli {
    /// Prompt; omit to start an interactive session
    prompt: Vec<String>,
    /// Model spec: "provider/model", model id, or a unique substring
    #[arg(short, long)]
    model: Option<String>,
    /// Auto-approve every tool call
    #[arg(long)]
    yolo: bool,
    /// Reasoning effort for models that support it
    #[arg(long)]
    effort: Option<EffortArg>,
    /// Maximum model round-trips per turn
    #[arg(long)]
    max_turns: Option<u32>,
    /// Continue the most recent session
    #[arg(short = 'C', long)]
    r#continue: bool,
    /// Resume a session by id prefix
    #[arg(short = 's', long)]
    session: Option<String>,
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// List models from the embedded catalog
    Models {
        /// Filter by provider id
        #[arg(short, long)]
        provider: Option<String>,
        /// Case-insensitive substring over provider/model/name
        #[arg(short, long)]
        search: Option<String>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .with_target(false)
        .init();

    let cli = Cli::parse();
    let cfg = Config::load().context("loading config")?;

    match cli.cmd {
        Some(Cmd::Models { provider, search }) => {
            list_models(provider.as_deref(), search.as_deref());
            Ok(())
        }
        None => run(cli, cfg).await,
    }
}

async fn run(cli: Cli, cfg: Config) -> Result<()> {
    let env_model = std::env::var("HALIS_MODEL").ok();
    let spec = cli.model.as_deref().or(env_model.as_deref());
    let model = resolver::resolve(spec, &cfg).context("resolving model")?;

    let cwd = std::env::current_dir().context("reading cwd")?;
    let session = if cli.session.is_some() || cli.r#continue {
        Session::load_latest(cli.session.as_deref()).context("resuming session")?
    } else {
        Session::create(cwd.clone(), &model.label()).context("creating session")?
    };

    let mut agent_cfg = halis_core::agent::AgentConfig::defaults(model.clone());
    if cli.yolo {
        agent_cfg.approval = ApprovalMode::Auto;
    } else {
        agent_cfg.approval = cfg.approval;
    }
    agent_cfg.effort = cli.effort.map(Effort::from).or(cfg.effort);
    agent_cfg.max_turns = cli.max_turns.or(cfg.max_turns).unwrap_or(40);
    agent_cfg.bash_timeout = Duration::from_secs(cfg.bash_timeout_secs.unwrap_or(120));

    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let mut agent = Agent::new(agent_cfg, session, tx).context("starting agent")?;

    let render = tokio::spawn(render_loop(rx));
    let slot = CancelSlot::new();
    tokio::spawn(signal_task(slot.clone()));

    banner(&agent);

    let approver: Box<dyn Approver> = if cli.yolo || cfg.approval == ApprovalMode::Auto {
        Box::new(AutoApprover)
    } else if std::io::stdin().is_terminal() {
        Box::new(PromptApprover)
    } else {
        Box::new(HeadlessApprover::new())
    };
    let mut approver = approver;

    let prompt = cli.prompt.join(" ");
    if !prompt.trim().is_empty() {
        let token = CancellationToken::new();
        slot.set(token.clone());
        let result = agent.run_turn(&prompt, &token, approver.as_mut()).await;
        slot.clear();
        match result {
            Ok(stats) => print_stats(&stats),
            Err(e) => {
                print_error(&e.to_string());
                drop(agent);
                render.abort();
                std::process::exit(1);
            }
        }
    } else {
        repl(&mut agent, approver.as_mut(), &slot).await?;
    }

    drop(agent);
    render.abort();
    Ok(())
}

async fn repl(agent: &mut Agent, approver: &mut dyn Approver, slot: &CancelSlot) -> Result<()> {
    let interactive = std::io::stdin().is_terminal();
    loop {
        if interactive {
            eprint!("{}", style("halis › ", Style::Cyan));
            std::io::stderr().flush().ok();
        }
        let line = match read_line().await? {
            Some(l) => l,
            None => break,
        };
        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('/') {
            if !handle_command(agent, &line)? {
                break;
            }
            continue;
        }
        let token = CancellationToken::new();
        slot.set(token.clone());
        let result = agent.run_turn(&line, &token, approver).await;
        slot.clear();
        match result {
            Ok(stats) => print_stats(&stats),
            Err(e) => print_error(&e.to_string()),
        }
    }
    Ok(())
}

fn handle_command(agent: &mut Agent, line: &str) -> Result<bool> {
    let (cmd, rest) = match line.split_once(' ') {
        Some((c, r)) => (c, r.trim()),
        None => (line, ""),
    };
    match cmd {
        "/exit" | "/quit" => return Ok(false),
        "/help" => {
            eprintln!("{}", style("commands:", Style::Cyan));
            for (name, desc) in [
                ("/model [spec]", "show or switch the active model"),
                ("/models", "list catalog models (or run `halis models`)"),
                ("/tools", "list available tools"),
                ("/usage", "session token totals"),
                ("/clear", "start a fresh session"),
                ("/help", "show this list"),
                ("/exit", "quit"),
            ] {
                eprintln!("  {name:<24} {desc}");
            }
        }
        "/model" => {
            if rest.is_empty() {
                let m = agent.model();
                eprintln!(
                    "{} {} ({})",
                    style("model:", Style::Cyan),
                    m.label(),
                    m.display
                );
            } else {
                let cfg = Config::load().unwrap_or_default();
                match resolver::resolve(Some(rest), &cfg) {
                    Ok(m) => {
                        let label = m.label();
                        agent.set_model(m).context("switching model")?;
                        eprintln!("{} switched to {label}", style("·", Style::Green));
                    }
                    Err(e) => print_error(&e.to_string()),
                }
            }
        }
        "/models" => {
            list_models(None, None);
        }
        "/tools" => {
            for t in agent.tools().list() {
                let kind = match t.kind() {
                    halis_core::tools::ToolKind::ReadOnly => "read",
                    halis_core::tools::ToolKind::Mutating => "write",
                };
                eprintln!("  {:<12} {:<6} {}", t.name(), kind, t.description());
            }
        }
        "/usage" => {
            let t = agent.totals();
            eprintln!(
                "{} in {} · out {} · cache read {}",
                style("session:", Style::Cyan),
                util::fmt_tokens(t.input),
                util::fmt_tokens(t.output),
                util::fmt_tokens(t.cache_read)
            );
        }
        "/clear" => {
            let cwd = agent.session().cwd.clone();
            let label = agent.model().label();
            let session = Session::create(cwd, &label).context("creating session")?;
            agent.reset(session);
            eprintln!("{} started a fresh session", style("·", Style::Green));
        }
        other => eprintln!(
            "{} unknown command {other}; try /help",
            style("·", Style::Yellow)
        ),
    }
    Ok(true)
}

async fn read_line() -> Result<Option<String>> {
    tokio::task::spawn_blocking(|| {
        let mut buf = String::new();
        match std::io::stdin().read_line(&mut buf) {
            Ok(0) => Ok(None),
            Ok(_) => Ok(Some(buf)),
            Err(e) => Err(anyhow::anyhow!("stdin: {e}")),
        }
    })
    .await
    .context("reading stdin")?
}

struct AutoApprover;

impl Approver for AutoApprover {
    fn decide(&mut self, _call: &ToolCall, _preview: &str) -> halis_core::error::Result<Decision> {
        Ok(Decision::Approve)
    }
}

struct PromptApprover;

impl Approver for PromptApprover {
    fn decide(&mut self, call: &ToolCall, preview: &str) -> halis_core::error::Result<Decision> {
        eprintln!(
            "\n{} {} {}",
            style("approve:", Style::Cyan),
            call.name,
            style(preview, Style::Dim)
        );
        eprint!("{} ", style("[y]es / (n)o / (a)lways:", Style::Cyan));
        std::io::stderr().flush().ok();
        let mut buf = String::new();
        match std::io::stdin().read_line(&mut buf) {
            Ok(0) => Ok(Decision::Deny),
            Ok(_) => match buf.trim().to_lowercase().as_str() {
                "y" | "yes" => Ok(Decision::Approve),
                "a" | "always" => Ok(Decision::Always),
                _ => Ok(Decision::Deny),
            },
            Err(_) => Ok(Decision::Deny),
        }
    }
}

struct HeadlessApprover {
    warned: bool,
}

impl HeadlessApprover {
    fn new() -> Self {
        Self { warned: false }
    }
}

impl Approver for HeadlessApprover {
    fn decide(&mut self, _call: &ToolCall, _preview: &str) -> halis_core::error::Result<Decision> {
        if !self.warned {
            self.warned = true;
            eprintln!(
                "{}",
                style(
                    "stdin is not a terminal; mutating tools are denied \
                     (rerun with --yolo to allow them)",
                    Style::Yellow
                )
            );
        }
        Ok(Decision::Deny)
    }
}

async fn render_loop(mut rx: tokio::sync::mpsc::UnboundedReceiver<AgentEvent>) {
    while let Some(ev) = rx.recv().await {
        match ev {
            AgentEvent::TurnStarted { model } => {
                eprintln!("{} {}", style("·", Style::Dim), style(model, Style::Dim));
            }
            AgentEvent::TextDelta(t) => {
                let mut out = std::io::stdout().lock();
                out.write_all(t.as_bytes()).ok();
                out.flush().ok();
            }
            AgentEvent::ThinkingDelta(t) => {
                eprint!("{}", style(&t, Style::Dim));
                std::io::stderr().flush().ok();
            }
            AgentEvent::ToolCall { name, preview } => {
                eprintln!(
                    "{} {} {}",
                    style("»", Style::Cyan),
                    name,
                    style(preview, Style::Dim)
                );
            }
            AgentEvent::ToolResult { name, ok, preview } => {
                let (mark, color) = if ok {
                    ("·", Style::Green)
                } else {
                    ("·", Style::Red)
                };
                eprintln!(
                    "{} {} {} {}",
                    style(mark, color),
                    name,
                    style(if ok { "ok:" } else { "failed:" }, color),
                    style(preview, Style::Dim)
                );
            }
            AgentEvent::Warning(w) => {
                eprintln!("{} {}", style("·", Style::Yellow), w);
            }
            AgentEvent::Aborted => {
                eprintln!("{}", style("· interrupted", Style::Yellow));
            }
        }
    }
}

fn banner(agent: &Agent) {
    let m = agent.model();
    let session = agent.session();
    eprintln!(
        "{}",
        style(
            format!(
                "halis {} · model {} · session {}",
                env!("CARGO_PKG_VERSION"),
                m.label(),
                session.id
            ),
            Style::Dim
        )
    );
    if let Some(path) = session.path() {
        eprintln!("{}", style(format!("log: {}", path.display()), Style::Dim));
    }
    eprintln!(
        "{}",
        style(
            "type /help for commands · ctrl-c interrupts a running turn",
            Style::Dim
        )
    );
}

fn print_stats(stats: &halis_core::message::TurnStats) {
    eprintln!(
        "{}",
        style(
            format!(
                "turns {} · in {} · out {} · {} tool calls · ${:.4} · {:.1}s",
                stats.turns,
                util::fmt_tokens(stats.usage.input),
                util::fmt_tokens(stats.usage.output),
                stats.tool_calls,
                stats.cost,
                stats.secs
            ),
            Style::Dim
        )
    );
}

fn print_error(msg: &str) {
    eprintln!("{} {}", style("error:", Style::Red), msg);
}

#[derive(Clone, Copy)]
enum Style {
    Red,
    Green,
    Cyan,
    Yellow,
    Dim,
}

impl Style {
    fn code(self) -> &'static str {
        match self {
            Style::Red => "\x1b[31m",
            Style::Green => "\x1b[32m",
            Style::Cyan => "\x1b[36m",
            Style::Yellow => "\x1b[33m",
            Style::Dim => "\x1b[2m",
        }
    }
}

fn style(text: impl AsRef<str>, style: Style) -> String {
    let enabled = std::io::stderr().is_terminal() && std::env::var_os("NO_COLOR").is_none();
    let text = text.as_ref();
    if enabled {
        format!("{}{}\x1b[0m", style.code(), text)
    } else {
        text.to_string()
    }
}

#[derive(Clone)]
struct CancelSlot(Arc<Mutex<Option<CancellationToken>>>);

impl CancelSlot {
    fn new() -> Self {
        Self(Arc::new(Mutex::new(None)))
    }

    fn set(&self, token: CancellationToken) {
        *self.0.lock().unwrap() = Some(token);
    }

    fn clear(&self) {
        *self.0.lock().unwrap() = None;
    }

    fn take(&self) -> Option<CancellationToken> {
        self.0.lock().unwrap().take()
    }
}

async fn signal_task(slot: CancelSlot) {
    loop {
        if tokio::signal::ctrl_c().await.is_err() {
            return;
        }
        match slot.take() {
            Some(token) => {
                token.cancel();
                eprintln!("{}", style("· interrupting…", Style::Yellow));
            }
            None => std::process::exit(130),
        }
    }
}

fn list_models(provider_filter: Option<&str>, search: Option<&str>) {
    let providers = catalog::providers();
    let models = catalog::models();
    let search = search.map(|s| s.to_lowercase());

    let mut rows: Vec<(usize, &halis_core::catalog::ModelEntry)> = Vec::new();
    for m in models {
        let pid = &providers[m.provider as usize].id;
        if let Some(f) = provider_filter
            && !pid.eq_ignore_ascii_case(f)
        {
            continue;
        }
        if let Some(s) = &search {
            let hay = format!("{}/{}/{}", pid, m.id, m.name).to_lowercase();
            if !hay.contains(s) {
                continue;
            }
        }
        rows.push((m.provider as usize, m));
    }

    if rows.is_empty() {
        println!("no models matched");
        return;
    }

    println!(
        "{:<48} {:<28} {:>9} {:>5} {:>13}",
        "MODEL", "NAME", "CONTEXT", "CAPS", "$/1M IN/OUT"
    );
    let mut shown_providers = 0usize;
    let mut last_provider = usize::MAX;
    for (pidx, m) in &rows {
        let p = &providers[*pidx];
        if *pidx != last_provider {
            shown_providers += 1;
            last_provider = *pidx;
        }
        let mut caps = String::new();
        if m.tools() {
            caps.push('T');
        }
        if m.vision() {
            caps.push('V');
        }
        if m.reasoning() {
            caps.push('R');
        }
        if m.deprecated() {
            caps.push('!');
        }
        let ready = if p.env.iter().any(|k| std::env::var(k).is_ok()) {
            "*"
        } else {
            ""
        };
        let ctx = m
            .context
            .map(util::fmt_context)
            .unwrap_or_else(|| "-".into());
        let price = format!(
            "{}/{}",
            util::fmt_price(m.price_in),
            util::fmt_price(m.price_out)
        );
        println!(
            "{:<48} {:<28} {:>9} {:>5} {:>13}{}",
            format!("{}/{}", p.id, m.id),
            util::truncate_chars(&m.name, 27),
            ctx,
            if caps.is_empty() { "-".into() } else { caps },
            price,
            ready
        );
    }
    eprintln!(
        "{} models from {} providers{}",
        rows.len(),
        shown_providers,
        style(
            " · * = api key detected · T/V/R/! = tools/vision/reasoning/deprecated",
            Style::Dim
        )
    );
}
