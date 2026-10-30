//! nginx 日志分析服务
//!
//! Run with
//!
//! ```not_rust
//! cargo run
//! ```
mod logger;
mod state;
mod app;
mod config;
mod tools;
mod error;
mod database;
mod nginx;
mod web;

use logger::init;
use std::sync::Arc;
use crate::tools::rate_limiter::{rate_limit_config, rate_limit_middleware, RateLimiter};
use shadow_rs::shadow;
use chrono::DateTime;

shadow!(build);
fn format_build_time() -> String {
    DateTime::parse_from_str(build::BUILD_TIME, "%Y-%m-%d %H:%M:%S %z")
        .map(|dt| dt.format("%Y%m%d_%H%M%S").to_string())
        .unwrap_or_else(|_| build::BUILD_TIME.to_string())
}

#[tokio::main(worker_threads = 8)]
async fn main() {
    // 初始化日志
    let _logger = init();

    // 创建限速器
    let rate_limiter = Arc::new(RateLimiter::new(rate_limit_config()));

    let shared_state = std::sync::Arc::new(state::AppState::new().await);
    log::info!("{}:{}: Build_v{}_{}",
            env!("CARGO_PKG_NAME"),
            std::env::var("RUN_MODE").unwrap_or("none".to_string()),
            env!("CARGO_PKG_VERSION"),
            format_build_time());

    // 构建应用并注入状态
    let app = app::create_router()
        .layer(axum::middleware::from_fn_with_state(
            rate_limiter.clone(),
            rate_limit_middleware,
        ))
        .layer(axum::middleware::from_fn_with_state(
            shared_state.clone(),
            web::auth::require_login,
        ))
        .with_state(shared_state.clone())
        .into_make_service_with_connect_info::<std::net::SocketAddr>();

    // run it（端口取 config.toml 的 [server] port）
    let listener = tokio::net::TcpListener::bind(format!(
        "{}:{}",
        shared_state.app_config.server.host,
        shared_state.app_config.server.port
    ))
    .await
    .unwrap();
    log::info!("listening on {}", listener.local_addr().unwrap());
    axum::serve(listener, app).await.unwrap();
}
