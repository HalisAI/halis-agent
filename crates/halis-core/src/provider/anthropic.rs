use super::{
    BoxStream, CompletionRequest, Provider, ProviderEvent, post_sse, require_key, sse_events,
};
use crate::config::Effort;
use crate::error::{CoreError, Result};
use crate::message::{Block, Msg, Role, Usage};
use crate::resolver::ResolvedModel;
use crate::sse::SseEvent;
use futures_util::StreamExt;

const ANTHROPIC_VERSION: &str = "2023-06-01";

pub struct Anthropic {
    client: reqwest::Client,
    model: ResolvedModel,
}

impl Anthropic {
    pub fn new(client: reqwest::Client, model: ResolvedModel) -> Self {
        Self { client, model }
    }
}

#[async_trait::async_trait]
impl Provider for Anthropic {
    fn name(&self) -> &str {
        "anthropic"
    }

    async fn stream(&self, req: &CompletionRequest) -> Result<BoxStream<Result<ProviderEvent>>> {
        let key = require_key(&self.model)?;
        let body = self.build_body(req)?;
        let url = format!("{}/messages", self.model.base_url);
        let headers: Vec<(&str, &str)> = vec![
            ("x-api-key", key.as_str()),
            ("anthropic-version", ANTHROPIC_VERSION),
            ("accept", "text/event-stream"),
        ];

        let resp = post_sse(&self.client, &url, &headers, &body, &self.model.provider_id).await?;
        let provider = self.model.provider_id.clone();
        let mut acc = Acc::default();
        let inner = sse_events(resp);
        let out = async_stream::stream! {
            let mut inner = inner;
            while let Some(item) = inner.next().await {
                match item {
                    Ok(ev) => match acc.push(&ev, &provider) {
                        Ok(Some(pe)) => yield Ok(pe),
                        Ok(None) => {}
                        Err(e) => {
                            yield Err(e);
                            return;
                        }
                    },
                    Err(e) => {
                        yield Err(e);
                        return;
                    }
                }
            }
            match acc.finish() {
                Ok(Some(pe)) => yield Ok(pe),
                Ok(None) => {}
                Err(e) => yield Err(e),
            }
        };
        Ok(Box::pin(out))
    }
}

impl Anthropic {
    fn build_body(&self, req: &CompletionRequest) -> Result<serde_json::Value> {
        use serde_json::{Value, json};

        let mut system = String::new();
        let mut messages: Vec<Value> = Vec::with_capacity(req.messages.len());
        for msg in &req.messages {
            match msg.role {
                Role::System => system.push_str(&msg.text()),
                Role::User => {
                    let blocks: Vec<Value> = msg
                        .blocks
                        .iter()
                        .filter_map(|b| match b {
                            Block::Text { text } => Some(json!({ "type": "text", "text": text })),
                            Block::ToolResult {
                                id, ok, content, ..
                            } => Some(json!({
                                "type": "tool_result",
                                "tool_use_id": id,
                                "content": content,
                                "is_error": !ok,
                            })),
                            _ => None,
                        })
                        .collect();
                    if blocks.is_empty() {
                        continue;
                    }
                    if let Some(Value::Object(last)) = messages.last_mut()
                        && last.get("role").and_then(|r| r.as_str()) == Some("user")
                        && let Some(arr) = last.get_mut("content").and_then(|c| c.as_array_mut())
                    {
                        arr.extend(blocks);
                        continue;
                    }
                    messages.push(json!({ "role": "user", "content": blocks }));
                }
                Role::Assistant => {
                    let blocks: Vec<Value> = msg
                        .blocks
                        .iter()
                        .filter_map(|b| match b {
                            Block::Text { text } => Some(json!({ "type": "text", "text": text })),
                            Block::Thinking { text, signature } => {
                                let sig = if signature.is_empty() {
                                    serde_json::Value::Null
                                } else {
                                    json!(signature)
                                };
                                let mut b = json!({ "type": "thinking", "thinking": text });
                                b["signature"] = sig;
                                Some(b)
                            }
                            Block::ToolCall {
                                id,
                                name,
                                arguments,
                            } => Some(json!({
                                "type": "tool_use",
                                "id": id,
                                "name": name,
                                "input": arguments,
                            })),
                            _ => None,
                        })
                        .collect();
                    if blocks.is_empty() {
                        continue;
                    }
                    if let Some(Value::Object(last)) = messages.last_mut()
                        && last.get("role").and_then(|r| r.as_str()) == Some("assistant")
                        && let Some(arr) = last.get_mut("content").and_then(|c| c.as_array_mut())
                    {
                        arr.extend(blocks);
                        continue;
                    }
                    messages.push(json!({ "role": "assistant", "content": blocks }));
                }
            }
        }

        let mut max_tokens = req.max_out.unwrap_or(8192).clamp(1024, 128_000);
        let mut body = json!({
            "model": self.model.model_id,
            "max_tokens": max_tokens,
            "stream": true,
            "messages": messages,
        });
        if !system.is_empty() {
            body["system"] = json!(system);
        }
        if req.tools_enabled && !req.tools.is_empty() && self.model.supports_tools {
            body["tools"] = json!(
                req.tools
                    .iter()
                    .map(|t| json!({
                        "name": t.name,
                        "description": t.description,
                        "input_schema": t.parameters,
                    }))
                    .collect::<Vec<_>>()
            );
        }
        if let Some(effort) = req.effort
            && self.model.reasoning
        {
            let mut budget = match effort {
                Effort::Low => 2048,
                Effort::Medium => 8192,
                Effort::High => 16384,
            };
            if max_tokens <= budget + 1024 {
                max_tokens = (budget + 4096).min(128_000);
                if max_tokens <= budget + 1024 {
                    budget = 1024;
                    max_tokens = 4096;
                }
            }
            body["max_tokens"] = json!(max_tokens);
            body["thinking"] = json!({ "type": "enabled", "budget_tokens": budget });
        }
        Ok(body)
    }
}

enum BlockAcc {
    Text(String),
    Thinking {
        text: String,
        signature: String,
    },
    Tool {
        id: String,
        name: String,
        json: String,
    },
}

#[derive(Default)]
struct Acc {
    blocks: std::collections::BTreeMap<usize, BlockAcc>,
    usage: Usage,
    stop_reason: Option<String>,
    finished: bool,
}

impl Acc {
    fn push(&mut self, ev: &SseEvent, provider: &str) -> Result<Option<ProviderEvent>> {
        if self.finished {
            return Ok(None);
        }
        let v: serde_json::Value = serde_json::from_str(&ev.data)
            .map_err(|e| CoreError::Stream(format!("malformed chunk from {provider}: {e}")))?;
        let kind = ev.event.as_deref().unwrap_or("");
        match kind {
            "message_start" => {
                if let Some(u) = v.pointer("/message/usage") {
                    self.usage.input = u.get("input_tokens").and_then(|x| x.as_u64()).unwrap_or(0);
                    self.usage.cache_read = u
                        .get("cache_read_input_tokens")
                        .and_then(|x| x.as_u64())
                        .unwrap_or(0);
                    self.usage.cache_write = u
                        .get("cache_creation_input_tokens")
                        .and_then(|x| x.as_u64())
                        .unwrap_or(0);
                }
            }
            "content_block_start" => {
                let idx = v.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as usize;
                let cb = v
                    .get("content_block")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);
                match cb.get("type").and_then(|t| t.as_str()).unwrap_or("") {
                    "text" => {
                        self.blocks.insert(idx, BlockAcc::Text(String::new()));
                    }
                    "thinking" => {
                        self.blocks.insert(
                            idx,
                            BlockAcc::Thinking {
                                text: String::new(),
                                signature: String::new(),
                            },
                        );
                    }
                    "tool_use" => {
                        self.blocks.insert(
                            idx,
                            BlockAcc::Tool {
                                id: cb
                                    .get("id")
                                    .and_then(|i| i.as_str())
                                    .unwrap_or("")
                                    .to_string(),
                                name: cb
                                    .get("name")
                                    .and_then(|i| i.as_str())
                                    .unwrap_or("")
                                    .to_string(),
                                json: String::new(),
                            },
                        );
                    }
                    _ => {}
                }
            }
            "content_block_delta" => {
                let idx = v.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as usize;
                let delta = v.get("delta").cloned().unwrap_or(serde_json::Value::Null);
                match delta.get("type").and_then(|t| t.as_str()).unwrap_or("") {
                    "text_delta" => {
                        let t = delta.get("text").and_then(|x| x.as_str()).unwrap_or("");
                        if let Some(BlockAcc::Text(out)) = self.blocks.get_mut(&idx) {
                            out.push_str(t);
                            return Ok(Some(ProviderEvent::TextDelta(t.to_string())));
                        }
                    }
                    "thinking_delta" => {
                        let t = delta.get("thinking").and_then(|x| x.as_str()).unwrap_or("");
                        if let Some(BlockAcc::Thinking { text, .. }) = self.blocks.get_mut(&idx) {
                            text.push_str(t);
                            return Ok(Some(ProviderEvent::ThinkingDelta(t.to_string())));
                        }
                    }
                    "signature_delta" => {
                        let s = delta
                            .get("signature")
                            .and_then(|x| x.as_str())
                            .unwrap_or("");
                        if let Some(BlockAcc::Thinking { signature, .. }) =
                            self.blocks.get_mut(&idx)
                        {
                            signature.push_str(s);
                        }
                    }
                    "input_json_delta" => {
                        let j = delta
                            .get("partial_json")
                            .and_then(|x| x.as_str())
                            .unwrap_or("");
                        if let Some(BlockAcc::Tool { json, .. }) = self.blocks.get_mut(&idx) {
                            json.push_str(j);
                        }
                    }
                    _ => {}
                }
            }
            "message_delta" => {
                if let Some(u) = v.pointer("/usage")
                    && let Some(out) = u.get("output_tokens").and_then(|x| x.as_u64())
                {
                    self.usage.output = out;
                }
                if let Some(stop) = v.pointer("/delta/stop_reason").and_then(|s| s.as_str()) {
                    self.stop_reason = Some(stop.to_string());
                }
            }
            "message_stop" => {
                self.finished = true;
            }
            "error" => {
                let msg = v
                    .pointer("/error/message")
                    .and_then(|m| m.as_str())
                    .unwrap_or("unknown error");
                return Err(CoreError::Provider(provider.to_string(), msg.to_string()));
            }
            _ => {}
        }
        Ok(None)
    }

    fn finish(&mut self) -> Result<Option<ProviderEvent>> {
        if self.finished && self.blocks.is_empty() {
            return Ok(None);
        }
        self.finished = true;
        let mut blocks = Vec::new();
        for (_, b) in std::mem::take(&mut self.blocks) {
            match b {
                BlockAcc::Text(text) => blocks.push(Block::Text { text }),
                BlockAcc::Thinking { text, signature } => {
                    blocks.push(Block::Thinking { text, signature })
                }
                BlockAcc::Tool { id, name, json } => {
                    let arguments = serde_json::from_str(&json)
                        .unwrap_or(serde_json::Value::String(json.clone()));
                    blocks.push(Block::ToolCall {
                        id,
                        name,
                        arguments,
                    });
                }
            }
        }
        let usage = self.usage;
        Ok(Some(ProviderEvent::Done {
            message: Msg::assistant(blocks),
            usage,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sse::SseDecoder;

    fn events(payload: &str) -> Vec<SseEvent> {
        let mut dec = SseDecoder::default();
        dec.push(payload.as_bytes())
    }

    const STREAM: &str = concat!(
        r#"event: message_start"#,
        "\n",
        r#"data: {"type":"message_start","message":{"usage":{"input_tokens":25}}}"#,
        "\n\n",
        r#"event: content_block_start"#,
        "\n",
        r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text"}}"#,
        "\n\n",
        r#"event: content_block_delta"#,
        "\n",
        r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hel"}}"#,
        "\n\n",
        r#"event: content_block_delta"#,
        "\n",
        r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"lo"}}"#,
        "\n\n",
        r#"event: content_block_stop"#,
        "\n",
        r#"data: {"type":"content_block_stop","index":0}"#,
        "\n\n",
        r#"event: content_block_start"#,
        "\n",
        r#"data: {"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_1","name":"bash"}}"#,
        "\n\n",
        r#"event: content_block_delta"#,
        "\n",
        r#"data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"command\":"}}"#,
        "\n\n",
        r#"event: content_block_delta"#,
        "\n",
        r#"data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"\"ls\"}"}}"#,
        "\n\n",
        r#"event: content_block_stop"#,
        "\n",
        r#"data: {"type":"content_block_stop","index":1}"#,
        "\n\n",
        r#"event: message_delta"#,
        "\n",
        r#"data: {"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":7}}"#,
        "\n\n",
        r#"event: message_stop"#,
        "\n",
        r#"data: {"type":"message_stop"}"#,
        "\n\n",
    );

    #[test]
    fn assembles_blocks_and_usage() {
        let mut acc = Acc::default();
        let mut text = String::new();
        for ev in events(STREAM) {
            if let Some(ProviderEvent::TextDelta(t)) = acc.push(&ev, "anthropic").unwrap() {
                text.push_str(&t);
            }
        }
        assert_eq!(text, "Hello");
        let Some(ProviderEvent::Done { message, usage }) = acc.finish().unwrap() else {
            panic!("expected Done");
        };
        assert_eq!(message.text(), "Hello");
        let calls = message.tool_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "toolu_1");
        assert_eq!(calls[0].name, "bash");
        assert_eq!(calls[0].arguments["command"], "ls");
        assert_eq!(usage.input, 25);
        assert_eq!(usage.output, 7);
    }

    #[test]
    fn captures_thinking_deltas() {
        let payload = concat!(
            r#"event: content_block_start"#,
            "\n",
            r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"thinking"}}"#,
            "\n\n",
            r#"event: content_block_delta"#,
            "\n",
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"hmm"}}"#,
            "\n\n",
            r#"event: content_block_delta"#,
            "\n",
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig1"}}"#,
            "\n\n",
            r#"event: message_stop"#,
            "\n",
            r#"data: {"type":"message_stop"}"#,
            "\n\n",
        );
        let mut acc = Acc::default();
        for ev in events(payload) {
            acc.push(&ev, "anthropic").unwrap();
        }
        let Some(ProviderEvent::Done { message, .. }) = acc.finish().unwrap() else {
            panic!("expected Done");
        };
        assert!(matches!(message.blocks[0], Block::Thinking { .. }));
    }

    #[test]
    fn surfaces_error_events() {
        let payload = concat!(
            r#"event: error"#,
            "\n",
            r#"data: {"type":"error","error":{"type":"overloaded_error","message":"overloaded"}}"#,
            "\n\n",
        );
        let mut acc = Acc::default();
        for ev in events(payload) {
            let err = acc.push(&ev, "anthropic").unwrap_err();
            assert!(err.to_string().contains("overloaded"));
        }
    }
}
