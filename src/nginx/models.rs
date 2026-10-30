// src/nginx/models.rs
//! nginx 日志源配置与解析结果的数据模型

use serde::{Deserialize, Serialize};

/// 整体日志源配置（存 settings 表 key='nginx_sources'，JSON 序列化）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NginxSourcesConfig {
    /// 站点列表（逻辑分组：一个站点可挂多个日志源，统计按站点隔离）
    #[serde(default)]
    pub sites: Vec<Site>,
    /// 日志源列表
    #[serde(default)]
    pub sources: Vec<LogSource>,
    /// 后台解析间隔（秒），默认 60
    #[serde(default = "default_interval")]
    pub interval_secs: u64,
    /// 自动清理：是否启用（每天在 auto_cleanup_time 删除 auto_cleanup_days 天前的日志）
    #[serde(default)]
    pub auto_cleanup_enabled: bool,
    /// 自动清理执行时刻（本地时间 HH:MM，如 "04:00"）
    #[serde(default = "default_cleanup_time")]
    pub auto_cleanup_time: String,
    /// 自动清理：保留最近 N 天，删除更早的日志
    #[serde(default = "default_cleanup_days")]
    pub auto_cleanup_days: u32,
}

fn default_interval() -> u64 {
    60
}

fn default_cleanup_time() -> String {
    "04:00".to_string()
}

fn default_cleanup_days() -> u32 {
    90
}

impl Default for NginxSourcesConfig {
    fn default() -> Self {
        Self {
            sites: Vec::new(),
            sources: Vec::new(),
            interval_secs: default_interval(),
            auto_cleanup_enabled: false,
            auto_cleanup_time: default_cleanup_time(),
            auto_cleanup_days: default_cleanup_days(),
        }
    }
}

/// 站点（逻辑分组）
///
/// 数据隔离方式：access_logs 按来源写入（source_id），站点过滤时先取该站点
/// 下的全部 source_id，再用 `source_id IN (...)` 查询——无需给日志表加站点列。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Site {
    /// 唯一 ID（新增时由后端分配）
    #[serde(default)]
    pub id: i64,
    /// 站点名称（如 example.com）
    #[serde(default)]
    pub name: String,
    /// 备注（可选）
    #[serde(default)]
    pub note: String,
}

/// 单个 nginx 日志源
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogSource {
    /// 唯一 ID（新增时由后端分配，取当前最大 id + 1）
    #[serde(default)]
    pub id: i64,
    /// 所属站点 ID（0 = 未分组，不出现在站点隔离统计中，但可在「全部」里看到）
    #[serde(default)]
    pub site_id: i64,
    /// 显示名称
    #[serde(default)]
    pub name: String,
    /// nginx 日志文件绝对路径
    pub path: String,
    /// 解析格式，三选一：
    /// - 留空 ""：自动检测（JSON 行 → nginx error.log → combined）
    /// - "error"：强制按 nginx 原生 error.log 解析
    /// - 其它：access log 的 log_format 字符串（如 `$remote_addr - ... "$request" $status ...`），
    ///   同一站点可挂多个源且各用各的格式（如 access.log 用 combined、自定义格式日志各配各的）
    #[serde(default)]
    pub format: String,
    /// 是否启用该源
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// 新增源首次解析时是否从文件末尾开始（true=只解析新增内容；false=解析全部历史）
    #[serde(default)]
    pub start_at_end: bool,
}

fn default_true() -> bool {
    true
}

/// 单行日志解析结果（与 access_logs 表列一一对应）
#[derive(Debug, Clone, Default)]
pub struct ParsedLine {
    pub ts: i64,
    pub remote_addr: Option<String>,
    pub method: Option<String>,
    pub path: Option<String>,
    pub query: Option<String>,
    pub protocol: Option<String>,
    pub status: Option<i64>,
    pub bytes: i64,
    pub referer: Option<String>,
    pub user_agent: Option<String>,
    pub request_time: Option<f64>,
    pub extra: Option<String>,
    pub raw: Option<String>,
}

/// 单个源的解析统计（写入 parse_state 表 + 返回给调用方记日志）
#[derive(Debug, Default)]
pub struct ParseSummary {
    pub lines_read: u64,
    pub lines_failed: u64,
    pub bytes_consumed: u64,
    pub last_error: Option<String>,
}
