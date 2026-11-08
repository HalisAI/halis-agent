use thiserror::Error;

#[derive(Debug, Error)]
pub enum CoreError {
    #[error("config error: {0}")]
    Config(String),

    #[error("unknown model `{0}`; run `halis models` to list the catalog")]
    UnknownModel(String),

    #[error("ambiguous model `{0}`; candidates: {1}")]
    AmbiguousModel(String, String),

    #[error("no api key for {0}; set {1}")]
    MissingKey(String, String),

    #[error("{0}: {1}")]
    Provider(String, String),

    #[error("stream error: {0}")]
    Stream(String),

    #[error("tool `{0}`: {1}")]
    Tool(String, String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("{0}")]
    Other(String),
}

pub type Result<T, E = CoreError> = std::result::Result<T, E>;
