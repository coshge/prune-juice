//! One expected-error type, with a stable machine-readable code per variant.
//!
//! Anticipated failures become `Error` and are rendered as a single line by the
//! CLI. Anything that is genuinely a bug is left to panic — the two are handled
//! differently on purpose.

use thiserror::Error as ThisError;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, ThisError)]
pub enum Error {
    #[error("cannot reach the Docker daemon: {0}")]
    DaemonUnreachable(String),

    #[error("permission denied talking to Docker: {0}")]
    PermissionDenied(String),

    #[error("Docker API error: {0}")]
    Api(String),

    #[error("no Docker context could be resolved")]
    NoContext,

    #[error("index error: {0}")]
    Index(String),

    #[error("cancelled")]
    Cancelled,

    #[error("{0}")]
    Config(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
}

impl Error {
    /// Stable identifier for machine consumers. Never reword these — they are
    /// part of the NDJSON contract.
    pub fn code(&self) -> &'static str {
        match self {
            Error::DaemonUnreachable(_) => "daemon_unreachable",
            Error::PermissionDenied(_) => "permission_denied",
            Error::Api(_) => "api_error",
            Error::NoContext => "no_context",
            Error::Index(_) => "index_error",
            Error::Cancelled => "cancelled",
            Error::Config(_) => "config_error",
            Error::Io(_) => "io_error",
            Error::Json(_) => "json_error",
        }
    }

    /// Process exit code. Matches the house convention:
    /// 0 ok, 2 usage, 3 daemon unreachable, 4 permission denied,
    /// 5 partial failure, 130 cancelled.
    pub fn exit_code(&self) -> i32 {
        match self {
            Error::DaemonUnreachable(_) | Error::NoContext => 3,
            Error::PermissionDenied(_) => 4,
            Error::Cancelled => 130,
            Error::Config(_) => 2,
            _ => 1,
        }
    }

    /// A short actionable hint, where there is an obvious one.
    pub fn hint(&self) -> Option<&'static str> {
        match self {
            Error::DaemonUnreachable(_) => Some("Is Docker running?"),
            Error::PermissionDenied(_) => {
                Some("Your user may not be in the docker group, or the socket is root-owned.")
            }
            Error::NoContext => Some("Try `docker context ls` to see what is configured."),
            _ => None,
        }
    }
}

impl From<rusqlite::Error> for Error {
    fn from(e: rusqlite::Error) -> Self {
        Error::Index(e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_codes_match_the_house_convention() {
        assert_eq!(Error::DaemonUnreachable("x".into()).exit_code(), 3);
        assert_eq!(Error::PermissionDenied("x".into()).exit_code(), 4);
        assert_eq!(Error::Cancelled.exit_code(), 130);
        assert_eq!(Error::Config("x".into()).exit_code(), 2);
    }

    #[test]
    fn codes_are_snake_case_and_stable() {
        for e in [
            Error::DaemonUnreachable("x".into()),
            Error::PermissionDenied("x".into()),
            Error::NoContext,
            Error::Cancelled,
        ] {
            let c = e.code();
            assert!(!c.is_empty());
            assert!(c.chars().all(|ch| ch.is_ascii_lowercase() || ch == '_'));
        }
    }
}
