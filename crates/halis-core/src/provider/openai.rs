use super::{
    BoxStream, CompletionRequest, Provider, ProviderEvent, post_sse, require_key, sse_events,
};
use crate::error::{CoreError, Result};
use crate::message::{Block, Msg, Role, Usage};
use crate::resolver::ResolvedModel;
use crate::sse::SseEvent;
use futures_util::StreamExt;

pub struct OpenAi {
    client: reqwest::Client,
    model: ResolvedModel,
}

impl OpenAi {
    pub fn new(client: reqwest::Client, model: ResolvedModel) -> Self {
        Self { client, model }
    }
}

#[async_trait::async_trait]
impl Provider for OpenAi {
    fn name(&self) -> &str {
        "openai"
    }

    async fn stream(&self, req: &CompletionRequest) -> Result<BoxStream<Result<ProviderEvent>>> {
        let key = require_key(&self.model)?;
        let body = self.build_body(req)?;
        let url = format!("{}/chat/completions", self.model.base_url);
        let mut headers: Vec<(&str, String)> = vec![
            ("authorization", format!("Bearer {key}")),
            ("accept", "text/event-stream".into()),
        ];
        if self.model.base_url.contains("openrouter") {
            headers.push((
                "http-referer",
                "https://github.com/HalisAI/halis-agent".into(),
            ));
            headers.push(("x-title", "Halis".into()));
        }
        let header_refs: Vec<(&str, &str)> =
            headers.iter().map(|(k, v)| (*k, v.as_str())).collect();

        let resp = post_sse(
            &self.client,
            &url,
            &header_refs,
            &body,
            &self.model.provider_id,
        )
        .await?;
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

impl OpenAi {
    fn build_body(&self, req: &CompletionRequest) -> Result<serde_json::Value> {
        use serde_json::{Value, json};

        let mut messages: Vec<Value> = Vec::with_capacity(req.messages.len());
        for msg in &req.messages {
            match msg.role {
                Role::System => messages.push(json!({
                    "role": "system",
                    "content": msg.text(),
                })),
                Role::User => {
                    let text = msg.text();
                    let tool_results: Vec<&Block> = msg
                        .blocks
                        .iter()
                        .filter(|b| matches!(b, Block::ToolResult { .. }))
                        .collect();
                    if !text.is_empty() || tool_results.is_empty() {
                        messages.push(json!({ "role": "user", "content": text }));
                    }
                    for b in tool_results {
                        if let Block::ToolResult { id, content, .. } = b {
                            messages.push(json!({
                                "role": "tool",
                                "tool_call_id": id,
                                "content": content,
                            }));
                        }
                    }
                }
                Role::Assistant => {
                    let text = msg.text();
                    let calls: Vec<Value> = msg
                        .blocks
                        .iter()
                        .filter_map(|b| match b {
                            Block::ToolCall {
                                id,
                                name,
                                arguments,
                            } => Some(json!({
                                "id": id,
                                "type": "function",
                                "function": {
                                    "name": name,
                                    "arguments": arguments.to_string(),
                                },
                            })),
                            _ => None,
                        })
                        .collect();
                    let mut m = json!({ "role": "assistant" });
                    m["content"] = if text.is_empty() {
                        Value::Null
                    } else {
                        json!(text)
                    };
                    if !calls.is_empty() {
                        m["tool_calls"] = json!(calls);
                    }
                    messages.push(m);
                }
            }
        }

        let mut body = json!({
            "model": self.model.model_id,
            "messages": messages,
            "stream": true,
            "stream_options": { "include_usage": true },
        });
        if req.tools_enabled && !req.tools.is_empty() && self.model.supports_tools {
            body["tools"] = json!(
                req.tools
                    .iter()
                    .map(|t| json!({
                        "type": "function",
                        "function": {
                            "name": t.name,
                            "description": t.description,
                            "parameters": t.parameters,
                        },
                    }))
                    .collect::<Vec<_>>()
            );
            body["tool_choice"] = json!("auto");
        }
        if let Some(effort) = req.effort
            && self.model.reasoning
        {
            body["reasoning_effort"] = json!(effort.as_str());
        }
        if let Some(temp) = req.temperature
            && !self.model.reasoning
        {
            body["temperature"] = json!(temp);
        }
        Ok(body)
    }
}

#[derive(Default)]
struct PartialCall {
    id: Option<String>,
    name: Option<String>,
    args: String,
}

#[derive(Default)]
struct Acc {
    text: String,
    thinking: String,
    calls: std::collections::BTreeMap<u64, PartialCall>,
    usage: Option<Usage>,
    emitted: bool,
}

impl Acc {
    fn push(&mut self, ev: &SseEvent, provider: &str) -> Result<Option<ProviderEvent>> {
        if self.emitted {
            return Ok(None);
        }
        if ev.data.trim() == "[DONE]" {
            return Ok(None);
        }
        let v: serde_json::Value = serde_json::from_str(&ev.data)
            .map_err(|e| CoreError::Stream(format!("malformed chunk from {provider}: {e}")))?;
        if let Some(err) = v.get("error") {
            let msg = err
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("unknown error");
            return Err(CoreError::Provider(provider.to_string(), msg.to_string()));
        }
        if let Some(u) = v.get("usage").filter(|u| !u.is_null()) {
            self.usage = Some(parse_usage(u));
        }
        let Some(delta) = v
            .get("choices")
            .and_then(|c| c.get(0))
            .and_then(|c| c.get("delta"))
        else {
            return Ok(None);
        };
        if let Some(t) = delta.get("content").and_then(|c| c.as_str())
            && !t.is_empty()
        {
            self.text.push_str(t);
            return Ok(Some(ProviderEvent::TextDelta(t.to_string())));
        }
        for key in ["reasoning_content", "reasoning"] {
            if let Some(t) = delta.get(key).and_then(|c| c.as_str())
                && !t.is_empty()
            {
                self.thinking.push_str(t);
                return Ok(Some(ProviderEvent::ThinkingDelta(t.to_string())));
            }
        }
        if let Some(tcs) = delta.get("tool_calls").and_then(|t| t.as_array()) {
            for tc in tcs {
                let idx = tc.get("index").and_then(|i| i.as_u64()).unwrap_or(0);
                let entry = self.calls.entry(idx).or_default();
                if let Some(id) = tc.get("id").and_then(|i| i.as_str()) {
                    entry.id = Some(id.to_string());
                }
                if let Some(f) = tc.get("function") {
                    if let Some(n) = f.get("name").and_then(|n| n.as_str()) {
                        entry.name = Some(n.to_string());
                    }
                    if let Some(a) = f.get("arguments").and_then(|a| a.as_str()) {
                        entry.args.push_str(a);
                    }
                }
            }
        }
        Ok(None)
    }

    fn finish(&mut self) -> Result<Option<ProviderEvent>> {
        if self.emitted {
            return Ok(None);
        }
        self.emitted = true;
        let mut blocks = Vec::new();
        if !self.thinking.is_empty() {
            blocks.push(Block::Thinking {
                text: std::mem::take(&mut self.thinking),
                signature: String::new(),
            });
        }
        if !self.text.is_empty() {
            blocks.push(Block::Text {
                text: std::mem::take(&mut self.text),
            });
        }
        for (_, c) in std::mem::take(&mut self.calls) {
            let arguments =
                serde_json::from_str(&c.args).unwrap_or(serde_json::Value::String(c.args.clone()));
            blocks.push(Block::ToolCall {
                id: c.id.unwrap_or_default(),
                name: c.name.unwrap_or_default(),
                arguments,
            });
        }
        let usage = self.usage.unwrap_or_default();
        Ok(Some(ProviderEvent::Done {
            message: Msg::assistant(blocks),
            usage,
        }))
    }
}

fn parse_usage(u: &serde_json::Value) -> Usage {
    Usage {
        input: u.get("prompt_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
        output: u
            .get("completion_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
        cache_read: u
            .get("prompt_tokens_details")
            .and_then(|d| d.get("cached_tokens"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
        cache_write: 0,
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

    #[test]
    fn assembles_text_and_usage() {
        let mut acc = Acc::default();
        let payload = concat!(
            r#"data: {"choices":[{"delta":{"role":"assistant","content":"Hel"}}]}"#,
            "\n\n",
            r#"data: {"choices":[{"delta":{"content":"lo"}}]}"#,
            "\n\n",
            r#"data: {"choices":[],"usage":{"prompt_tokens":11,"completion_tokens":2}}"#,
            "\n\n",
            "data: [DONE]\n\n",
        );
        for ev in events(payload) {
            if let Some(ProviderEvent::TextDelta(t)) = acc.push(&ev, "openai").unwrap() {
                assert!(t == "Hel" || t == "lo")
            }
        }
        let Some(ProviderEvent::Done { message, usage }) = acc.finish().unwrap() else {
            panic!("expected Done");
        };
        assert_eq!(message.text(), "Hello");
        assert_eq!(usage.input, 11);
        assert_eq!(usage.output, 2);
        assert!(acc.finish().unwrap().is_none());
    }

    #[test]
    fn accumulates_tool_call_fragments() {
        let mut acc = Acc::default();
        let payload = concat!(
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"bash","arguments":""}}]}}]}"#,
            "\n\n",
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"command\":"}}]}}]}"#,
            "\n\n",
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"ls\"}"}}]}}]}"#,
            "\n\n",
            r#"data: {"choices":[{"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":40,"completion_tokens":9}}"#,
            "\n\n",
            "data: [DONE]\n\n",
        );
        for ev in events(payload) {
            acc.push(&ev, "openai").unwrap();
        }
        let Some(ProviderEvent::Done { message, usage }) = acc.finish().unwrap() else {
            panic!("expected Done");
        };
        let calls = message.tool_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "call_1");
        assert_eq!(calls[0].name, "bash");
        assert_eq!(calls[0].arguments["command"], "ls");
        assert_eq!(usage.input, 40);
        assert_eq!(usage.output, 9);
    }

    #[test]
    fn captures_reasoning_content() {
        let mut acc = Acc::default();
        let payload = concat!(
            r#"data: {"choices":[{"delta":{"reasoning_content":"pondering"}}]}"#,
            "\n\n",
            "data: [DONE]\n\n",
        );
        for ev in events(payload) {
            if let Some(ProviderEvent::ThinkingDelta(t)) = acc.push(&ev, "openai").unwrap() {
                assert_eq!(t, "pondering");
            }
        }
        let Some(ProviderEvent::Done { message, .. }) = acc.finish().unwrap() else {
            panic!("expected Done");
        };
        assert!(message.blocks[0].as_text().is_none());
        assert!(matches!(message.blocks[0], Block::Thinking { .. }));
    }

    #[test]
    fn surfaces_stream_errors() {
        let mut acc = Acc::default();
        let payload = concat!(r#"data: {"error":{"message":"bad key"}}"#, "\n\n",);
        for ev in events(payload) {
            let err = acc.push(&ev, "openai").unwrap_err();
            assert!(err.to_string().contains("bad key"));
        }
    }
}
