//! Typed error variants for backend operations.
//!
//! Using `thiserror` gives us structured, matchable errors that the UI
//! can translate into actionable user-facing messages, while still
//! supporting `anyhow`-style context chaining when needed.

use thiserror::Error;
use uuid::Uuid;

/// Errors that can occur during backend operations.
#[derive(Debug, Error)]
pub enum BackendError {
    /// The backend could not establish a connection to its data source.
    #[error("Connection failed: {message}")]
    ConnectionFailed {
        message: String,
        #[source]
        source: Option<Box<dyn std::error::Error + Send + Sync>>,
    },

    /// Authentication or authorization was rejected.
    #[error("Authentication failed: {message}")]
    AuthenticationFailed { message: String },

    /// The server does not support token-based authentication
    /// (Subsonic error code 41).  The caller should retry with
    /// plaintext / hex-encoded credentials.
    #[error("Token authentication not supported: {message}")]
    TokenAuthNotSupported { message: String },

    /// A requested entity was not found.
    #[error("{entity_type} not found: {id}")]
    NotFound { entity_type: String, id: Uuid },

    /// The operation is not supported by this backend.
    ///
    /// For example, a DAAP backend may not support full-text search.
    #[error("Operation not supported: {operation}")]
    Unsupported { operation: String },

    /// A timeout occurred while waiting for a response.
    #[error("Operation timed out after {duration_secs}s")]
    Timeout { duration_secs: u64 },

    /// The backend received a response it could not parse.
    #[error("Failed to parse response: {message}")]
    ParseError {
        message: String,
        #[source]
        source: Option<Box<dyn std::error::Error + Send + Sync>>,
    },

    /// An I/O error occurred (filesystem, network, etc.).
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// A catch-all for unexpected errors with additional context.
    #[error("Internal error: {0}")]
    Internal(#[from] anyhow::Error),
}
