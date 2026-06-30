use thiserror::Error;

#[derive(Debug, Error)]
pub enum ToolError {
    #[error("Tool not configured: {0}")]
    NotConfigured(String),

    #[error("Invalid argument: {0}")]
    InvalidArgument(String),

    #[error("HTTP error: {0}")]
    Http(String),

    #[error("Database error: {0}")]
    Database(String),

    #[error("Execution error: {0}")]
    Execution(String),

    #[error("Not found: {0}")]
    NotFound(String),
}
