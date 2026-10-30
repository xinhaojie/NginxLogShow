// src/web/handler.rs
//! Web handler：页面、统计、日志源配置管理、清理

use std::io::{BufRead, Seek, SeekFrom};
use std::sync::Arc;

use axum::{
    extract::{Query, State},
    response::Html,
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::error::{AppError, AppResult};
use crate::nginx::models::{LogSource, NginxSourcesConfig};
use crate::nginx::parser;
use crate::state::{load_sources_config, AppState};

use super::ip_geo;
use super::stats;

// ============================================================
// GET / — Web 仪表盘页面
// ============================================================

pub async fn web_page() -> Html<&'static str> {
    Html(include_str!("../../static/index.html"))
}

// ============================================================
// GET /api/stats/overview?range=24h&source_id=1 — 统计概览
// ============================================================

#[derive(Debug, Deserialize)]
pub struct OverviewReq {
    /// 时间范围：1h / 6h / 24h / 7d / 30d，默认 24h
    pub range: Option<String>,
    /// 可选：只统计指定站点（站点 → 其下全部来源）
    pub site_id: Option<i64>,
    /// 可选：只统计指定日志源（优先级高于 site_id）
    pub source_id: Option<i64>,
}

pub async fn stats_overview(
    State(state): State<Arc<AppState>>,
    Query(req): Query<OverviewReq>,
) -> AppResult<Json<serde_json::Value>> {
    let range = stats::parse_range(req.range.as_deref().unwrap_or("24h"));
    let read_db = state.read_db.clone();

    let overview = tokio::task::spawn_blocking(move || {
        let conn = read_db.lock().expect("读锁获取失败");

        // 解析过滤条件：source_id 显式指定 > site_id 归属集合 > 不过滤
        let source_ids: Option<Vec<i64>> = if let Some(sid) = req.source_id {
            Some(vec![sid])
        } else if let Some(site) = req.site_id {
            let config = load_sources_config(&conn);
            let ids: Vec<i64> = config
                .sources
                .iter()
                .filter(|s| s.site_id == site)
                .map(|s| s.id)
                .collect();
            Some(ids)
        } else {
            None
        };

        stats::query_overview(&conn, &range, source_ids.as_deref())
    })
    .await
    .map_err(|e| AppError::internal(format!("任务调度异常: {}", e)))??;

    Ok(Json(json!({
        "code": 0,
        "message": "success",
        "data": overview,
    })))
}

// ============================================================
// GET /api/logs — 访问明细（分页，支持站点/状态/搜索过滤）
// ============================================================

#[derive(Debug, Deserialize)]
pub struct LogsReq {
    /// 时间范围：1h / 6h / 24h / 7d / 30d，默认 24h（提供 start_ts 后忽略）
    pub range: Option<String>,
    /// 自定义起始时间（unix 秒，可选，优先于 range）
    pub start_ts: Option<i64>,
    /// 自定义结束时间（unix 秒，可选）
    pub end_ts: Option<i64>,
    /// 可选：站点过滤
    pub site_id: Option<i64>,
    /// 可选：来源过滤
    pub source_id: Option<i64>,
    /// 可选：-400 = 仅错误（>=400），或精确状态码
    pub status: Option<i64>,
    /// 可选：路径/IP/UA 模糊搜索
    pub q: Option<String>,
    /// 页码（从 1 开始）
    pub page: Option<i64>,
    /// 每页条数（默认 50，最大 200）
    pub page_size: Option<i64>,
}

pub async fn logs_query(
    State(state): State<Arc<AppState>>,
    Query(req): Query<LogsReq>,
) -> AppResult<Json<serde_json::Value>> {
    let range = stats::parse_range(req.range.as_deref().unwrap_or("24h"));
    let read_db = state.read_db.clone();

    let page = tokio::task::spawn_blocking(move || {
        let conn = read_db.lock().expect("读锁获取失败");

        let source_ids: Option<Vec<i64>> = if let Some(sid) = req.source_id {
            Some(vec![sid])
        } else if let Some(site) = req.site_id {
            let config = load_sources_config(&conn);
            let ids: Vec<i64> = config
                .sources
                .iter()
                .filter(|s| s.site_id == site)
                .map(|s| s.id)
                .collect();
            Some(ids)
        } else {
            None
        };

        let now = chrono::Utc::now().timestamp();
        let filter = stats::LogsFilter {
            start_ts: req.start_ts.unwrap_or(now - range.back_secs),
            end_ts: req.end_ts,
            source_ids,
            status: req.status,
            q: req.q,
            page: req.page.unwrap_or(1),
            page_size: req.page_size.unwrap_or(50),
        };
        stats::query_logs(&conn, &filter)
    })
    .await
    .map_err(|e| AppError::internal(format!("任务调度异常: {}", e)))?
    .map_err(|e| AppError::internal(e.to_string()))?;

    Ok(Json(json!({
        "code": 0,
        "message": "success",
        "data": {
            "rows": page.rows,
            "total": page.total,
        },
    })))
}

// ============================================================
// GET /api/ipgeo?ips=1.2.3.4,5.6.7.8 — IP 归属地批量查询（10 天缓存）
// ============================================================

#[derive(Debug, Deserialize)]
pub struct IpGeoReq {
    /// 逗号分隔的 IP 列表（最多 200 个）
    pub ips: String,
}

pub async fn ipgeo_query(
    State(state): State<Arc<AppState>>,
    Query(req): Query<IpGeoReq>,
) -> AppResult<Json<serde_json::Value>> {
    let ips: Vec<String> = req
        .ips
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .take(200)
        .map(String::from)
        .collect();

    let read_db = state.read_db.clone();
    let geo = tokio::task::spawn_blocking(move || {
        let conn = read_db.lock().expect("读锁获取失败");
        ip_geo::lookup_geo(&conn, &ips)
    })
    .await
    .map_err(|e| AppError::internal(format!("任务调度异常: {}", e)))?;

    // 以 IP 为 key 的 map，方便前端直接取
    let map: serde_json::Map<String, serde_json::Value> = geo
        .into_iter()
        .map(|g| {
            (
                g.ip,
                json!({
                    "country": g.country,
                    "region": g.region,
                    "city": g.city,
                    "isp": g.isp,
                }),
            )
        })
        .collect();

    Ok(Json(json!({
        "code": 0,
        "message": "success",
        "data": { "map": map },
    })))
}

// ============================================================
// GET /api/sources — 日志源配置 + 解析状态
// ============================================================

/// parse_state 表的运行状态
#[derive(Debug, Serialize)]
pub struct SourceState {
    pub source_id: i64,
    pub offset: i64,
    pub last_parse_at: Option<String>,
    pub lines_total: i64,
    pub lines_failed: i64,
    pub last_error: Option<String>,
}

pub async fn get_sources(
    State(state): State<Arc<AppState>>,
) -> AppResult<Json<serde_json::Value>> {
    let read_db = state.read_db.clone();

    let (config, states) = tokio::task::spawn_blocking(move || {
        let conn = read_db.lock().expect("读锁获取失败");
        let config = load_sources_config(&conn);
        let states = query_source_states(&conn)?;
        anyhow::Ok((config, states))
    })
    .await
    .map_err(|e| AppError::internal(format!("任务调度异常: {}", e)))?
    .map_err(|e: anyhow::Error| AppError::internal(e.to_string()))?;

    Ok(Json(json!({
        "code": 0,
        "message": "success",
        "data": {
            "sites": config.sites,
            "sources": config.sources,
            "interval_secs": config.interval_secs,
            "auto_cleanup_enabled": config.auto_cleanup_enabled,
            "auto_cleanup_time": config.auto_cleanup_time,
            "auto_cleanup_days": config.auto_cleanup_days,
            "states": states,
        }
    })))
}

fn query_source_states(
    db: &rusqlite::Connection,
) -> Result<Vec<SourceState>, rusqlite::Error> {
    let mut stmt = db.prepare(
        "SELECT source_id, offset, last_parse_at, lines_total, lines_failed, last_error
         FROM parse_state ORDER BY source_id",
    )?;
    let rows = stmt
        .query_map([], |row| {
            Ok(SourceState {
                source_id: row.get(0)?,
                offset: row.get(1)?,
                last_parse_at: row.get(2)?,
                lines_total: row.get(3)?,
                lines_failed: row.get(4)?,
                last_error: row.get(5)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

// ============================================================
// POST /api/sources — 保存日志源配置（热生效）
// ============================================================

#[derive(Debug, Deserialize)]
pub struct SaveSourcesReq {
    /// 站点列表（可选；不传表示不修改站点列表——兼容旧调用，但推荐显式传全量）
    #[serde(default)]
    pub sites: Option<Vec<crate::nginx::models::Site>>,
    #[serde(default)]
    pub sources: Vec<LogSource>,
    pub interval_secs: Option<u64>,
    /// 自动清理：是否启用
    #[serde(default)]
    pub auto_cleanup_enabled: Option<bool>,
    /// 自动清理执行时刻（本地时间 HH:MM）
    #[serde(default)]
    pub auto_cleanup_time: Option<String>,
    /// 自动清理：保留最近 N 天
    #[serde(default)]
    pub auto_cleanup_days: Option<u32>,
}

/// 校验并规范化配置；失败返回错误信息
fn validate_config(req: &mut SaveSourcesReq) -> Result<(), String> {
    // ---- 站点校验与 ID 分配 ----
    let mut sites = req.sites.take().unwrap_or_default();
    // 已存在的最大正 ID（新站点从它之后开始分配，占位负值不参与计数）
    let mut next_site_id = sites
        .iter()
        .map(|s| s.id)
        .filter(|id| *id > 0)
        .max()
        .unwrap_or(0);
    for site in &mut sites {
        site.name = site.name.trim().to_string();
        site.note = site.note.trim().to_string();
        if site.name.is_empty() {
            return Err("站点名称不能为空".to_string());
        }
        if site.name.chars().count() > 100 {
            return Err("站点名称过长（>100 字符）".to_string());
        }
        if site.id <= 0 {
            next_site_id += 1;
            site.id = next_site_id;
        }
    }
    // 站点 ID 重复检查 + 名称重复检查
    let mut site_ids: Vec<i64> = sites.iter().map(|s| s.id).collect();
    site_ids.sort();
    if site_ids.windows(2).any(|w| w[0] == w[1]) {
        return Err("存在重复的站点 ID".to_string());
    }
    let mut site_names: Vec<&str> = sites.iter().map(|s| s.name.as_str()).collect();
    site_names.sort_unstable();
    if site_names.windows(2).any(|w| w[0].eq_ignore_ascii_case(w[1])) {
        return Err("存在重名的站点".to_string());
    }
    let valid_site_ids: Vec<i64> = site_ids;

    // ---- 日志源校验与 ID 分配 ----
    // 已存在的最大正 ID（新源从它之后开始分配）
    let mut next_id = req
        .sources
        .iter()
        .map(|s| s.id)
        .filter(|id| *id > 0)
        .max()
        .unwrap_or(0);
    for src in &mut req.sources {
        src.name = src.name.trim().to_string();
        src.path = src.path.trim().to_string();
        src.format = src.format.trim().to_string();

        if src.path.is_empty() {
            return Err(format!("日志源「{}」的路径不能为空", src.name));
        }
        if src.path.len() > 500 {
            return Err("日志路径过长（>500 字符）".to_string());
        }
        // 非空 format 必须能编译（"error" 是内置 error.log 预设，直接通过）
        if !src.format.is_empty() {
            parser::resolve_format(&src.format)
                .map(|_| ())
                .map_err(|e| format!("日志源「{}」格式错误: {}", src.name, e))?;
        }
        // 站点归属必须存在（0 = 未分组允许）
        if src.site_id != 0 && !valid_site_ids.contains(&src.site_id) {
            return Err(format!(
                "日志源「{}」归属的站点不存在（site_id={}）",
                if src.name.is_empty() { &src.path } else { &src.name },
                src.site_id
            ));
        }
        // 分配 ID
        if src.id <= 0 {
            next_id += 1;
            src.id = next_id;
        }
    }

    // 同一 ID 重复检查
    let mut ids: Vec<i64> = req.sources.iter().map(|s| s.id).collect();
    ids.sort();
    if ids.windows(2).any(|w| w[0] == w[1]) {
        return Err("存在重复的日志源 ID".to_string());
    }

    // 同一日志文件路径只能作为一个日志源（重复添加会对同一文件重复解析、数据翻倍）
    let mut paths: Vec<&str> = req.sources.iter().map(|s| s.path.as_str()).collect();
    paths.sort_unstable();
    if let Some(dup) = paths.windows(2).find(|w| w[0] == w[1]) {
        return Err(format!(
            "日志路径重复：「{}」已被其它日志源使用，同一文件不能添加两次",
            dup[0]
        ));
    }

    let interval = req.interval_secs.unwrap_or(60).clamp(5, 86400);
    req.interval_secs = Some(interval);

    // ---- 自动清理配置校验 ----
    if let Some(t) = req.auto_cleanup_time.as_deref() {
        if parse_hhmm(t).is_none() {
            return Err(format!("自动清理时间格式错误：「{}」，需要 HH:MM（如 04:00）", t));
        }
    }
    if let Some(d) = req.auto_cleanup_days {
        if !(1..=3650).contains(&d) {
            return Err("自动清理保留天数需在 1-3650 之间".to_string());
        }
    }

    req.sites = Some(sites);
    Ok(())
}

/// 校验 HH:MM 格式（如 "04:30"、"23:59"），返回 (时, 分)
fn parse_hhmm(s: &str) -> Option<(u32, u32)> {
    let (h, m) = s.split_once(':')?;
    let h: u32 = h.trim().parse().ok()?;
    let m: u32 = m.trim().parse().ok()?;
    if h < 24 && m < 60 {
        Some((h, m))
    } else {
        None
    }
}

pub async fn save_sources(
    State(state): State<Arc<AppState>>,
    Json(mut req): Json<SaveSourcesReq>,
) -> AppResult<Json<serde_json::Value>> {
    validate_config(&mut req)
        .map_err(|e| AppError::bad_request(e))?;

    // 未提交的字段沿用当前生效配置，避免「只改站点/日志源」的保存把自动清理等设置重置为默认
    let prev = state.config_tx.borrow().clone();

    let interval_secs = req.interval_secs.unwrap_or(prev.interval_secs);
    let config = NginxSourcesConfig {
        sites: req.sites.unwrap_or_default(),
        sources: req.sources,
        interval_secs,
        auto_cleanup_enabled: req
            .auto_cleanup_enabled
            .unwrap_or(prev.auto_cleanup_enabled),
        auto_cleanup_time: req
            .auto_cleanup_time
            .unwrap_or(prev.auto_cleanup_time),
        auto_cleanup_days: req.auto_cleanup_days.unwrap_or(prev.auto_cleanup_days),
    };

    let json = serde_json::to_string(&config)
        .map_err(|e| AppError::internal(format!("配置序列化失败: {}", e)))?;

    let read_db = state.read_db.clone();
    let now = chrono::Utc::now().to_rfc3339();
    // 已删除来源的解析进度清理 SQL（闭包外先算好，config 还要用于热生效通知）
    let cleanup_sql = {
        let kept_ids: Vec<String> = config.sources.iter().map(|s| s.id.to_string()).collect();
        if kept_ids.is_empty() {
            "DELETE FROM parse_state".to_string()
        } else {
            format!(
                "DELETE FROM parse_state WHERE source_id NOT IN ({})",
                kept_ids.join(",")
            )
        }
    };
    // 已删除来源的孤儿日志数据同样清理（否则残留行永远查不到也删不掉）
    let cleanup_logs_sql = {
        let kept_ids: Vec<String> = config.sources.iter().map(|s| s.id.to_string()).collect();
        if kept_ids.is_empty() {
            "DELETE FROM access_logs".to_string()
        } else {
            format!(
                "DELETE FROM access_logs WHERE source_id NOT IN ({})",
                kept_ids.join(",")
            )
        }
    };

    tokio::task::spawn_blocking(move || {
        let conn = read_db.lock().expect("读锁获取失败");
        conn.execute(
            "INSERT INTO settings (key, value, updated_at) VALUES ('nginx_sources', ?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
            rusqlite::params![json, now],
        )?;
        // 清理已删除来源的解析进度与孤儿日志数据（避免残留孤儿状态）
        conn.execute(&cleanup_sql, [])?;
        conn.execute(&cleanup_logs_sql, [])?;
        Ok::<(), rusqlite::Error>(())
    })
    .await
    .map_err(|e| AppError::internal(format!("任务调度异常: {}", e)))?
    .map_err(|e| AppError::from(e))?;

    // 热生效：通知后台解析任务
    log::info!(
        "日志源配置已更新（网页保存，热生效）：{} 个站点 / {} 个源，解析间隔 {}s，自动清理 {}",
        config.sites.len(),
        config.sources.len(),
        interval_secs,
        if config.auto_cleanup_enabled {
            format!("已启用（每天 {} 删除 {} 天前日志）", config.auto_cleanup_time, config.auto_cleanup_days)
        } else {
            "未启用".to_string()
        }
    );
    state.config_tx.send_replace(config);

    Ok(Json(json!({"code": 0, "message": "success"})))
}

// ============================================================
// POST /api/sources/test — 测试解析（验证 log_format）
// ============================================================

#[derive(Debug, Deserialize)]
pub struct TestParseReq {
    /// 日志文件路径
    pub path: String,
    /// 日志格式：空 = 自动检测；"error" = nginx error.log；其它 = log_format 字符串
    #[serde(default)]
    pub format: String,
    /// 最多读取的行数，默认 50
    pub max_lines: Option<u32>,
}

#[derive(Debug, Serialize)]
pub struct TestParseResp {
    pub code: i32,
    pub message: String,
    /// 读取的总行数
    pub total_lines: u64,
    /// 解析成功行数
    pub matched: u64,
    /// 解析失败样例（最多 3 行）
    pub failed_samples: Vec<String>,
    /// 解析成功样例（最多 3 条，JSON 展示）
    pub parsed_samples: Vec<serde_json::Value>,
}

pub async fn test_parse(
    Json(req): Json<TestParseReq>,
) -> AppResult<Json<TestParseResp>> {
    let path = req.path.trim().to_string();
    if path.is_empty() {
        return Err(AppError::bad_request("路径不能为空"));
    }
    let format = req.format.trim().to_string();
    let max_lines = req.max_lines.unwrap_or(50).clamp(1, 500) as usize;

    // 解析格式（空 = 自动检测，"error" = error.log，其它 = log_format）
    let mode = parser::resolve_format(&format)
        .map_err(|e| AppError::bad_request(format!("log_format 错误: {}", e)))?;

    let result = tokio::task::spawn_blocking(move || test_parse_blocking(&path, &mode, max_lines))
        .await
        .map_err(|e| AppError::internal(format!("任务调度异常: {}", e)))?;

    Ok(Json(result))
}

/// 读取文件末尾 256KB，逐行试解析，返回成功率与样例
fn test_parse_blocking(path: &str, mode: &parser::FormatMode, max_lines: usize) -> TestParseResp {
    let mut resp = TestParseResp {
        code: 1,
        message: String::new(),
        total_lines: 0,
        matched: 0,
        failed_samples: Vec::new(),
        parsed_samples: Vec::new(),
    };

    let mut file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) => {
            resp.message = format!("打开文件失败: {}", e);
            return resp;
        }
    };

    // 从末尾 256KB 处开始读（不足则从头）
    let size = file.metadata().map(|m| m.len()).unwrap_or(0);
    let start = size.saturating_sub(256 * 1024);
    if file.seek(SeekFrom::Start(start)).is_err() {
        resp.message = "文件定位失败".to_string();
        return resp;
    }
    let mut reader = std::io::BufReader::new(file);
    if start > 0 {
        let mut discard = String::new();
        let _ = reader.read_line(&mut discard); // 跳过半行
    }

    let mut buf = Vec::with_capacity(1024);
    loop {
        buf.clear();
        match reader.read_until(b'\n', &mut buf) {
            Ok(0) => break,
            Ok(_) => {}
            Err(e) => {
                resp.message = format!("读取失败: {}", e);
                return resp;
            }
        }
        let line = String::from_utf8_lossy(&buf);
        let line = line.trim_end_matches(['\n', '\r']);
        if line.is_empty() {
            continue;
        }
        resp.total_lines += 1;

        match parser::parse_line_mode(line, mode) {
            Some(p) => {
                resp.matched += 1;
                if resp.parsed_samples.len() < 3 {
                    resp.parsed_samples.push(json!({
                        "ts": p.ts,
                        "remote_addr": p.remote_addr,
                        "method": p.method,
                        "path": p.path,
                        "query": p.query,
                        "status": p.status,
                        "bytes": p.bytes,
                        "request_time": p.request_time,
                        "extra": p.extra,
                    }));
                }
            }
            None => {
                if resp.failed_samples.len() < 3 {
                    resp.failed_samples.push(line.chars().take(200).collect());
                }
            }
        }

        if resp.total_lines >= max_lines as u64 {
            break;
        }
    }

    if resp.total_lines == 0 {
        resp.message = "未读到任何日志行（文件为空或全部是半行）".to_string();
    } else if resp.matched == 0 {
        resp.message = "全部行解析失败：请检查 log_format 是否与日志实际格式一致".to_string();
    } else {
        resp.code = 0;
        resp.message = format!("{}/{} 行解析成功", resp.matched, resp.total_lines);
    }
    resp
}

// ============================================================
// POST /api/sources/{id}/reparse — 手动重解析单个源
// ============================================================

/// 清除该源已入库的历史数据 + 重置解析进度，并通知后台任务立即从头解析整个文件
pub async fn reparse_source(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(id): axum::extract::Path<i64>,
) -> AppResult<Json<serde_json::Value>> {
    if id <= 0 {
        return Err(AppError::bad_request("无效的日志源 ID"));
    }

    let read_db = state.read_db.clone();
    let deleted = tokio::task::spawn_blocking(move || -> Result<i64, rusqlite::Error> {
        let conn = read_db.lock().expect("读锁获取失败");
        let n = conn.execute(
            "DELETE FROM access_logs WHERE source_id = ?1",
            rusqlite::params![id],
        )?;
        // 清空解析进度：offset 归零，后台从头解析
        conn.execute(
            "DELETE FROM parse_state WHERE source_id = ?1",
            rusqlite::params![id],
        )?;
        Ok(n as i64)
    })
    .await
    .map_err(|e| AppError::internal(format!("任务调度异常: {}", e)))?
    .map_err(|e| AppError::internal(e.to_string()))?;

    // 通知后台解析任务立即从头解析该源（mpsc 不会合并丢失多次请求）
    let _ = state.reparse_tx.send(id);
    log::info!("手动重解析源 {}：已清除 {} 条历史数据", id, deleted);

    Ok(Json(json!({
        "code": 0,
        "message": "success",
        "deleted": deleted,
    })))
}

// ============================================================
// DELETE /api/logs?days=30 — 删除 N 天前的日志
// ============================================================

#[derive(Debug, Deserialize)]
pub struct DeleteLogsReq {
    /// 保留最近 N 天的日志，删除更早的
    pub days: i64,
}

pub async fn delete_logs_before(
    State(state): State<Arc<AppState>>,
    Query(req): Query<DeleteLogsReq>,
) -> AppResult<Json<serde_json::Value>> {
    if !(1..=3650).contains(&req.days) {
        return Err(AppError::bad_request("days 需在 1-3650 之间"));
    }
    let cutoff = chrono::Utc::now().timestamp() - req.days * 86400;
    let read_db = state.read_db.clone();
    let vacuum_enabled = state.app_config.sqlite.vacuum_after_delete;
    let vacuum_min_deleted = state.app_config.sqlite.vacuum_min_deleted;

    let deleted = tokio::task::spawn_blocking(move || -> Result<i64, rusqlite::Error> {
        let conn = read_db.lock().expect("读锁获取失败");
        let n = stats::delete_before(&conn, cutoff)?;

        // 删除达到阈值后自动 VACUUM 整理碎片
        if vacuum_enabled && n >= vacuum_min_deleted {
            let (_, free_before) = stats::db_freelist(&conn).unwrap_or((0, 0));
            match stats::vacuum(&conn) {
                Ok(()) => {
                    let (_, free_after) = stats::db_freelist(&conn).unwrap_or((0, 0));
                    log::info!("VACUUM 完成：freelist {} → {}", free_before, free_after);
                }
                Err(e) => log::warn!("VACUUM 失败（不影响删除结果）: {}", e),
            }
        }
        Ok(n)
    })
    .await
    .map_err(|e| AppError::internal(format!("任务调度异常: {}", e)))??;

    log::info!("删除 {} 天前的日志 {} 条（web 清理）", req.days, deleted);

    Ok(Json(json!({"code": 0, "message": "success", "deleted": deleted})))
}

// ============================================================
// GET /api/db/size — 数据库文件大小
// ============================================================

pub async fn db_size(State(state): State<Arc<AppState>>) -> AppResult<Json<serde_json::Value>> {
    let path = state.app_config.sqlite.path.clone();

    let (db_bytes, wal_bytes) = tokio::task::spawn_blocking(move || {
        let db_bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        let wal_path = format!("{path}-wal");
        let wal_bytes = std::fs::metadata(&wal_path).map(|m| m.len()).unwrap_or(0);
        (db_bytes, wal_bytes)
    })
    .await
    .map_err(|e| AppError::internal(format!("任务调度异常: {}", e)))?;

    Ok(Json(json!({
        "code": 0,
        "message": "success",
        "db_bytes": db_bytes,
        "wal_bytes": wal_bytes,
        "total_bytes": db_bytes + wal_bytes,
    })))
}
