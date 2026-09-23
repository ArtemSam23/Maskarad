use thiserror::Error;

#[derive(Debug, Error)]
pub enum CoreError {
    #[error("configuration error: {0}")]
    Config(String),
    #[error("invalid regular expression in type {ty}: {source}")]
    Regex {
        ty: String,
        #[source]
        source: regex::Error,
    },
    #[error("dictionary error: {0}")]
    Dictionary(String),
    #[error("unknown system: {0}")]
    UnknownSystem(String),
    #[error("unknown profile: {0}")]
    UnknownProfile(String),
    #[error("yaml error: {0}")]
    Yaml(#[from] serde_yaml::Error),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, CoreError>;
