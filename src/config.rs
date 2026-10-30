// src/config.rs
use serde::Deserialize;

// 顶层配置
#[derive(Debug, Deserialize, Clone)]
pub struct AppConfig {
    pub server: ServerConfig,
    pub sqlite: SqliteConfig,
    /// Web 登录配置；config.toml 缺省 [web] 时按未启用处理
    #[serde(default)]
    pub web: WebConfig,
}

#[derive(Debug, Deserialize, Clone)]
pub struct ServerConfig {
    pub host: String,
    pub port: u16,
}

#[derive(Debug, Deserialize, Clone)]
pub struct SqliteConfig {
    pub path: String,
    /// 删除日志后是否自动 VACUUM 整理数据库碎片，默认开启
    #[serde(default = "default_true")]
    pub vacuum_after_delete: bool,
    /// 删除条数达到该值才触发 VACUUM（避免小删除频繁全量重建），默认 1000
    #[serde(default = "default_vacuum_min_deleted")]
    pub vacuum_min_deleted: i64,
    /// 自动 checkpoint 阈值（页，1 页 = 4096 字节）：写入累计达到该页数时 SQLite 自动把
    /// WAL 回写主库。0 表示保持 SQLite 默认(1000≈4MB)。
    #[serde(default = "default_wal_autocheckpoint")]
    pub wal_autocheckpoint: u32,
    /// WAL 文件硬上限（字节）：即使 checkpoint 滞后也限制 WAL 增长，防止极端情况下撑爆磁盘。
    /// 0 表示不限制（SQLite 默认）。
    #[serde(default = "default_journal_size_limit")]
    pub journal_size_limit: u64,
    /// 每天定时把 WAL 回写主库（checkpoint）并截断 `-wal` 文件，避免长期运行后 WAL 无限膨胀。
    /// 默认开启。
    #[serde(default = "default_true")]
    pub wal_checkpoint_enabled: bool,
    /// 每天执行 WAL checkpoint 的时刻（HH:MM，本地时间），默认 04:30（与自动清理 04:00 错开）
    #[serde(default = "default_wal_checkpoint_time")]
    pub wal_checkpoint_time: String,
}

fn default_true() -> bool {
    true
}

fn default_vacuum_min_deleted() -> i64 {
    1000
}

fn default_wal_autocheckpoint() -> u32 {
    1000
}

fn default_journal_size_limit() -> u64 {
    8 * 1024 * 1024
}

fn default_wal_checkpoint_time() -> String {
    "04:30".to_string()
}

#[derive(Debug, Deserialize, Clone)]
pub struct WebConfig {
    /// 日志页面登录用户名；为空表示不启用登录保护
    #[serde(default)]
    pub username: String,
    /// 日志页面登录密码；为空表示不启用登录保护
    #[serde(default)]
    pub password: String,
    /// 登录态有效期（天），默认 7
    #[serde(default = "default_session_days")]
    pub session_days: i64,
}

impl Default for WebConfig {
    fn default() -> Self {
        Self {
            username: String::new(),
            password: String::new(),
            session_days: default_session_days(),
        }
    }
}

fn default_session_days() -> i64 {
    7
}

impl AppConfig {
    /// 加载配置的函数
    pub fn load() -> Result<Self, config::ConfigError> {
        // 1. 优先读取环境变量 RUN_MODE (如 dev, prod)，默认是 dev
        let run_mode = std::env::var("RUN_MODE").unwrap_or_else(|_| "debug".into());

        let builder = config::Config::builder()
            // 2. 先加载默认配置
            .add_source(config::File::with_name("config").required(false))
            // 3. 根据环境加载特定配置 (如 config.prod.toml 会覆盖 config.toml)
            .add_source(config::File::with_name(&format!("config.{}", run_mode)).required(false))
            // 4. 允许环境变量覆盖配置
            .add_source(config::Environment::with_prefix("APP").separator("__"))
            .build()?;

        // 5. 将配置数据反序列化为我们的结构体
        builder.try_deserialize()
    }
}
