use crate::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionState {
    Unknown,
    Connecting,
    Online,
    Offline,
    AuthRequired,
    Throttled,
    ServiceError,
}

impl ConnectionState {
    pub fn from_error(error: &Error) -> Self {
        match error {
            Error::AuthRequired => Self::AuthRequired,
            Error::Transport(e) if e.is_connect() || e.is_timeout() => Self::Offline,
            Error::Http { status: 429, .. } => Self::Throttled,
            Error::Http {
                status: 403,
                reason,
            } if reason == "rateLimitExceeded" || reason == "userRateLimitExceeded" => {
                Self::Throttled
            }
            _ => Self::ServiceError,
        }
    }
}
