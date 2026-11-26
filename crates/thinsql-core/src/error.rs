//! 错误分类。UI 按 `kind` 给出不同的引导文案，而不是把驱动原文直接甩给用户。

/// 错误大类。每个大类对应一句可执行的建议（见 [`ErrorKind::hint`]）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    Network,
    Auth,
    UnknownDatabase,
    Timeout,
    ServerError,
    Protocol,
    Cancelled,
    Config,
    Unsupported,
    Internal,
}

impl ErrorKind {
    pub fn hint(self) -> &'static str {
        match self {
            Self::Network => "检查主机地址、端口与防火墙，确认数据库服务正在运行",
            Self::Auth => "用户名或密码不正确，或该账号不允许从当前主机登录",
            Self::UnknownDatabase => "数据库不存在，或当前账号没有访问权限",
            Self::Timeout => "服务器响应超时，可调大超时时间或在低峰期重试",
            Self::ServerError => "服务器拒绝了这条语句，请根据报错原文调整 SQL",
            Self::Protocol => "与服务器的协议交互异常，可能是版本不兼容或连接被中断",
            Self::Cancelled => "操作已取消",
            Self::Config => "配置有误，请检查连接信息与配置文件的读写权限",
            Self::Unsupported => "当前驱动尚不支持该操作",
            Self::Internal => "内部错误，请查看日志",
        }
    }

    pub fn is_retryable(self) -> bool {
        matches!(self, Self::Network | Self::Timeout)
    }
}

/// 服务器返回的原始错误信息，尽量保留原文供用户排查。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerError {
    /// 数据库错误码（MySQL error number / PostgreSQL SQLSTATE / Oracle ORA-xxxxx）
    pub code: Option<String>,
    pub sql_state: Option<String>,
    pub message: String,
    /// 出错位置（字符偏移），PostgreSQL 会给出
    pub position: Option<u32>,
}

#[derive(Debug, thiserror::Error)]
#[error("{}", .message)]
pub struct Error {
    pub kind: ErrorKind,
    pub message: String,
    pub server: Option<ServerError>,
    #[source]
    pub source: Option<Box<dyn std::error::Error + Send + Sync>>,
}

impl Error {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            server: None,
            source: None,
        }
    }

    pub fn with_source(
        kind: ErrorKind,
        message: impl Into<String>,
        source: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        Self {
            kind,
            message: message.into(),
            server: None,
            source: Some(Box::new(source)),
        }
    }

    pub fn from_server(kind: ErrorKind, server: ServerError) -> Self {
        Self {
            kind,
            message: server.message.clone(),
            server: Some(server),
            source: None,
        }
    }

    pub fn cancelled() -> Self {
        Self::new(ErrorKind::Cancelled, "操作已取消")
    }

    pub fn unsupported(what: impl Into<String>) -> Self {
        Self::new(ErrorKind::Unsupported, what)
    }

    pub fn config(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Config, message)
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Internal, message)
    }

    pub fn hint(&self) -> &'static str {
        self.kind.hint()
    }

    /// 面向日志的单行摘要，绝不包含密码等敏感信息。
    pub fn log_summary(&self) -> String {
        match &self.server {
            Some(s) => format!(
                "{:?}: {} [code={:?} state={:?}]",
                self.kind, self.message, s.code, s.sql_state
            ),
            None => format!("{:?}: {}", self.kind, self.message),
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(err: std::io::Error) -> Self {
        Self::with_source(ErrorKind::Config, format!("文件读写失败: {err}"), err)
    }
}

impl From<serde_json::Error> for Error {
    fn from(err: serde_json::Error) -> Self {
        Self::with_source(ErrorKind::Config, format!("配置格式非法: {err}"), err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_kind_has_actionable_hint() {
        for kind in [
            ErrorKind::Network,
            ErrorKind::Auth,
            ErrorKind::UnknownDatabase,
            ErrorKind::Timeout,
            ErrorKind::ServerError,
            ErrorKind::Protocol,
            ErrorKind::Cancelled,
            ErrorKind::Config,
            ErrorKind::Unsupported,
            ErrorKind::Internal,
        ] {
            assert!(!kind.hint().is_empty(), "{kind:?} 缺少引导文案");
        }
    }

    #[test]
    fn only_network_and_timeout_are_retryable() {
        assert!(ErrorKind::Network.is_retryable());
        assert!(ErrorKind::Timeout.is_retryable());
        assert!(!ErrorKind::Auth.is_retryable());
        assert!(!ErrorKind::ServerError.is_retryable());
    }

    #[test]
    fn server_error_keeps_original_message() {
        let err = Error::from_server(
            ErrorKind::ServerError,
            ServerError {
                code: Some("1064".into()),
                sql_state: Some("42000".into()),
                message: "You have an error in your SQL syntax".into(),
                position: Some(8),
            },
        );
        assert!(err.to_string().contains("SQL syntax"));
        assert!(err.log_summary().contains("1064"));
    }
}
