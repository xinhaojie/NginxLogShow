use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use log::error;
use serde::Serialize;
use std::fmt;

#[derive(Debug)]
pub struct AppError {
    pub status_code: StatusCode,
    pub code: i32,
    pub source: anyhow::Error,
    pub user_message: Option<String>,
}

impl AppError {
    pub fn new(status_code: StatusCode, code: i32, msg: impl Into<String>) -> Self {
        Self {
            status_code,
            code,
            source: anyhow::anyhow!("{}", msg.into()),
            user_message: None,
        }
    }

    pub fn _with_user_message(mut self, user_msg: impl Into<String>) -> Self {
        self.user_message = Some(user_msg.into());
        self
    }

    pub fn _with_context(
        status_code: StatusCode,
        code: i32,
        msg: impl Into<String>,
    ) -> impl FnOnce(anyhow::Error) -> AppError {
        let msg = msg.into();
        move |err: anyhow::Error| AppError {
            status_code,
            code,
            source: err.context(msg.clone()),
            user_message: None,
        }
    }

    pub fn database(err: impl Into<anyhow::Error>) -> Self {
        Self {
            status_code: StatusCode::INTERNAL_SERVER_ERROR,
            code: -500,
            source: err.into().context("数据库操作失败"),
            user_message: None,
        }
    }

    pub fn bad_request(msg: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, -400, msg)
    }

    pub fn internal(msg: impl Into<String>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, -500, msg)
    }
}

impl fmt::Display for AppError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.source)
    }
}

impl std::error::Error for AppError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source.source()
    }
}

impl From<anyhow::Error> for AppError {
    fn from(err: anyhow::Error) -> Self {
        Self {
            status_code: StatusCode::INTERNAL_SERVER_ERROR,
            code: -500,
            source: err,
            user_message: None,
        }
    }
}

impl From<rusqlite::Error> for AppError {
    fn from(err: rusqlite::Error) -> Self {
        Self::database(err)
    }
}

impl From<std::io::Error> for AppError {
    fn from(err: std::io::Error) -> Self {
        Self::internal(format!("IO 操作失败: {}", err))
    }
}

#[derive(Serialize)]
struct ErrorResponse {
    code: i32,
    message: String,
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let error_chain: Vec<String> = self
            .source
            .chain()
            .map(|e| e.to_string())
            .collect();

        let detailed_message = error_chain.join(" <- ");

        error!("========== API 错误详情 ==========");
        error!("HTTP 状态码: {}", self.status_code);
        error!("错误码: {}", self.code);
        error!("错误链: {}", detailed_message);
        
        if let Some(root_cause) = error_chain.last() {
            error!("根本原因: {}", root_cause);
        }
        
        error!("====================================");

        let user_friendly_message = self.user_message.unwrap_or_else(|| {
            match self.status_code {
                // 参数校验错误把具体原因返回给调用方（error_chain 最外层即我们拼装的消息）
                StatusCode::BAD_REQUEST => error_chain
                    .first()
                    .cloned()
                    .unwrap_or_else(|| "请求参数错误".to_string()),
                StatusCode::NOT_FOUND => "请求的资源不存在".to_string(),
                StatusCode::UNAUTHORIZED => "认证失败，请重新登录".to_string(),
                StatusCode::INTERNAL_SERVER_ERROR => "服务器内部错误，请稍后重试".to_string(),
                _ => "操作失败，请稍后重试".to_string(),
            }
        });

        let response = ErrorResponse {
            code: self.code,
            message: user_friendly_message,
        };

        (self.status_code, Json(response)).into_response()
    }
}

pub type AppResult<T> = Result<T, AppError>;
