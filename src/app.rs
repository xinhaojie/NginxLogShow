// src/app.rs
use axum::{routing::get, Json, Router};
use serde::Serialize;
use std::sync::Arc;

use crate::state::AppState;
use crate::web;

#[derive(Serialize)]
pub struct RootResp {
    code: i32,
    message: &'static str,
}

async fn health() -> Json<RootResp> {
    Json(RootResp {
        code: 0,
        message: "success",
    })
}

pub fn create_router() -> Router<Arc<AppState>> {
    Router::<Arc<AppState>>::new()
        // Web 页面 + 统计/配置 API
        .merge(web::routes())
        // 健康检查
        .route("/health", get(health))
}
