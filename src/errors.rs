use std::path::PathBuf;

/// Typed library errors.
///
/// The public API never returns `anyhow::Error`: a future service mode must be able to
/// serialize a structured error per file.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("{path}: {message}")]
    Query { path: PathBuf, message: String },

    #[error("{path}: unusable server response: {message}")]
    Response { path: PathBuf, message: String },

    #[error("{path}: {message}")]
    GraphQl { path: PathBuf, message: String },

    #[error("config: {0}")]
    Config(String),
}

/// A transient failure worth retrying (rate limit, timeout, 5xx).
///
/// A GraphQL validation error never is: it is deterministic, and retrying only wastes time.
pub trait Retryable {
    fn is_retryable(&self) -> bool;
}
