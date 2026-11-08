pub mod anthropic;
pub mod google;
pub mod openai;

use crate::config::Effort;
use crate::error::{CoreError, Result};
use crate::message::{Msg, Usage};
use crate::resolver::{Protocol, ResolvedModel};
use crate::sse::SseEvent;
use futures_util::Stream;
use std::pin::Pin;
use std::time::Duration;

pub type BoxStream<T> = Pin<Box<dyn Stream<Item = T> + Send>>;

#[derive(Debug, Clone)]
pub struct ToolSchema {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

#[derive(Debug, Clone)]
pub struct CompletionRequest {
    pub model: ResolvedModel,
    pub messages: Vec<Msg>,
    pub tools: Vec<ToolSchema>,
    pub max_out: Option<u64>,
    pub effort: Option<Effort>,
    pub temperature: Option<f32>,
    pub tools_enabled: bool,
}

#[derive(Debug, Clone)]
pub enum ProviderEvent {
    TextDelta(String),
    ThinkingDelta(String),
    Done { message: Msg, usage: Usage },
}

#[async_trait::async_trait]
pub trait Provider: Send + Sync {
    fn name(&self) -> &str;
    async fn stream(&self, req: &CompletionRequest) -> Result<BoxStream<Result<ProviderEvent>>>;
}

pub fn build(model: &ResolvedModel) -> Result<Box<dyn Provider>> {
    let client = reqwest::Client::builder()
        .user_agent(concat!("halis/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(20))
        .build()
        .map_err(|e| CoreError::Provider(model.provider_id.clone(), e.to_string()))?;
    Ok(match model.protocol {
        Protocol::OpenAi => Box::new(openai::OpenAi::new(client, model.clone())),
        Protocol::Anthropic => Box::new(anthropic::Anthropic::new(client, model.clone())),
        Protocol::Google => Box::new(google::Google::new(client, model.clone())),
    })
}

const MAX_ATTEMPTS: u32 = 4;
const MAX_RETRY_AFTER: Duration = Duration::from_secs(30);

pub(crate) async fn post_sse(
    client: &reqwest::Client,
    url: &str,
    headers: &[(&str, &str)],
    body: &serde_json::Value,
    provider: &str,
) -> Result<reqwest::Response> {
    let mut attempt = 1u32;
    loop {
        let mut req = client.post(url).json(body);
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        match req.send().await {
            Ok(resp) if resp.status().is_success() => return Ok(resp),
            Ok(resp) => {
                let status = resp.status();
                let retry_after = resp
                    .headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(|v| v.to_str().ok())
                    .and_then(parse_retry_after);
                let text = resp.text().await.unwrap_or_default();
                if is_retryable(status.as_u16()) && attempt < MAX_ATTEMPTS {
                    let delay = retry_after.unwrap_or_else(|| backoff(attempt));
                    attempt += 1;
                    tracing::warn!(provider, status = %status, ?delay, "retrying request");
                    tokio::time::sleep(delay).await;
                    continue;
                }
                let detail = crate::util::truncate_chars(
                    text.lines().find(|l| !l.trim().is_empty()).unwrap_or(""),
                    300,
                );
                return Err(CoreError::Provider(
                    provider.to_string(),
                    format!("HTTP {status}: {detail}"),
                ));
            }
            Err(e) => {
                if (e.is_timeout() || e.is_connect()) && attempt < MAX_ATTEMPTS {
                    let delay = backoff(attempt);
                    attempt += 1;
                    tracing::warn!(provider, error = %e, ?delay, "retrying request");
                    tokio::time::sleep(delay).await;
                    continue;
                }
                return Err(CoreError::Provider(provider.to_string(), e.to_string()));
            }
        }
    }
}

fn is_retryable(status: u16) -> bool {
    matches!(status, 408 | 429 | 500 | 502 | 503 | 504 | 529)
}

fn parse_retry_after(value: &str) -> Option<Duration> {
    let secs: f64 = value.trim().parse().ok()?;
    if !secs.is_finite() || secs < 0.0 {
        return None;
    }
    Some(Duration::from_secs_f64(secs).min(MAX_RETRY_AFTER))
}

fn backoff(attempt: u32) -> Duration {
    let base_ms = 500u64 << (attempt - 1).min(4);
    let jitter = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() % 200)
        .unwrap_or(100) as i64
        - 100;
    let ms = base_ms as i64 + jitter * base_ms as i64 / 1000;
    Duration::from_millis(ms.max(50) as u64)
}

pub(crate) fn sse_events(resp: reqwest::Response) -> BoxStream<Result<SseEvent>> {
    use futures_util::StreamExt;
    let stream = async_stream::stream! {
        let mut decoder = crate::sse::SseDecoder::default();
        let mut body = resp.bytes_stream();
        while let Some(chunk) = body.next().await {
            match chunk {
                Ok(bytes) => {
                    for ev in decoder.push(&bytes) {
                        yield Ok(ev);
                    }
                }
                Err(e) => {
                    yield Err(CoreError::Stream(e.to_string()));
                    break;
                }
            }
        }
    };
    Box::pin(stream)
}

pub(crate) fn require_key(model: &ResolvedModel) -> Result<String> {
    crate::resolver::require_api_key(model).map(|k| k.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_after_parsing() {
        assert_eq!(parse_retry_after("2"), Some(Duration::from_secs(2)));
        assert_eq!(parse_retry_after("120"), Some(MAX_RETRY_AFTER));
        assert_eq!(parse_retry_after("junk"), None);
    }

    #[test]
    fn retry_classification() {
        for s in [408, 429, 500, 502, 503, 504, 529] {
            assert!(is_retryable(s));
        }
        for s in [400, 401, 403, 404, 422] {
            assert!(!is_retryable(s));
        }
    }
}
