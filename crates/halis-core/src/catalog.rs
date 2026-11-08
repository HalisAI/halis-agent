use crate::error::{CoreError, Result};
use serde::Deserialize;
use std::sync::OnceLock;

pub const FLAG_TOOLS: u8 = 1;
pub const FLAG_VISION: u8 = 2;
pub const FLAG_REASONING: u8 = 4;
pub const FLAG_DEPRECATED: u8 = 8;

#[derive(Debug, Clone, Deserialize)]
pub struct ProviderMeta {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub env: Vec<String>,
    #[serde(default)]
    pub protocol: String,
    #[serde(default)]
    pub base: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ModelEntry {
    #[serde(rename = "p")]
    pub provider: u16,
    pub id: String,
    #[serde(rename = "n", default)]
    pub name: String,
    #[serde(rename = "f", default)]
    pub flags: u8,
    #[serde(rename = "r", default)]
    pub release: i32,
    #[serde(rename = "cx", default)]
    pub context: Option<u64>,
    #[serde(rename = "mo", default)]
    pub max_out: Option<u64>,
    #[serde(rename = "pi", default)]
    pub price_in: Option<f64>,
    #[serde(rename = "po", default)]
    pub price_out: Option<f64>,
    #[serde(rename = "pp", default)]
    pub protocol_override: Option<String>,
    #[serde(rename = "pb", default)]
    pub base_override: Option<String>,
}

impl ModelEntry {
    pub fn provider_meta(&self) -> &'static ProviderMeta {
        &catalog().providers[self.provider as usize]
    }

    pub fn full_id(&self) -> String {
        format!("{}/{}", self.provider_meta().id, self.id)
    }

    pub fn tools(&self) -> bool {
        self.flags & FLAG_TOOLS != 0
    }

    pub fn vision(&self) -> bool {
        self.flags & FLAG_VISION != 0
    }

    pub fn reasoning(&self) -> bool {
        self.flags & FLAG_REASONING != 0
    }

    pub fn deprecated(&self) -> bool {
        self.flags & FLAG_DEPRECATED != 0
    }

    pub fn protocol(&self) -> &str {
        self.protocol_override
            .as_deref()
            .unwrap_or(&self.provider_meta().protocol)
    }

    pub fn base(&self) -> &str {
        self.base_override
            .as_deref()
            .unwrap_or(&self.provider_meta().base)
    }
}

#[derive(Debug, Deserialize)]
struct CatalogData {
    providers: Vec<ProviderMeta>,
    models: Vec<ModelEntry>,
}

static CATALOG: OnceLock<CatalogData> = OnceLock::new();

fn catalog() -> &'static CatalogData {
    CATALOG.get_or_init(|| {
        serde_json::from_str(include_str!("catalog/data.json")).expect("embedded catalog parses")
    })
}

pub fn providers() -> &'static [ProviderMeta] {
    &catalog().providers
}

pub fn models() -> &'static [ModelEntry] {
    &catalog().models
}

pub fn find_provider(id: &str) -> Option<&'static ProviderMeta> {
    providers().iter().find(|p| p.id.eq_ignore_ascii_case(id))
}

pub fn models_of_provider(id: &str) -> Vec<&'static ModelEntry> {
    let idx = find_provider(id).map(|p| {
        providers()
            .iter()
            .position(|p2| p2.id == p.id)
            .expect("provider index")
    });
    match idx {
        Some(i) => models()
            .iter()
            .filter(|m| m.provider as usize == i)
            .collect(),
        None => Vec::new(),
    }
}

pub fn resolve(spec: &str) -> Result<ModelEntry> {
    let spec = spec.trim();
    if spec.is_empty() {
        return Err(CoreError::UnknownModel(spec.to_string()));
    }
    if let Some((pid, mid)) = spec.split_once('/') {
        let hit = models().iter().find(|m| {
            m.provider_meta().id.eq_ignore_ascii_case(pid) && m.id.eq_ignore_ascii_case(mid)
        });
        if let Some(m) = hit {
            return Ok(m.clone());
        }
        return Err(CoreError::UnknownModel(spec.to_string()));
    }
    let exact: Vec<&ModelEntry> = models()
        .iter()
        .filter(|m| m.id.eq_ignore_ascii_case(spec))
        .collect();
    match exact.len() {
        1 => return Ok(exact[0].clone()),
        n if n > 1 => return Err(ambiguous(spec, &exact)),
        _ => {}
    }
    let lower = spec.to_lowercase();
    let hits: Vec<&ModelEntry> = models()
        .iter()
        .filter(|m| m.id.to_lowercase().contains(&lower) || m.name.to_lowercase().contains(&lower))
        .collect();
    match hits.len() {
        1 => Ok(hits[0].clone()),
        0 => Err(CoreError::UnknownModel(spec.to_string())),
        _ => Err(ambiguous(spec, &hits)),
    }
}

fn ambiguous(spec: &str, hits: &[&ModelEntry]) -> CoreError {
    let list = hits
        .iter()
        .take(8)
        .map(|m| m.full_id())
        .collect::<Vec<_>>()
        .join(", ");
    let suffix = if hits.len() > 8 { " …" } else { "" };
    CoreError::AmbiguousModel(spec.to_string(), format!("{list}{suffix}"))
}

fn provider_has_key(meta: &ProviderMeta) -> bool {
    meta.env.iter().any(|k| std::env::var(k).is_ok())
}

fn newest_model(pid: &str) -> Option<&'static ModelEntry> {
    let mut best: Option<&ModelEntry> = None;
    for m in models_of_provider(pid) {
        if !m.tools() || m.protocol().is_empty() {
            continue;
        }
        let id = m.id.to_lowercase();
        if id.contains("opus") || id.contains("ultra") || id.contains("premier") {
            continue;
        }
        if best.is_none_or(|b| m.release > b.release) {
            best = Some(m);
        }
    }
    best
}

const PROVIDER_PRIORITY: &[&str] = &[
    "anthropic",
    "openai",
    "google",
    "xai",
    "groq",
    "deepseek",
    "mistral",
    "openrouter",
    "together",
    "cerebras",
    "perplexity",
    "cohere",
    "deepinfra",
    "fireworks",
];

pub fn default_model() -> &'static ModelEntry {
    for pid in PROVIDER_PRIORITY {
        if let Some(meta) = find_provider(pid)
            && provider_has_key(meta)
            && let Some(m) = newest_model(pid)
        {
            return m;
        }
    }
    let mut fallback: Option<&ModelEntry> = None;
    for p in providers() {
        if provider_has_key(p)
            && let Some(m) = newest_model(&p.id)
        {
            fallback = Some(m);
            break;
        }
    }
    fallback
        .or_else(|| newest_model("anthropic"))
        .unwrap_or(&models()[0])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_loads() {
        assert!(providers().len() > 100);
        assert!(models().len() > 1000);
        for pid in ["anthropic", "openai", "google", "openrouter", "xai"] {
            assert!(find_provider(pid).is_some(), "missing {pid}");
            assert!(!models_of_provider(pid).is_empty(), "no models for {pid}");
        }
    }

    #[test]
    fn resolves_specs() {
        let m = resolve("anthropic/claude-sonnet-4-5").unwrap();
        assert_eq!(m.id, "claude-sonnet-4-5");
        assert!(m.tools());
        assert_eq!(m.context, Some(200_000));

        let m = resolve("google/gemini-2.5-pro").unwrap();
        assert_eq!(m.provider_meta().id, "google");
        assert!(m.protocol().contains("google"));

        let m = resolve("openai/gpt-5.4-mini").unwrap();
        assert!(m.tools());

        assert!(resolve("definitely-not-a-model-xyz").is_err());
    }

    #[test]
    fn ambiguous_lists_candidates() {
        let err = resolve("claude-sonnet-4-5").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("anthropic"), "got: {msg}");
    }

    #[test]
    fn default_model_is_usable() {
        let m = default_model();
        assert!(m.tools());
        assert!(!m.protocol().is_empty());
    }
}
