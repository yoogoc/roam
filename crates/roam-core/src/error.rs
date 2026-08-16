use std::sync::Arc;

use opendal::ErrorKind;

pub type Result<T> = std::result::Result<T, Error>;

/// Errors are `Clone` because the UI stores the last failure in view state and
/// re-renders it; `opendal::Error` is not `Clone`, hence the `Arc`.
#[derive(Debug, Clone, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Backend(Arc<opendal::Error>),

    /// The tokio task was aborted — this is the normal outcome of cancelling a
    /// listing by navigating away, so callers usually ignore it.
    #[error("background task was cancelled")]
    Cancelled,

    #[error("{0}")]
    Config(String),

    #[error("{0}")]
    Secret(String),

    /// The backend does not support the requested operation. Distinct from a
    /// failure: nothing was attempted.
    #[error("{0}")]
    Unsupported(String),
}

impl From<opendal::Error> for Error {
    fn from(e: opendal::Error) -> Self {
        Self::Backend(Arc::new(e))
    }
}

impl From<tokio::task::JoinError> for Error {
    fn from(_: tokio::task::JoinError) -> Self {
        Self::Cancelled
    }
}

/// The one action we offer the user alongside an error message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recovery {
    Refresh,
    EditCredentials,
    RetryLater,
    OpenSettings,
}

impl Error {
    pub fn kind(&self) -> Option<ErrorKind> {
        match self {
            Self::Backend(e) => Some(e.kind()),
            _ => None,
        }
    }

    pub fn is_cancelled(&self) -> bool {
        matches!(self, Self::Cancelled)
    }

    /// A message written for the person looking at the window, not for a log.
    pub fn user_message(&self) -> String {
        match self.kind() {
            Some(ErrorKind::NotFound) => "路径已不存在".into(),
            Some(ErrorKind::PermissionDenied) => "没有访问权限".into(),
            Some(ErrorKind::RateLimited) => "请求过于频繁".into(),
            Some(ErrorKind::ConfigInvalid) => "连接配置有误".into(),
            Some(ErrorKind::IsADirectory) => "这是一个目录".into(),
            Some(ErrorKind::NotADirectory) => "这不是一个目录".into(),
            _ => match self {
                Self::Cancelled => "操作已取消".into(),
                Self::Config(msg) | Self::Secret(msg) | Self::Unsupported(msg) => msg.clone(),
                Self::Backend(e) => e.to_string(),
            },
        }
    }

    pub fn recovery(&self) -> Option<Recovery> {
        match self {
            // A bad profile or a missing credential is fixed in the connection
            // dialog, not by retrying.
            Self::Config(_) => return Some(Recovery::OpenSettings),
            Self::Secret(_) => return Some(Recovery::EditCredentials),
            _ => {}
        }

        match self.kind()? {
            ErrorKind::NotFound => Some(Recovery::Refresh),
            ErrorKind::PermissionDenied => Some(Recovery::EditCredentials),
            ErrorKind::RateLimited => Some(Recovery::RetryLater),
            ErrorKind::ConfigInvalid => Some(Recovery::OpenSettings),
            _ => None,
        }
    }
}
