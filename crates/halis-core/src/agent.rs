use crate::config::{ApprovalMode, Effort};
use crate::error::Result;
use crate::message::{Msg, ToolCall, ToolOutput, TurnStats, Usage};
use crate::provider::{self, CompletionRequest, Provider};
use crate::resolver::ResolvedModel;
use crate::session::Session;
use crate::tools::{Registry, ToolCtx, ToolKind};
use crate::util;
use futures_util::StreamExt;
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

pub type EventTx = tokio::sync::mpsc::UnboundedSender<AgentEvent>;

#[derive(Debug, Clone)]
pub enum AgentEvent {
    TurnStarted {
        model: String,
    },
    TextDelta(String),
    ThinkingDelta(String),
    ToolCall {
        name: String,
        preview: String,
    },
    ToolResult {
        name: String,
        ok: bool,
        preview: String,
    },
    Warning(String),
    Aborted,
}

#[derive(Debug, Clone, Copy)]
pub enum Decision {
    Approve,
    Always,
    Deny,
}

pub trait Approver: Send + Sync {
    fn decide(&mut self, call: &ToolCall, preview: &str) -> Result<Decision>;
}

pub struct AgentConfig {
    pub model: ResolvedModel,
    pub approval: ApprovalMode,
    pub effort: Option<Effort>,
    pub temperature: Option<f32>,
    pub max_turns: u32,
    pub bash_timeout: Duration,
}

impl AgentConfig {
    pub fn defaults(model: ResolvedModel) -> Self {
        Self {
            model,
            approval: ApprovalMode::Confirm,
            effort: None,
            temperature: None,
            max_turns: 40,
            bash_timeout: Duration::from_secs(120),
        }
    }
}

pub struct Agent {
    cfg: AgentConfig,
    provider: Box<dyn Provider>,
    tools: Registry,
    session: Session,
    events: EventTx,
    totals: Usage,
    allow_rest_of_session: bool,
}

impl Agent {
    pub fn new(cfg: AgentConfig, session: Session, events: EventTx) -> Result<Self> {
        let provider = provider::build(&cfg.model)?;
        let mut agent = Self {
            cfg,
            provider,
            tools: Registry::new(),
            session,
            events,
            totals: Usage::default(),
            allow_rest_of_session: false,
        };
        if agent.session.messages().is_empty() {
            let prompt = crate::prompt::default_system_prompt(&agent.session.cwd);
            agent.session.push(Msg::system(prompt), None);
        }
        Ok(agent)
    }

    pub fn with_provider(
        cfg: AgentConfig,
        provider: Box<dyn Provider>,
        session: Session,
        events: EventTx,
    ) -> Self {
        let mut agent = Self {
            cfg,
            provider,
            tools: Registry::new(),
            session,
            events,
            totals: Usage::default(),
            allow_rest_of_session: false,
        };
        if agent.session.messages().is_empty() {
            let prompt = crate::prompt::default_system_prompt(&agent.session.cwd);
            agent.session.push(Msg::system(prompt), None);
        }
        agent
    }

    pub fn set_model(&mut self, model: ResolvedModel) -> Result<()> {
        self.provider = provider::build(&model)?;
        self.cfg.model = model;
        Ok(())
    }

    pub fn model(&self) -> &ResolvedModel {
        &self.cfg.model
    }

    pub fn session(&self) -> &Session {
        &self.session
    }

    pub fn totals(&self) -> Usage {
        self.totals
    }

    pub fn tools(&self) -> &Registry {
        &self.tools
    }

    pub fn reset(&mut self, session: Session) {
        self.session = session;
        self.totals = Usage::default();
        self.allow_rest_of_session = false;
        if self.session.messages().is_empty() {
            let prompt = crate::prompt::default_system_prompt(&self.session.cwd);
            self.session.push(Msg::system(prompt), None);
        }
    }

    pub async fn run_turn(
        &mut self,
        input: &str,
        cancel: &CancellationToken,
        approver: &mut dyn Approver,
    ) -> Result<TurnStats> {
        self.session.push(Msg::user_text(input), None);
        self.emit(AgentEvent::TurnStarted {
            model: self.cfg.model.label(),
        });
        let mut stats = TurnStats::default();
        let started = Instant::now();

        for turn in 1..=self.cfg.max_turns {
            stats.turns = turn;
            let last_turn = turn == self.cfg.max_turns;
            let req = CompletionRequest {
                model: self.cfg.model.clone(),
                messages: self.session.messages().to_vec(),
                tools: self.tools.schemas(),
                max_out: self.cfg.model.max_out,
                effort: self.cfg.effort,
                temperature: self.cfg.temperature,
                tools_enabled: self.cfg.model.supports_tools && !last_turn,
            };

            let mut stream = self.provider.stream(&req).await?;
            let mut text_acc = String::new();
            let mut done: Option<(Msg, Usage)> = None;
            let mut cancelled = false;
            loop {
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => {
                        cancelled = true;
                        break;
                    }
                    item = stream.next() => {
                        match item {
                            None => break,
                            Some(Err(e)) => {
                                if !text_acc.is_empty() {
                                    self.session.push(Msg::assistant_text(&text_acc), None);
                                }
                                return Err(e);
                            }
                            Some(Ok(provider::ProviderEvent::TextDelta(t))) => {
                                text_acc.push_str(&t);
                                self.emit(AgentEvent::TextDelta(t));
                            }
                            Some(Ok(provider::ProviderEvent::ThinkingDelta(t))) => {
                                self.emit(AgentEvent::ThinkingDelta(t));
                            }
                            Some(Ok(provider::ProviderEvent::Done { message, usage })) => {
                                done = Some((message, usage));
                                break;
                            }
                        }
                    }
                }
            }
            drop(stream);

            if cancelled {
                if !text_acc.is_empty() {
                    self.session.push(Msg::assistant_text(&text_acc), None);
                }
                self.emit(AgentEvent::Aborted);
                break;
            }

            let Some((message, usage)) = done else {
                break;
            };
            stats.usage.add(&usage);
            self.session.push(message.clone(), Some(usage));

            let calls = message.tool_calls();
            if calls.is_empty() {
                break;
            }
            if last_turn {
                self.emit(AgentEvent::Warning(
                    "turn limit reached; stopping before further tool calls".into(),
                ));
                break;
            }

            let mut results: Vec<(ToolCall, ToolOutput)> = Vec::with_capacity(calls.len());
            for call in calls {
                let preview = util::args_preview(&call.arguments);
                self.emit(AgentEvent::ToolCall {
                    name: call.name.clone(),
                    preview: preview.clone(),
                });
                let approved = self.approve(&call, &preview, approver)?;
                let output = if approved {
                    let ctx = ToolCtx {
                        cwd: self.session.cwd.clone(),
                        timeout: self.cfg.bash_timeout,
                        cancel: cancel.child_token(),
                    };
                    match self.tools.run(&call.name, &call.arguments, &ctx).await {
                        Ok(o) => o,
                        Err(e) => ToolOutput::err(e.to_string()),
                    }
                } else {
                    ToolOutput::err("the user declined this action")
                };
                self.emit(AgentEvent::ToolResult {
                    name: call.name.clone(),
                    ok: output.ok,
                    preview: util::first_line(&output.content),
                });
                results.push((call, output));
            }
            stats.tool_calls += results.len() as u32;
            self.session.push(Msg::user_tool_results(results), None);
        }

        stats.secs = started.elapsed().as_secs_f64();
        stats.cost = stats
            .usage
            .cost(self.cfg.model.price_in, self.cfg.model.price_out);
        self.totals.add(&stats.usage);
        self.session.push_turn(&stats);
        Ok(stats)
    }

    fn approve(
        &mut self,
        call: &ToolCall,
        preview: &str,
        approver: &mut dyn Approver,
    ) -> Result<bool> {
        if self.cfg.approval == ApprovalMode::Auto || self.allow_rest_of_session {
            return Ok(true);
        }
        let read_only = self
            .tools
            .find(&call.name)
            .map(|t| t.kind() == ToolKind::ReadOnly)
            .unwrap_or(false);
        if read_only {
            return Ok(true);
        }
        match approver.decide(call, preview)? {
            Decision::Approve => Ok(true),
            Decision::Always => {
                self.allow_rest_of_session = true;
                Ok(true)
            }
            Decision::Deny => Ok(false),
        }
    }

    fn emit(&self, ev: AgentEvent) {
        let _ = self.events.send(ev);
    }
}
