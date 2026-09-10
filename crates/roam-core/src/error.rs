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
    /// Deliberately short — the backend's own words are in [`Self::detail`].
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
                // No sentence of our own for this kind, so the backend's words
                // are all there is. `detail` is the readable half of opendal's
                // `Display`; fall back to the whole thing only if it is empty.
                Self::Backend(e) => self.backend_detail().unwrap_or_else(|| e.to_string()),
            },
        }
    }

    /// What the backend said that the headline does not already say.
    /// `user_message` flattens whole classes of failure into one short
    /// sentence, which is useless when the question is *why* a connection
    /// failed — this is what separates a wrong password from a host that is
    /// simply not listening. `None` when there is nothing to add, so a caller
    /// can render it unconditionally without ever printing the same text twice.
    pub fn detail(&self) -> Option<String> {
        self.backend_detail()
            .filter(|detail| *detail != self.user_message())
    }

    /// The backend's own words, whether or not `user_message` already uses
    /// them. opendal's `Display` leads with `kind (status) at operation,
    /// context { … }`, which only restates the headline, so this keeps the
    /// message and the source chain and drops the preamble.
    fn backend_detail(&self) -> Option<String> {
        let Self::Backend(err) = self else {
            return None;
        };

        let mut parts: Vec<String> = Vec::new();
        let mut push = |text: &str| {
            let text = text.trim();
            // A source usually restates its own cause, so anything already
            // spelled out inside an earlier part is noise.
            if !text.is_empty() && !parts.iter().any(|p: &String| p.contains(text)) {
                parts.push(text.to_string());
            }
        };

        push(err.message());
        let mut source = std::error::Error::source(err.as_ref());
        while let Some(cause) = source {
            push(&cause.to_string());
            source = cause.source();
        }

        // The parts come from the backend and are almost always English, so
        // they read better chained with an ASCII colon than a fullwidth one.
        (!parts.is_empty()).then(|| parts.join(": "))
    }

    /// Headline and detail in one line, for a notification or a tooltip. Where
    /// the layout has room for two lines, prefer rendering the two separately.
    pub fn full_message(&self) -> String {
        let headline = self.user_message();
        match self.detail() {
            Some(detail) => format!("{headline}：{detail}"),
            None => headline,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_headline_error_still_carries_the_backend_s_own_words() {
        let err: Error = opendal::Error::new(ErrorKind::PermissionDenied, "access denied")
            .with_operation("list")
            .with_context("service", "webdav")
            .into();

        // The headline stays short, and the reason lives beside it rather than
        // being thrown away.
        assert_eq!(err.user_message(), "没有访问权限");
        assert_eq!(err.detail().as_deref(), Some("access denied"));
        assert_eq!(err.full_message(), "没有访问权限：access denied");

        // opendal's `kind (status) at operation, context { … }` preamble is
        // noise for someone reading a banner.
        let detail = err.detail().unwrap();
        assert!(!detail.contains("PermissionDenied"), "got {detail}");
        assert!(!detail.contains("service"), "got {detail}");
    }

    #[test]
    fn the_source_chain_follows_the_message() {
        let source = std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "connect refused");
        let err: Error = opendal::Error::new(ErrorKind::ConfigInvalid, "cannot reach host")
            .set_source(source)
            .into();

        assert_eq!(
            err.detail().as_deref(),
            Some("cannot reach host: connect refused")
        );
    }

    #[test]
    fn a_kind_we_have_no_sentence_for_says_it_once() {
        let err: Error = opendal::Error::new(ErrorKind::Unexpected, "cannot reach host").into();

        // The backend's words *are* the headline here, so there is no detail to
        // put beside them — a banner rendering both must not print them twice.
        assert_eq!(err.user_message(), "cannot reach host");
        assert_eq!(err.detail(), None);
        assert_eq!(err.full_message(), "cannot reach host");
    }

    #[test]
    fn errors_that_are_already_their_own_message_have_no_detail() {
        let err = Error::Config("缺少服务地址".into());

        assert_eq!(err.detail(), None);
        assert_eq!(err.full_message(), "缺少服务地址");
    }
}
