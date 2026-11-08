use crate::catalog::{self, ModelEntry};
use crate::config::{Config, ProviderConfig};
use crate::error::{CoreError, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    OpenAi,
    Anthropic,
    Google,
}

impl Protocol {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "openai" => Some(Protocol::OpenAi),
            "anthropic" => Some(Protocol::Anthropic),
            "google" => Some(Protocol::Google),
            _ => None,
        }
    }

    pub fn default_base(&self) -> &'static str {
        match self {
            Protocol::OpenAi => "https://api.openai.com/v1",
            Protocol::Anthropic => "https://api.anthropic.com/v1",
            Protocol::Google => "https://generativelanguage.googleapis.com/v1beta",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ResolvedModel {
    pub provider_id: String,
    pub provider_name: String,
    pub model_id: String,
    pub display: String,
    pub protocol: Protocol,
    pub base_url: String,
    pub api_key: Option<String>,
    pub env_keys: Vec<String>,
    pub context: Option<u64>,
    pub max_out: Option<u64>,
    pub price_in: f64,
    pub price_out: f64,
    pub supports_tools: bool,
    pub reasoning: bool,
    pub deprecated: bool,
}

impl ResolvedModel {
    pub fn label(&self) -> String {
        format!("{}/{}", self.provider_id, self.model_id)
    }
}

pub fn resolve(spec: Option<&str>, cfg: &Config) -> Result<ResolvedModel> {
    let owned;
    let spec = match spec {
        Some(s) => s.trim(),
        None => match &cfg.model {
            Some(m) => m.trim(),
            None => {
                owned = catalog::default_model().full_id();
                &owned
            }
        },
    };
    resolve_spec(spec, cfg)
}

fn resolve_spec(spec: &str, cfg: &Config) -> Result<ResolvedModel> {
    if let Some((pid, custom_pcfg)) = custom_provider(spec, cfg) {
        let mid = spec.split_once('/').map(|(_, m)| m.trim()).unwrap_or("");
        return finish_custom(pid, mid, custom_pcfg);
    }
    let entry = catalog::resolve(spec)?;
    let pcfg = cfg.providers.get(&entry.provider_meta().id.to_lowercase());
    finish_catalog(entry, pcfg)
}

fn custom_provider<'a>(spec: &str, cfg: &'a Config) -> Option<(String, &'a ProviderConfig)> {
    let (pid, _) = spec.split_once('/')?;
    let pid = pid.trim().to_lowercase();
    let pcfg = cfg.providers.get(&pid)?;
    let kind = pcfg.kind.as_deref().unwrap_or("openai");
    Protocol::parse(kind)?;
    Some((pid, pcfg))
}

fn finish_custom(pid: String, mid: &str, pcfg: &ProviderConfig) -> Result<ResolvedModel> {
    let protocol = Protocol::parse(pcfg.kind.as_deref().unwrap_or("openai"))
        .expect("validated in custom_provider");
    Ok(ResolvedModel {
        provider_id: pid.clone(),
        provider_name: pid,
        model_id: mid.to_string(),
        display: mid.to_string(),
        protocol,
        base_url: pcfg
            .base_url
            .clone()
            .unwrap_or_else(|| protocol.default_base().to_string()),
        api_key: pcfg.api_key.clone(),
        env_keys: Vec::new(),
        context: None,
        max_out: None,
        price_in: 0.0,
        price_out: 0.0,
        supports_tools: true,
        reasoning: false,
        deprecated: false,
    })
}

fn finish_catalog(entry: ModelEntry, pcfg: Option<&ProviderConfig>) -> Result<ResolvedModel> {
    let meta = entry.provider_meta();
    let protocol_str = pcfg
        .and_then(|p| p.kind.as_deref())
        .or(entry.protocol_override.as_deref())
        .unwrap_or(meta.protocol.as_str());
    let protocol = Protocol::parse(protocol_str).ok_or_else(|| {
        CoreError::Config(format!(
            "provider `{}` needs account setup Halis does not support yet; \
             pick a provider marked with a protocol in `halis models`",
            meta.id
        ))
    })?;

    let mut base_url = pcfg
        .and_then(|p| p.base_url.clone())
        .or(entry.base_override.clone())
        .unwrap_or_else(|| meta.base.clone());
    if base_url.is_empty() {
        base_url = protocol.default_base().to_string();
    }
    while base_url.ends_with('/') {
        base_url.pop();
    }

    let api_key = pcfg
        .and_then(|p| p.api_key.clone())
        .or_else(|| meta.env.iter().find_map(|k| std::env::var(k).ok()));

    Ok(ResolvedModel {
        provider_id: meta.id.clone(),
        provider_name: meta.name.clone(),
        display: entry.name.clone(),
        model_id: entry.id.clone(),
        protocol,
        base_url,
        api_key,
        env_keys: meta.env.clone(),
        context: entry.context,
        max_out: entry.max_out,
        price_in: entry.price_in.unwrap_or(0.0),
        price_out: entry.price_out.unwrap_or(0.0),
        supports_tools: entry.tools(),
        reasoning: entry.reasoning(),
        deprecated: entry.deprecated(),
    })
}

pub fn require_api_key(model: &ResolvedModel) -> Result<&str> {
    model.api_key.as_deref().ok_or_else(|| {
        let hint = if model.env_keys.is_empty() {
            "an api key under [providers.<id>] in config.toml".to_string()
        } else {
            model.env_keys.join(" or ")
        };
        CoreError::MissingKey(model.provider_name.clone(), hint)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_from_catalog() {
        let cfg = Config::default();
        let m = resolve(Some("anthropic/claude-sonnet-4-5"), &cfg).unwrap();
        assert_eq!(m.protocol, Protocol::Anthropic);
        assert_eq!(m.base_url, "https://api.anthropic.com/v1");
        assert_eq!(m.env_keys, vec!["ANTHROPIC_API_KEY"]);

        let m = resolve(Some("xai/grok-4.6"), &cfg).unwrap();
        assert_eq!(m.protocol, Protocol::OpenAi);
        assert_eq!(m.base_url, "https://api.x.ai/v1");

        let m = resolve(Some("google/gemini-2.5-pro"), &cfg).unwrap();
        assert_eq!(m.protocol, Protocol::Google);
    }

    #[test]
    fn resolves_custom_provider() {
        let mut cfg = Config::default();
        cfg.providers.insert(
            "mylocal".into(),
            ProviderConfig {
                kind: Some("openai".into()),
                base_url: Some("http://localhost:11434/v1".into()),
                api_key: Some("ollama".into()),
            },
        );
        let m = resolve(Some("mylocal/llama3"), &cfg).unwrap();
        assert_eq!(m.provider_id, "mylocal");
        assert_eq!(m.model_id, "llama3");
        assert_eq!(m.protocol, Protocol::OpenAi);
        assert_eq!(m.base_url, "http://localhost:11434/v1");
        assert_eq!(m.api_key.as_deref(), Some("ollama"));
    }

    #[test]
    fn unsupported_provider_errors_clearly() {
        let cfg = Config::default();
        let err = resolve(Some("amazon-bedrock/amazon.nova-lite-v1:0"), &cfg).unwrap_err();
        assert!(err.to_string().contains("account setup"), "got: {err}");
    }
}
