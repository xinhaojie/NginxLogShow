// src/state.rs
use std::sync::{Arc, Mutex};

use rusqlite::Connection;
use tokio::sync::watch;

use crate::config::AppConfig;
use crate::database;
use crate::nginx::models::NginxSourcesConfig;
use crate::nginx::watcher;

/// 全局共享状态
pub struct AppState {
    /// 读连接（spawn_blocking 中加锁使用；也承担低频配置写入与清理）
    pub read_db: Arc<Mutex<Connection>>,
    /// 配置
    pub app_config: Arc<AppConfig>,
    /// 日志源配置热更新通道：handler 修改配置后 send_replace 通知后台解析任务
    pub config_tx: watch::Sender<NginxSourcesConfig>,
    /// 手动重解析通知通道：值 = 要从头重新解析的 source_id（清数据后触发立即解析）
    pub reparse_tx: tokio::sync::mpsc::UnboundedSender<i64>,
}

impl AppState {
    pub async fn new() -> Self {
        let app_config = AppConfig::load().expect("配置加载失败");

        // 初始化数据库，拿到读连接和写连接
        let (read_db, write_db) = database::init_db(&app_config.sqlite);

        // 从 settings 表加载日志源配置（无则使用默认空配置）
        let sources_config = load_sources_config(&read_db);
        log::info!(
            "已加载日志源配置：{} 个源，解析间隔 {}s",
            sources_config.sources.len(),
            sources_config.interval_secs
        );

        // 配置热更新 channel（后台解析任务持有 receiver）
        let (config_tx, config_rx) = watch::channel(sources_config);
        // 手动重解析通知 channel
        let (reparse_tx, reparse_rx) = tokio::sync::mpsc::unbounded_channel();

        // 启动后台解析任务（独占写连接；附带 sqlite 配置供自动清理后 VACUUM 使用）
        tokio::spawn(watcher::watcher(
            write_db,
            config_rx,
            reparse_rx,
            app_config.sqlite.clone(),
        ));

        Self {
            read_db: Arc::new(Mutex::new(read_db)),
            app_config: Arc::new(app_config),
            config_tx,
            reparse_tx,
        }
    }
}

/// 从 settings 表加载日志源配置（key='nginx_sources'），解析失败回退默认配置
pub fn load_sources_config(conn: &Connection) -> NginxSourcesConfig {
    conn.query_row(
        "SELECT value FROM settings WHERE key = 'nginx_sources'",
        [],
        |row| row.get::<_, String>(0),
    )
    .ok()
    .and_then(|v| serde_json::from_str(&v).ok())
    .unwrap_or_default()
}
