//! Error types for the agent runtime.

#[derive(Debug, PartialEq, Eq)]
pub enum AgentError {
    AutoDiscoveryFailed,
    ConfigError(String),
    LockError(String),
    Other(String),
}

impl std::fmt::Display for AgentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AgentError::AutoDiscoveryFailed => write!(f, "auto discovery aborted, sma exiting"),
            AgentError::ConfigError(e) => write!(f, "config error: {}", e),
            AgentError::LockError(e) => write!(f, "instance lock failed: {}", e),
            AgentError::Other(e) => write!(f, "{}", e),
        }
    }
}

impl std::error::Error for AgentError {}
