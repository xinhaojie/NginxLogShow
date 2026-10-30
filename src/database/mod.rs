// src/database/mod.rs
//! SQLite 数据库初始化模块（nginx 日志分析版）
//!
//! 设计要点：
//! - 双连接：读连接 + 写连接（WAL 模式下读不阻塞写）
//! - 写连接由后台解析任务独占使用（增量解析批量落盘）
//! - 读连接由 web handler 在 spawn_blocking 中加锁使用，也承担低频的配置/清理写入
//!
//! 表结构：
//! - settings        KV 配置（nginx 日志源配置存 JSON）
//! - access_logs     解析后的 nginx 访问日志（结构化列 + extra 自定义变量 JSON）
//! - parse_state     每个日志源的解析进度（offset、行数统计），支持增量读取

use std::path::Path;

use log::{info, warn};
use rusqlite::Connection;

use crate::config::SqliteConfig;

/// 初始化 SQLite 数据库，返回 (读连接, 写连接)
pub fn init_db(cfg: &SqliteConfig) -> (Connection, Connection) {
    let path = &cfg.path;
    // 确保父目录存在
    if let Some(parent) = Path::new(path).parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).expect("创建数据库目录失败");
        }
    }

    let read_db = Connection::open(path).expect("打开 SQLite 读连接失败");
    let write_db = Connection::open(path).expect("打开 SQLite 写连接失败");

    // 读连接也会执行配置写入 / DELETE 清理操作，与后台写入并发时等待而非报 BUSY
    read_db
        .pragma_update(None, "busy_timeout", 5000)
        .expect("设置读连接 busy_timeout 失败");

    // WAL 模式：读写并发不阻塞，性能更好
    write_db
        .pragma_update(None, "journal_mode", "WAL")
        .expect("设置 WAL 模式失败");
    write_db
        .pragma_update(None, "synchronous", "NORMAL")
        .expect("设置 synchronous 失败");
    write_db
        .pragma_update(None, "busy_timeout", 5000)
        .expect("设置 busy_timeout 失败");

    // WAL 调优：自动 checkpoint 阈值 + WAL 文件硬上限（读写连接都设，避免 WAL 无限膨胀）
    apply_wal_tuning(&write_db, cfg.wal_autocheckpoint, cfg.journal_size_limit);
    apply_wal_tuning(&read_db, cfg.wal_autocheckpoint, cfg.journal_size_limit);

    init_schema(&write_db).expect("初始化数据表失败");

    info!("SQLite 数据库初始化完成: {}", path);
    (read_db, write_db)
}

/// 设置 WAL 自动 checkpoint 阈值与 WAL 文件硬上限，防止长期运行后 WAL 无限膨胀。
/// - wal_autocheckpoint: 每写入 N 页自动 PASSIVE 回写 WAL 到主库；0 表示保持 SQLite 默认(1000)。
/// - journal_size_limit: WAL 文件字节上限；0 表示不限制。
fn apply_wal_tuning(conn: &Connection, wal_autocheckpoint: u32, journal_size_limit: u64) {
    if wal_autocheckpoint > 0 {
        if let Err(e) =
            conn.pragma_update(None, "wal_autocheckpoint", wal_autocheckpoint as i64)
        {
            warn!("设置 wal_autocheckpoint 失败: {}", e);
        }
    }
    if journal_size_limit > 0 {
        if let Err(e) =
            conn.pragma_update(None, "journal_size_limit", journal_size_limit as i64)
        {
            warn!("设置 journal_size_limit 失败: {}", e);
        }
    }
}

/// 建表建索引
fn init_schema(conn: &Connection) -> Result<(), rusqlite::Error> {
    conn.execute_batch(
        // KV 配置表：nginx 日志源配置以 JSON 存于 key='nginx_sources'
        "CREATE TABLE IF NOT EXISTS settings (
            key        TEXT PRIMARY KEY,
            value      TEXT NOT NULL,
            updated_at TEXT NOT NULL
        );

        -- 解析后的访问日志
        CREATE TABLE IF NOT EXISTS access_logs (
            id           INTEGER PRIMARY KEY AUTOINCREMENT,
            source_id    INTEGER NOT NULL,
            ts           INTEGER NOT NULL,          -- 请求时间（unix 秒）
            remote_addr  TEXT,
            method       TEXT,
            path         TEXT,                      -- URI 路径（不含查询串）
            query        TEXT,                      -- 查询串（不含 ?）
            protocol     TEXT,                      -- 如 HTTP/1.1
            status       INTEGER,
            bytes        INTEGER DEFAULT 0,         -- body_bytes_sent
            referer      TEXT,
            user_agent   TEXT,
            request_time REAL,
            extra        TEXT,                      -- log_format 中其它自定义变量的 JSON
            raw          TEXT                       -- 原始日志行
        );
        CREATE INDEX IF NOT EXISTS idx_access_ts         ON access_logs (ts);
        CREATE INDEX IF NOT EXISTS idx_access_status     ON access_logs (status);
        CREATE INDEX IF NOT EXISTS idx_access_addr       ON access_logs (remote_addr);
        CREATE INDEX IF NOT EXISTS idx_access_path       ON access_logs (path);
        CREATE INDEX IF NOT EXISTS idx_access_source_ts  ON access_logs (source_id, ts);

        -- 每个日志源的解析进度（增量读取断点 + 运行统计）
        CREATE TABLE IF NOT EXISTS parse_state (
            source_id     INTEGER PRIMARY KEY,
            offset        INTEGER NOT NULL DEFAULT 0,  -- 已读取到的字节偏移
            last_parse_at TEXT,                        -- 最近一次成功解析时间（RFC3339 UTC）
            lines_total   INTEGER NOT NULL DEFAULT 0,  -- 累计读取行数
            lines_failed  INTEGER NOT NULL DEFAULT 0,  -- 累计解析失败行数
            last_error    TEXT                         -- 最近一次错误信息
        );

        -- IP 归属地缓存（10 天内不重复查询）
        CREATE TABLE IF NOT EXISTS ip_geo (
            ip         TEXT PRIMARY KEY,
            country    TEXT,      -- 国家（如 中国 / 美国）
            region     TEXT,      -- 省份/州
            city       TEXT,      -- 城市
            isp        TEXT,      -- 运营商 / 机构
            queried_at INTEGER NOT NULL  -- 首次查询时间（unix 秒），仅作记录；缓存命中即永久有效
        );",
    )
}
