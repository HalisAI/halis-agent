use halis_core::agent::{Agent, AgentConfig, Approver, Decision};
use halis_core::error::Result;
use halis_core::message::{Block, Msg, Usage};
use halis_core::provider::{BoxStream, CompletionRequest, Provider, ProviderEvent};
use halis_core::resolver::{Protocol, ResolvedModel};
use halis_core::session::Session;
use serde_json::json;
use std::sync::Mutex;
use tokio_util::sync::CancellationToken;

struct Mock {
    responses: Mutex<Vec<Msg>>,
}

impl Mock {
    fn new(responses: Vec<Msg>) -> Self {
        Self {
            responses: Mutex::new(responses.into_iter().rev().collect()),
        }
    }
}

#[async_trait::async_trait]
impl Provider for Mock {
    fn name(&self) -> &str {
        "mock"
    }

    async fn stream(&self, _req: &CompletionRequest) -> Result<BoxStream<Result<ProviderEvent>>> {
        let msg = self
            .responses
            .lock()
            .unwrap()
            .pop()
            .unwrap_or_else(|| Msg::assistant_text("fallback"));
        let usage = Usage {
            input: 10,
            output: 5,
            ..Usage::default()
        };
        let events = match msg.blocks.first() {
            Some(Block::Text { text }) => vec![
                Ok(ProviderEvent::TextDelta(text.clone())),
                Ok(ProviderEvent::Done {
                    message: msg,
                    usage,
                }),
            ],
            _ => vec![Ok(ProviderEvent::Done {
                message: msg,
                usage,
            })],
        };
        Ok(Box::pin(futures_util::stream::iter(events)))
    }
}

struct YesMan;

impl Approver for YesMan {
    fn decide(
        &mut self,
        _call: &halis_core::message::ToolCall,
        _preview: &str,
    ) -> Result<Decision> {
        Ok(Decision::Approve)
    }
}

struct NoMan;

impl Approver for NoMan {
    fn decide(
        &mut self,
        _call: &halis_core::message::ToolCall,
        _preview: &str,
    ) -> Result<Decision> {
        Ok(Decision::Deny)
    }
}

fn model() -> ResolvedModel {
    ResolvedModel {
        provider_id: "mock".into(),
        provider_name: "Mock".into(),
        model_id: "test-model".into(),
        display: "Test Model".into(),
        protocol: Protocol::OpenAi,
        base_url: "http://127.0.0.1:1".into(),
        api_key: Some("k".into()),
        env_keys: vec![],
        context: Some(100_000),
        max_out: Some(4096),
        price_in: 3.0,
        price_out: 15.0,
        supports_tools: true,
        reasoning: false,
        deprecated: false,
    }
}

fn agent_with(
    provider: Box<dyn Provider>,
) -> (
    Agent,
    tokio::sync::mpsc::UnboundedReceiver<halis_core::agent::AgentEvent>,
    tempfile::TempDir,
) {
    let tmp = tempfile::tempdir().unwrap();
    let session = Session::in_memory(tmp.path().to_path_buf(), "mock/test-model");
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let mut cfg = AgentConfig::defaults(model());
    cfg.approval = halis_core::config::ApprovalMode::Confirm;
    let agent = Agent::with_provider(cfg, provider, session, tx);
    (agent, rx, tmp)
}

#[tokio::test]
async fn runs_tool_then_finishes() {
    let tmp_path = std::env::temp_dir().join(format!("halis-it-{}", std::process::id()));
    let target = tmp_path.join("a.txt");
    let _ = std::fs::remove_file(&target);

    let call = Msg::assistant(vec![Block::ToolCall {
        id: "c1".into(),
        name: "write_file".into(),
        arguments: json!({ "path": target.to_string_lossy(), "content": "hello\n" }),
    }]);
    let final_msg = Msg::assistant_text("file created");

    let (mut agent, mut rx, _tmp) = agent_with(Box::new(Mock::new(vec![call, final_msg])));
    let cancel = CancellationToken::new();
    let stats = agent
        .run_turn("create the file", &cancel, &mut YesMan)
        .await
        .unwrap();

    assert_eq!(stats.tool_calls, 1);
    assert_eq!(stats.turns, 2);
    assert_eq!(stats.usage.input, 20);
    assert_eq!(stats.usage.output, 10);
    assert!((stats.cost - 20.0 * 3.0 / 1_000_000.0 - 10.0 * 15.0 / 1_000_000.0).abs() < 1e-9);
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "hello\n");
    let _ = std::fs::remove_file(&target);

    let messages = agent.session().messages();
    assert_eq!(messages.len(), 5);
    assert!(matches!(
        messages[0].role,
        halis_core::message::Role::System
    ));
    assert!(matches!(messages[1].role, halis_core::message::Role::User));
    assert!(matches!(
        messages[2].role,
        halis_core::message::Role::Assistant
    ));
    assert!(matches!(messages[3].role, halis_core::message::Role::User));
    assert!(matches!(
        messages[4].role,
        halis_core::message::Role::Assistant
    ));

    let mut saw_tool_call = false;
    let mut saw_tool_result = false;
    let mut saw_text = false;
    while let Ok(ev) = rx.try_recv() {
        match ev {
            halis_core::agent::AgentEvent::ToolCall { name, .. } => {
                assert_eq!(name, "write_file");
                saw_tool_call = true;
            }
            halis_core::agent::AgentEvent::ToolResult { ok, .. } => {
                assert!(ok);
                saw_tool_result = true;
            }
            halis_core::agent::AgentEvent::TextDelta(t) => {
                assert_eq!(t, "file created");
                saw_text = true;
            }
            _ => {}
        }
    }
    assert!(saw_tool_call && saw_tool_result && saw_text);
}

#[tokio::test]
async fn denied_tool_returns_error_result() {
    let target = std::env::temp_dir().join(format!("halis-deny-{}.txt", std::process::id()));
    let _ = std::fs::remove_file(&target);

    let call = Msg::assistant(vec![Block::ToolCall {
        id: "c1".into(),
        name: "write_file".into(),
        arguments: json!({ "path": target.to_string_lossy(), "content": "x" }),
    }]);
    let final_msg = Msg::assistant_text("ok, skipped");

    let (mut agent, _rx, _tmp) = agent_with(Box::new(Mock::new(vec![call, final_msg])));
    let cancel = CancellationToken::new();
    agent
        .run_turn("create the file", &cancel, &mut NoMan)
        .await
        .unwrap();

    assert!(!target.exists());
    let messages = agent.session().messages();
    let tool_results: Vec<&Block> = messages[3]
        .blocks
        .iter()
        .filter(|b| matches!(b, Block::ToolResult { .. }))
        .collect();
    assert_eq!(tool_results.len(), 1);
    if let Block::ToolResult { ok, content, .. } = tool_results[0] {
        assert!(!ok);
        assert!(content.contains("declined"));
    } else {
        panic!("expected tool result");
    }
    let _ = std::fs::remove_file(&target);
}

#[tokio::test]
async fn aborted_turn_preserves_partial_text() {
    struct HangingMock;
    #[async_trait::async_trait]
    impl Provider for HangingMock {
        fn name(&self) -> &str {
            "hanging"
        }
        async fn stream(
            &self,
            _req: &CompletionRequest,
        ) -> Result<BoxStream<Result<ProviderEvent>>> {
            let stream = async_stream::stream! {
                yield Ok(ProviderEvent::TextDelta("partial answer".into()));
                futures_util::future::pending::<()>().await;
            };
            Ok(Box::pin(stream))
        }
    }

    let (mut agent, _rx, _tmp) = agent_with(Box::new(HangingMock));
    let cancel = CancellationToken::new();
    let timed = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        timed.cancel();
    });
    let stats = agent.run_turn("hello", &cancel, &mut YesMan).await.unwrap();
    assert_eq!(stats.turns, 1);
    assert_eq!(stats.usage.input, 0);
    let messages = agent.session().messages();
    assert_eq!(messages.len(), 3);
    assert_eq!(messages[2].text(), "partial answer");
}
