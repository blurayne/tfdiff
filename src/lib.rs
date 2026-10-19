pub mod authors;
pub mod cache;
pub mod compact;
pub mod export;
pub mod formatter;
pub mod input;
pub mod inventory;
pub mod migrations;
pub mod models;
pub mod parser;
pub mod pattern;
pub mod progress;
pub mod secrets;
pub mod tfstate_branches;
pub mod tfstate_duplicates;
pub mod ui;

/// Serializes tests that touch the process-global `colored` override.
#[cfg(test)]
pub(crate) static TEST_COLOR_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub use export::*;
pub use formatter::*;
pub use models::*;
pub use parser::*;

pub type Result<T> = std::result::Result<T, TfdiffError>;

#[derive(Debug, thiserror::Error)]
pub enum TfdiffError {
    #[error("Failed to parse Terraform output: {0}")]
    ParseError(String),

    #[error("IO error: {0}")]
    IoError(#[from] std::io::Error),

    #[error("JSON error: {0}")]
    JsonError(#[from] serde_json::Error),

    #[error("Regex error: {0}")]
    RegexError(#[from] regex::Error),

    #[error("Web server error: {0}")]
    WebError(String),

    #[error("Input error for {input}: {message}")]
    InputError { input: String, message: String },
}
