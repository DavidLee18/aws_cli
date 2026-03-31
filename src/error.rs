use std::fmt;

/// Errors that can occur in the AWS CLI.
#[derive(Debug)]
pub enum CliError {
    /// AWS SDK / transport error message.
    Aws(String),
    /// Configuration error (e.g., missing credentials).
    Config(String),
    /// Invalid user input.
    Input(String),
    /// I/O error.
    Io(std::io::Error),
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CliError::Aws(msg) => write!(f, "AWS error: {msg}"),
            CliError::Config(msg) => write!(f, "Configuration error: {msg}"),
            CliError::Input(msg) => write!(f, "Input error: {msg}"),
            CliError::Io(e) => write!(f, "I/O error: {e}"),
        }
    }
}

impl std::error::Error for CliError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            CliError::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for CliError {
    fn from(e: std::io::Error) -> Self {
        CliError::Io(e)
    }
}

impl From<anyhow::Error> for CliError {
    fn from(e: anyhow::Error) -> Self {
        CliError::Aws(e.to_string())
    }
}
