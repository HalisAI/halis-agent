use super::{
    BoxStream, CompletionRequest, Provider, ProviderEvent, post_sse, require_key, sse_events,
};
use crate::config::Effort;
use crate::error::{CoreError, Result};
use crate::message::{Block, Msg, Role, Usage};
use crate::resolver::ResolvedModel;
use crate::sse::SseEvent;
use futures_util::StreamExt;

pub struct Google {
    client: reqwest::Client,
    model: ResolvedModel,
}

impl Google {
    pub fn new(client: reqwest::Client, model: ResolvedModel) -> Self {
        Self { client, model }
    }
}

#[async_trait::async_trait]
impl Provider for Google {
    fn name(&self) -> &str {
        "google"
    }

    async fn stream(&self, req: &CompletionRequest) -> Result<BoxStream<Result<ProviderEvent>>> {
        let key = require_key(&self.model)?;
        let body = self.build_body(req)?;
        let model = encode_path_segment(&self.model.model_id);
        let url = format!(
            "{}/models/{model}:streamGenerateContent?alt=sse",
            self.model.base_url
        );
        let headers: Vec<(&str, &str)> = vec![("x-goog-api-key", key.as_str())];

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

impl Google {
    fn build_body(&self, req: &CompletionRequest) -> Result<serde_json::Value> {
        use serde_json::{Value, json};

        let mut system = String::new();
        let mut contents: Vec<Value> = Vec::with_capacity(req.messages.len());
        for msg in &req.messages {
            match msg.role {
                Role::System => system.push_str(&msg.text()),
                Role::User => {
                    let parts: Vec<Value> = msg
                        .blocks
                        .iter()
                        .filter_map(|b| match b {
                            Block::Text { text } => Some(json!({ "text": text })),
                            Block::ToolResult {
                                name, ok, content, ..
                            } => {
                                let key = if *ok { "output" } else { "error" };
                                Some(json!({
                                    "functionResponse": {
                                        "name": name,
                                        "response": { key: content },
                                    }
                                }))
                            }
                            _ => None,
                        })
                        .collect();
                    if parts.is_empty() {
                        continue;
                    }
                    if let Some(Value::Object(last)) = contents.last_mut()
                        && last.get("role").and_then(|r| r.as_str()) == Some("user")
                        && let Some(arr) = last.get_mut("parts").and_then(|p| p.as_array_mut())
                    {
                        arr.extend(parts);
                        continue;
                    }
                    contents.push(json!({ "role": "user", "parts": parts }));
                }
                Role::Assistant => {
                    let parts: Vec<Value> = msg
                        .blocks
                        .iter()
                        .filter_map(|b| match b {
                            Block::Text { text } => Some(json!({ "text": text })),
                            Block::ToolCall {
                                name, arguments, ..
                            } => {
                                Some(json!({ "functionCall": { "name": name, "args": arguments } }))
                            }
                            _ => None,
                        })
                        .collect();
                    if parts.is_empty() {
                        continue;
                    }
                    if let Some(Value::Object(last)) = contents.last_mut()
                        && last.get("role").and_then(|r| r.as_str()) == Some("model")
                        && let Some(arr) = last.get_mut("parts").and_then(|p| p.as_array_mut())
                    {
                        arr.extend(parts);
                        continue;
                    }
                    contents.push(json!({ "role": "model", "parts": parts }));
                }
            }
        }

        let mut body = json!({ "contents": contents });
        if !system.is_empty() {
            body["systemInstruction"] = json!({ "parts": [{ "text": system }] });
        }
        if req.tools_enabled && !req.tools.is_empty() && self.model.supports_tools {
            body["tools"] = json!([{
                "functionDeclarations": req
                    .tools
                    .iter()
                    .map(|t| json!({
                        "name": t.name,
                        "description": t.description,
                        "parameters": t.parameters,
                    }))
                    .collect::<Vec<_>>(),
            }]);
        }
        let mut generation = serde_json::Map::new();
        if let Some(max_out) = req.max_out {
            generation.insert("maxOutputTokens".into(), json!(max_out));
        }
        if let Some(temp) = req.temperature
            && !self.model.reasoning
        {
            generation.insert("temperature".into(), json!(temp));
        }
        if let Some(effort) = req.effort
            && self.model.reasoning
        {
            let budget = match effort {
                Effort::Low => 2048,
                Effort::Medium => 8192,
                Effort::High => 24576,
            };
            generation.insert("thinkingConfig".into(), json!({ "thinkingBudget": budget }));
        }
        if !generation.is_empty() {
            body["generationConfig"] = Value::Object(generation);
        }
        Ok(body)
    }
}

fn encode_path_segment(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[derive(Default)]
struct Acc {
    text: String,
    thinking: String,
    calls: Vec<(String, serde_json::Value)>,
    usage: Option<Usage>,
    finished: bool,
}

impl Acc {
    fn push(&mut self, ev: &SseEvent, provider: &str) -> Result<Option<ProviderEvent>> {
        if self.finished {
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
        if let Some(u) = v.get("usageMetadata") {
            let input = u
                .get("promptTokenCount")
                .and_then(|x| x.as_u64())
                .unwrap_or(0);
            let output = u
                .get("candidatesTokenCount")
                .and_then(|x| x.as_u64())
                .unwrap_or(0);
            let thoughts = u
                .get("thoughtsTokenCount")
                .and_then(|x| x.as_u64())
                .unwrap_or(0);
            let cached = u
                .get("cachedContentTokenCount")
                .and_then(|x| x.as_u64())
                .unwrap_or(0);
            self.usage = Some(Usage {
                input,
                output: output + thoughts,
                cache_read: cached,
                cache_write: 0,
            });
        }
        let parts = v
            .pointer("/candidates/0/content/parts")
            .and_then(|p| p.as_array())
            .cloned()
            .unwrap_or_default();
        let mut text_buf = String::new();
        let mut thought_buf = String::new();
        for part in parts {
            if let Some(fc) = part.get("functionCall") {
                let name = fc
                    .get("name")
                    .and_then(|n| n.as_str())
                    .unwrap_or("")
                    .to_string();
                let args = fc.get("args").cloned().unwrap_or(serde_json::Value::Null);
                self.calls.push((name, args));
                continue;
            }
            let is_thought = part
                .get("thought")
                .and_then(|t| t.as_bool())
                .unwrap_or(false);
            let Some(text) = part.get("text").and_then(|t| t.as_str()) else {
                continue;
            };
            if is_thought {
                thought_buf.push_str(text);
            } else {
                text_buf.push_str(text);
            }
        }
        if !text_buf.is_empty() {
            self.text.push_str(&text_buf);
            return Ok(Some(ProviderEvent::TextDelta(text_buf)));
        }
        if !thought_buf.is_empty() {
            self.thinking.push_str(&thought_buf);
            return Ok(Some(ProviderEvent::ThinkingDelta(thought_buf)));
        }
        Ok(None)
    }

    fn finish(&mut self) -> Result<Option<ProviderEvent>> {
        if self.finished {
            return Ok(None);
        }
        self.finished = true;
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
        for (n, (name, args)) in std::mem::take(&mut self.calls).into_iter().enumerate() {
            blocks.push(Block::ToolCall {
                id: format!("call_{n}"),
                name,
                arguments: args,
            });
        }
        let usage = self.usage.unwrap_or_default();
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

    #[test]
    fn assembles_parts_and_usage() {
        let payload = concat!(
            r#"data: {"candidates":[{"content":{"role":"model","parts":[{"text":"Hel"}]}}]}"#,
            "\n\n",
            r#"data: {"candidates":[{"content":{"role":"model","parts":[{"text":"lo"},{"functionCall":{"name":"bash","args":{"command":"ls"}}}]}}],"usageMetadata":{"promptTokenCount":30,"candidatesTokenCount":5,"thoughtsTokenCount":2}}"#,
            "\n\n",
        );
        let mut acc = Acc::default();
        let mut text = String::new();
        for ev in events(payload) {
            if let Some(ProviderEvent::TextDelta(t)) = acc.push(&ev, "google").unwrap() {
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
        assert_eq!(calls[0].name, "bash");
        assert_eq!(calls[0].arguments["command"], "ls");
        assert_eq!(usage.input, 30);
        assert_eq!(usage.output, 7);
    }

    #[test]
    fn skips_thought_parts() {
        let payload = concat!(
            r#"data: {"candidates":[{"content":{"parts":[{"text":"internal","thought":true},{"text":"answer"}]}}]}"#,
            "\n\n",
        );
        let mut acc = Acc::default();
        for ev in events(payload) {
            acc.push(&ev, "google").unwrap();
        }
        let Some(ProviderEvent::Done { message, .. }) = acc.finish().unwrap() else {
            panic!("expected Done");
        };
        assert_eq!(message.text(), "answer");
    }

    #[test]
    fn path_encoding() {
        assert_eq!(encode_path_segment("gemini-2.5-pro"), "gemini-2.5-pro");
        assert_eq!(encode_path_segment("a/b c"), "a%2Fb%20c");
    }
}
