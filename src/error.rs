use std::fmt;

/// Errors from Cardano contract interactions.
#[derive(Debug)]
pub enum CardanoError {
    /// HTTP/network errors (Blockfrost API unreachable, timeouts).
    Network(reqwest::Error),
    /// Datum or CBOR parsing failures.
    Parse(String),
    /// Expected resource not found (no UTxO, no datum, no script address).
    NotFound(String),
}

impl fmt::Display for CardanoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CardanoError::Network(e) => write!(f, "network error: {}", e),
            CardanoError::Parse(msg) => write!(f, "parse error: {}", msg),
            CardanoError::NotFound(msg) => write!(f, "not found: {}", msg),
        }
    }
}

impl std::error::Error for CardanoError {}

impl From<reqwest::Error> for CardanoError {
    fn from(e: reqwest::Error) -> Self {
        CardanoError::Network(e)
    }
}
