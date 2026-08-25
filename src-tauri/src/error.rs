use serde::Serialize;
use thiserror::Error;

#[derive(Debug, Clone, Serialize, Error)]
#[error("{message}")]
pub struct AppError {
    pub code: ErrorCode,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote_code: Option<i32>,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ErrorCode {
    AuthRequired,
    SessionExpired,
    SecurityVerificationRequired,
    InvalidArgument,
    RateLimited,
    NetworkError,
    RemoteApiError,
}

impl AppError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            remote_code: None,
        }
    }

    pub fn remote(code: ErrorCode, remote_code: i32, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            remote_code: Some(remote_code),
        }
    }

    pub fn auth_required() -> Self {
        Self::new(
            ErrorCode::AuthRequired,
            "尚未登录，请先打开 MC反馈查看器完成登录",
        )
    }

    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::InvalidArgument, message)
    }

    pub fn from_reqwest(error: reqwest::Error) -> Self {
        let message = if error.is_timeout() {
            "连接网易服务超时，请稍后重试"
        } else if error.is_connect() {
            "无法连接网易服务，请检查网络"
        } else {
            "网络请求失败，请稍后重试"
        };
        Self::new(ErrorCode::NetworkError, message)
    }
}
