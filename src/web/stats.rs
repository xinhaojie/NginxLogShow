// src/web/stats.rs
//! 统计查询 SQL（只读，供 handler 在 spawn_blocking 中调用）

use rusqlite::{Connection, ToSql};
use rusqlite::types::Value;

/// 预设时间范围
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Range {
    /// 起始时刻距现在的秒数
    pub back_secs: i64,
    /// 时序聚合桶大小（秒）
    pub bucket_secs: i64,
}

/// 解析范围参数：1h / 6h / 24h / 7d / 30d（非法回退 24h）
pub fn parse_range(s: &str) -> Range {
    match s {
        "1h" => Range { back_secs: 3600, bucket_secs: 60 },
        "6h" => Range { back_secs: 6 * 3600, bucket_secs: 300 },
        "24h" => Range { back_secs: 24 * 3600, bucket_secs: 900 },
        "7d" => Range { back_secs: 7 * 86400, bucket_secs: 3 * 3600 },
        "30d" => Range { back_secs: 30 * 86400, bucket_secs: 12 * 3600 },
        _ => Range { back_secs: 24 * 3600, bucket_secs: 900 },
    }
}

/// KPI 概览
#[derive(Debug, serde::Serialize)]
pub struct Kpi {
    pub pv: i64,
    pub uv: i64,
    pub bytes: i64,
    pub errors: i64,
    pub avg_request_time: Option<f64>,
    pub p95_request_time: Option<f64>,
}

/// 状态码分布项
#[derive(Debug, serde::Serialize)]
pub struct StatusItem {
    pub status: i64,
    pub count: i64,
}

/// Top 项（URI / IP 通用）
#[derive(Debug, serde::Serialize)]
pub struct TopItem {
    pub key: String,
    pub count: i64,
    pub bytes: i64,
}

/// 时序桶
#[derive(Debug, serde::Serialize)]
pub struct SeriesPoint {
    /// 桶起始时间（unix 秒）
    pub ts: i64,
    pub pv: i64,
    pub errors: i64,
    pub bytes: i64,
}

/// 最近错误日志
#[derive(Debug, serde::Serialize)]
pub struct ErrorRow {
    pub ts: i64,
    pub remote_addr: Option<String>,
    pub method: Option<String>,
    pub path: Option<String>,
    pub query: Option<String>,
    pub status: Option<i64>,
    pub user_agent: Option<String>,
}

/// 完整概览数据
#[derive(Debug, serde::Serialize)]
pub struct Overview {
    pub start_ts: i64,
    pub bucket_secs: i64,
    pub kpi: Kpi,
    pub status_dist: Vec<StatusItem>,
    pub top_paths: Vec<TopItem>,
    pub top_ips: Vec<TopItem>,
    pub series: Vec<SeriesPoint>,
    pub recent_errors: Vec<ErrorRow>,
}

/// 查询完整概览
///
/// `source_ids` 为 Some 时只统计这些来源（站点隔离：站点 → 其下全部 source_id）；
/// None 表示不过滤（全部数据，含未分组来源）
pub fn query_overview(
    db: &Connection,
    range: &Range,
    source_ids: Option<&[i64]>,
) -> Result<Overview, rusqlite::Error> {
    let now = chrono::Utc::now().timestamp();
    let start_ts = now - range.back_secs;
    let bucket_secs = range.bucket_secs;

    // 动态条件：时间范围 + 可选来源集合过滤
    let mut conds: Vec<String> = vec!["ts >= ?1".to_string()];
    let mut params: Vec<Value> = vec![Value::from(start_ts)];
    if let Some(ids) = source_ids {
        if ids.is_empty() {
            // 站点下没有任何来源：直接返回空结果
            return Ok(empty_overview(start_ts, bucket_secs));
        }
        let placeholders: Vec<String> = ids
            .iter()
            .map(|id| {
                params.push(Value::from(*id));
                format!("?{}", params.len())
            })
            .collect();
        conds.push(format!("source_id IN ({})", placeholders.join(",")));
    }
    let where_clause = conds.join(" AND ");

    // ---- KPI ----
    let kpi_sql = format!(
        "SELECT COUNT(*),
                COUNT(DISTINCT remote_addr),
                COALESCE(SUM(bytes), 0),
                COALESCE(SUM(CASE WHEN status >= 400 THEN 1 ELSE 0 END), 0),
                AVG(request_time)
         FROM access_logs WHERE {where_clause}"
    );
    let param_refs: Vec<&dyn ToSql> = params.iter().map(|p| p as &dyn ToSql).collect();
    let (pv, uv, bytes, errors, avg_rt): (i64, i64, i64, i64, Option<f64>) = db.query_row(
        &kpi_sql,
        param_refs.as_slice(),
        |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            ))
        },
    )?;

    // P95 响应时间：在「有 request_time 的行」中取第 95 分位
    let p95_rt: Option<f64> = if pv == 0 {
        None
    } else {
        let cnt_sql = format!(
            "SELECT COUNT(*) FROM access_logs WHERE {where_clause} AND request_time IS NOT NULL"
        );
        let cnt: i64 = db
            .query_row(&cnt_sql, param_refs.as_slice(), |row| row.get(0))
            .unwrap_or(0);
        if cnt == 0 {
            None
        } else {
            let offset = (((cnt as f64) * 0.95).floor() as i64).clamp(0, cnt - 1);
            let p95_sql = format!(
                "SELECT request_time FROM access_logs
                 WHERE {where_clause} AND request_time IS NOT NULL
                 ORDER BY request_time LIMIT 1 OFFSET ?{}",
                params.len() + 1
            );
            let mut params2 = params.clone();
            params2.push(Value::from(offset));
            let refs2: Vec<&dyn ToSql> = params2.iter().map(|p| p as &dyn ToSql).collect();
            db.query_row(&p95_sql, refs2.as_slice(), |row| row.get(0))
                .ok()
        }
    };

    // ---- 状态码分布 ----
    let status_sql = format!(
        "SELECT COALESCE(status, 0) AS st, COUNT(*) AS cnt
         FROM access_logs WHERE {where_clause}
         GROUP BY st ORDER BY cnt DESC"
    );
    let mut stmt = db.prepare(&status_sql)?;
    let status_dist = stmt
        .query_map(param_refs.as_slice(), |row| {
            Ok(StatusItem { status: row.get(0)?, count: row.get(1)? })
        })?
        .collect::<Result<Vec<_>, _>>()?;

    // ---- Top 路径 ----
    let top_paths_sql = format!(
        "SELECT COALESCE(method || ' ' || path, path, '(未知)') AS k,
                COUNT(*) AS cnt, COALESCE(SUM(bytes), 0) AS b
         FROM access_logs WHERE {where_clause} AND path IS NOT NULL
         GROUP BY k ORDER BY cnt DESC LIMIT 10"
    );
    let mut stmt = db.prepare(&top_paths_sql)?;
    let top_paths = stmt
        .query_map(param_refs.as_slice(), |row| {
            Ok(TopItem { key: row.get(0)?, count: row.get(1)?, bytes: row.get(2)? })
        })?
        .collect::<Result<Vec<_>, _>>()?;

    // ---- Top IP ----
    let top_ips_sql = format!(
        "SELECT COALESCE(remote_addr, '(未知)') AS k,
                COUNT(*) AS cnt, COALESCE(SUM(bytes), 0) AS b
         FROM access_logs WHERE {where_clause}
         GROUP BY k ORDER BY cnt DESC LIMIT 10"
    );
    let mut stmt = db.prepare(&top_ips_sql)?;
    let top_ips = stmt
        .query_map(param_refs.as_slice(), |row| {
            Ok(TopItem { key: row.get(0)?, count: row.get(1)?, bytes: row.get(2)? })
        })?
        .collect::<Result<Vec<_>, _>>()?;

    // ---- 时序（请求量 / 错误 / 流量）----
    let series_sql = format!(
        "SELECT (ts / {bucket_secs}) * {bucket_secs} AS bucket,
                COUNT(*) AS pv,
                COALESCE(SUM(CASE WHEN status >= 400 THEN 1 ELSE 0 END), 0) AS err,
                COALESCE(SUM(bytes), 0) AS b
         FROM access_logs WHERE {where_clause}
         GROUP BY bucket ORDER BY bucket"
    );
    let mut stmt = db.prepare(&series_sql)?;
    let series = stmt
        .query_map(param_refs.as_slice(), |row| {
            Ok(SeriesPoint {
                ts: row.get(0)?,
                pv: row.get(1)?,
                errors: row.get(2)?,
                bytes: row.get(3)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;

    // ---- 最近错误日志（最多 50 条）----
    let err_sql = format!(
        "SELECT ts, remote_addr, method, path, query, status, user_agent
         FROM access_logs
         WHERE {where_clause} AND status >= 400
         ORDER BY ts DESC LIMIT 50"
    );
    let mut stmt = db.prepare(&err_sql)?;
    let recent_errors = stmt
        .query_map(param_refs.as_slice(), |row| {
            Ok(ErrorRow {
                ts: row.get(0)?,
                remote_addr: row.get(1)?,
                method: row.get(2)?,
                path: row.get(3)?,
                query: row.get(4)?,
                status: row.get(5)?,
                user_agent: row.get(6)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;

    Ok(Overview {
        start_ts,
        bucket_secs,
        kpi: Kpi {
            pv,
            uv,
            bytes,
            errors,
            avg_request_time: avg_rt,
            p95_request_time: p95_rt,
        },
        status_dist,
        top_paths,
        top_ips,
        series,
        recent_errors,
    })
}

/// 删除指定时间之前的日志，返回删除条数
pub fn delete_before(db: &Connection, ts: i64) -> Result<i64, rusqlite::Error> {
    db.execute("DELETE FROM access_logs WHERE ts < ?1", rusqlite::params![ts])
        .map(|n| n as i64)
}

/// 数据库页统计：(总页数, 空闲页数)
pub fn db_freelist(db: &Connection) -> Result<(i64, i64), rusqlite::Error> {
    let page_count: i64 = db.query_row("PRAGMA page_count", [], |row| row.get(0))?;
    let freelist_count: i64 = db.query_row("PRAGMA freelist_count", [], |row| row.get(0))?;
    Ok((page_count, freelist_count))
}

/// VACUUM 整理碎片
pub fn vacuum(db: &Connection) -> Result<(), rusqlite::Error> {
    db.execute_batch("VACUUM")
}

/// 空概览（站点下无来源时直接返回，避免拼出空 IN 列表）
fn empty_overview(start_ts: i64, bucket_secs: i64) -> Overview {
    Overview {
        start_ts,
        bucket_secs,
        kpi: Kpi {
            pv: 0,
            uv: 0,
            bytes: 0,
            errors: 0,
            avg_request_time: None,
            p95_request_time: None,
        },
        status_dist: Vec::new(),
        top_paths: Vec::new(),
        top_ips: Vec::new(),
        series: Vec::new(),
        recent_errors: Vec::new(),
    }
}

// ============================================================
// 访问明细查询（分页）
// ============================================================

/// 明细查询条件
#[derive(Debug, Default)]
pub struct LogsFilter {
    /// 起始时间（unix 秒）
    pub start_ts: i64,
    /// 结束时间（unix 秒，可选；None = 至今）
    pub end_ts: Option<i64>,
    /// 来源集合过滤（站点隔离），None = 不过滤
    pub source_ids: Option<Vec<i64>>,
    /// 状态码过滤：0=全部，4xx/5xx 用 -400 表示 >=400；也可以直接指定精确状态码
    pub status: Option<i64>,
    /// 路径/IP 模糊搜索
    pub q: Option<String>,
    /// 分页
    pub page: i64,
    pub page_size: i64,
}

/// 明细行
#[derive(Debug, serde::Serialize)]
pub struct LogRow {
    pub id: i64,
    pub ts: i64,
    pub source_id: i64,
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
}

/// 明细查询结果：行 + 总数
pub struct LogsPage {
    pub rows: Vec<LogRow>,
    pub total: i64,
}

/// 分页查询访问明细（按时间倒序）
pub fn query_logs(db: &Connection, f: &LogsFilter) -> Result<LogsPage, rusqlite::Error> {
    let mut conds: Vec<String> = vec!["ts >= ?1".to_string()];
    let mut params: Vec<Value> = vec![Value::from(f.start_ts)];

    // 自定义结束时间
    if let Some(end) = f.end_ts {
        conds.push(format!("ts <= ?{}", params.len() + 1));
        params.push(Value::from(end));
    }

    if let Some(ids) = &f.source_ids {
        if ids.is_empty() {
            return Ok(LogsPage { rows: Vec::new(), total: 0 });
        }
        let placeholders: Vec<String> = ids
            .iter()
            .map(|id| {
                params.push(Value::from(*id));
                format!("?{}", params.len())
            })
            .collect();
        conds.push(format!("source_id IN ({})", placeholders.join(",")));
    }

    // 状态码过滤：-400 表示「仅错误」（status >= 400），其余为精确匹配
    if let Some(st) = f.status {
        if st == -400 {
            conds.push("status >= 400".to_string());
        } else {
            conds.push(format!("status = ?{}", params.len() + 1));
            params.push(Value::from(st));
        }
    }

    // 模糊搜索：路径 / IP / UA
    if let Some(q) = f.q.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        conds.push(format!(
            "(path LIKE ?{} OR remote_addr LIKE ?{} OR user_agent LIKE ?{})",
            params.len() + 1,
            params.len() + 1,
            params.len() + 1
        ));
        params.push(Value::from(format!("%{}%", q)));
    }

    let where_clause = conds.join(" AND ");
    let refs: Vec<&dyn ToSql> = params.iter().map(|p| p as &dyn ToSql).collect();

    let total: i64 = db.query_row(
        &format!("SELECT COUNT(*) FROM access_logs WHERE {where_clause}"),
        refs.as_slice(),
        |row| row.get(0),
    )?;

    let page_size = f.page_size.clamp(1, 200);
    let page = f.page.max(1);
    let offset = (page - 1) * page_size;
    params.push(Value::from(page_size));
    params.push(Value::from(offset));
    let refs2: Vec<&dyn ToSql> = params.iter().map(|p| p as &dyn ToSql).collect();

    let rows_sql = format!(
        "SELECT id, ts, source_id, remote_addr, method, path, query, protocol,
                status, bytes, referer, user_agent, request_time
         FROM access_logs WHERE {where_clause}
         ORDER BY ts DESC, id DESC LIMIT ?{} OFFSET ?{}",
        params.len() - 1,
        params.len()
    );
    let mut stmt = db.prepare(&rows_sql)?;
    let rows = stmt
        .query_map(refs2.as_slice(), |row| {
            Ok(LogRow {
                id: row.get(0)?,
                ts: row.get(1)?,
                source_id: row.get(2)?,
                remote_addr: row.get(3)?,
                method: row.get(4)?,
                path: row.get(5)?,
                query: row.get(6)?,
                protocol: row.get(7)?,
                status: row.get(8)?,
                bytes: row.get(9)?,
                referer: row.get(10)?,
                user_agent: row.get(11)?,
                request_time: row.get(12)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;

    Ok(LogsPage { rows, total })
}
