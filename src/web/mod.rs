// src/web/mod.rs
//! Web 层：路由 + 登录认证 + handler
pub mod auth;
pub mod handler;
pub mod ip_geo;
pub mod stats;

use axum::routing::{get, post};
use axum::Router;
use std::sync::Arc;

use crate::state::AppState;

/// 路由
pub fn routes() -> Router<Arc<AppState>> {
    Router::<Arc<AppState>>::new()
        // Web 页面
        .route("/", get(handler::web_page))
        // Web 登录（登录保护中间件在 main.rs 挂载）
        .route("/login", get(auth::login_page).post(auth::login))
        .route("/logout", post(auth::logout))
        // 统计仪表盘
        .route("/api/stats/overview", get(handler::stats_overview))
        // 访问明细（分页查询）
        .route("/api/logs", get(handler::logs_query))
        // IP 归属地批量查询（ip_geo 表 10 天缓存）
        .route("/api/ipgeo", get(handler::ipgeo_query))
        // 日志源配置管理（热生效）
        .route("/api/sources", get(handler::get_sources).post(handler::save_sources))
        // 测试解析（用户在页面上验证 log_format 是否正确）
        .route("/api/sources/test", post(handler::test_parse))
        // 手动重解析单个源（清历史数据 + 从头解析）
        .route("/api/sources/{id}/reparse", post(handler::reparse_source))
        // 日志清理（按天数保留）与数据库大小
        .route("/api/logs/cleanup", axum::routing::delete(handler::delete_logs_before))
        .route("/api/db/size", get(handler::db_size))
        .route("/version", get(version))
}

#[derive(serde::Serialize)]
struct VersionResp {
    code: i32,
    message: String,
    version: String,
}

async fn version() -> axum::Json<VersionResp> {
    axum::Json(VersionResp {
        code: 0,
        message: "success".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
    })
}
