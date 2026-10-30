// src/nginx/watcher.rs
//! 后台日志解析任务
//!
//! 职责：
//! - 按配置的间隔（默认 60s）对所有启用的日志源做一次增量解析
//! - 增量读取：记录每个源已读到的字节偏移（parse_state 表 + 内存缓存），只解析新增内容
//! - 轮转检测：文件变小（size < offset）视为日志被切割/截断，从头重新读取
//! - 半行处理：文件末尾可能是不完整的行（正在写入），回退到该行起始偏移，下次再读
//! - 配置热生效：web 修改配置后通过 watch channel 通知，下一轮立即用新配置解析
//!
//! 线程模型：独占一个写连接（WAL 模式下与读连接并发不阻塞）

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::time::Duration;

use log::{error, info, warn};
use rusqlite::Connection;
use tokio::sync::watch;

use super::models::{LogSource, NginxSourcesConfig, ParseSummary};
use super::parser;

/// 单个源在内存中的解析进度
#[derive(Debug, Clone, Copy)]
struct SourceState {
    offset: u64,
    initialized: bool,
}

/// 后台解析任务主循环
pub async fn watcher(
    mut write_db: Connection,
    mut config_rx: watch::Receiver<NginxSourcesConfig>,
    mut reparse_rx: tokio::sync::mpsc::UnboundedReceiver<i64>,
    sqlite_cfg: crate::config::SqliteConfig,
) {
    let mut current = config_rx.borrow_and_update().clone();
    info!(
        "nginx 日志解析任务启动：{} 个源，间隔 {}s，自动清理 {}",
        current.sources.len(),
        current.interval_secs,
        if current.auto_cleanup_enabled {
            format!("已启用（每天 {} 删除 {} 天前日志）", current.auto_cleanup_time, current.auto_cleanup_days)
        } else {
            "未启用".to_string()
        }
    );

    // 每个源的解析进度（首次从 parse_state 表加载）
    let mut states: HashMap<i64, SourceState> = HashMap::new();
    // 每个源的累计行数统计（缓存于内存，减少读库）
    let mut counters: HashMap<i64, (u64, u64)> = HashMap::new(); // (lines_total, lines_failed)
    // 自动清理最近一次执行的日期（同一天只执行一次）
    let mut cleanup_last_date: Option<chrono::NaiveDate> = None;
    // WAL 定时 checkpoint 最近一次执行的日期（同一天只执行一次）
    let mut wal_ckpt_last_date: Option<chrono::NaiveDate> = None;

    // 启动时先合并一次 WAL：上一次运行（尤其是异常退出）可能留下很大的 -wal 文件，
    // 此时还没有业务读连接抢占，是截断成功率最高的时机。之后按每天定时逻辑执行。
    if sqlite_cfg.wal_checkpoint_enabled {
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            do_wal_checkpoint(&write_db, &sqlite_cfg, "启动");
        }));
        if let Err(p) = r {
            error!(
                "WAL 启动 checkpoint 发生 panic（已捕获，任务继续运行）: {}",
                panic_msg(&p)
            );
        }
    }

    loop {
        // 解析一轮（panic 防护：单轮解析崩溃不拖垮整个后台任务）
        parse_round_guarded(&mut write_db, &current, &mut states, &mut counters);

        // 自动清理检查：每天到达配置时刻后执行一次（删除 N 天前的日志）
        auto_cleanup_guarded(&mut write_db, &current, &mut cleanup_last_date, &sqlite_cfg);

        // WAL 定时回写主库检查：每天到达配置时刻后执行一次（截断 -wal 文件）
        wal_checkpoint_guarded(&mut write_db, &sqlite_cfg, &mut wal_ckpt_last_date);

        // 等待：间隔到期 或 配置变更（变更则立即进入下一轮）或 手动重解析请求
        let interval = Duration::from_secs(current.interval_secs.max(5));
        tokio::select! {
            _ = tokio::time::sleep(interval) => {}
            _ = config_rx.changed() => {
                current = config_rx.borrow_and_update().clone();
                info!("nginx 日志源配置已更新：{} 个源，间隔 {}s", current.sources.len(), current.interval_secs);
            }
            Some(source_id) = reparse_rx.recv() => {
                // 手动重解析：清了历史数据后，立即从头解析该源（不等下一轮间隔）
                let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    reparse_source(&mut write_db, source_id, &current, &mut states, &mut counters)
                }));
                match r {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => error!("手动重解析源 {} 失败: {}", source_id, e),
                    Err(p) => error!(
                        "手动重解析源 {} 发生 panic（已捕获，任务继续运行）: {}",
                        source_id, panic_msg(&p)
                    ),
                }
            }
        }
    }
}

/// 提取 panic 消息（String / &str / 其它）
fn panic_msg(p: &(dyn std::any::Any + Send)) -> String {
    p.downcast_ref::<String>().cloned()
        .or_else(|| p.downcast_ref::<&str>().map(|s| s.to_string()))
        .unwrap_or_else(|| "unknown panic".to_string())
}

/// parse_round 的 panic 防护包装
///
/// tokio::spawn 的任务 panic 会被静默吞掉（JoinHandle 无人 await），
/// 表现为解析永久停止而 Web 接口一切正常——这里捕获单轮 panic 保证任务存活。
fn parse_round_guarded(
    db: &mut Connection,
    config: &NginxSourcesConfig,
    states: &mut HashMap<i64, SourceState>,
    counters: &mut HashMap<i64, (u64, u64)>,
) {
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        parse_round(db, config, states, counters);
    }));
    if let Err(p) = r {
        error!("解析轮次发生 panic（已捕获，任务继续运行）: {}", panic_msg(&p));
    }
}

/// 自动清理的 panic 防护包装
fn auto_cleanup_guarded(
    db: &mut Connection,
    config: &NginxSourcesConfig,
    last_date: &mut Option<chrono::NaiveDate>,
    sqlite_cfg: &crate::config::SqliteConfig,
) {
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        maybe_auto_cleanup(db, config, last_date, sqlite_cfg);
    }));
    if let Err(p) = r {
        error!("自动清理发生 panic（已捕获，任务继续运行）: {}", panic_msg(&p));
    }
}

/// WAL checkpoint 的 panic 防护包装
fn wal_checkpoint_guarded(
    db: &mut Connection,
    sqlite_cfg: &crate::config::SqliteConfig,
    last_date: &mut Option<chrono::NaiveDate>,
) {
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        maybe_wal_checkpoint(db, sqlite_cfg, last_date);
    }));
    if let Err(p) = r {
        error!("WAL checkpoint 发生 panic（已捕获，任务继续运行）: {}", panic_msg(&p));
    }
}

/// 检查 WAL 定时回写是否到期，到期则执行一次（每天最多一次）
///
/// 长期运行的服务 `-wal` 文件可能涨到与主库同量级（自动 checkpoint 受并发读等因素影响滞后）。
/// 这里每天在配置时刻做一次 `PRAGMA wal_checkpoint(TRUNCATE)`：把 WAL 全部回写主库并截断文件。
fn maybe_wal_checkpoint(
    db: &mut Connection,
    sqlite_cfg: &crate::config::SqliteConfig,
    last_date: &mut Option<chrono::NaiveDate>,
) {
    if !sqlite_cfg.wal_checkpoint_enabled {
        return;
    }
    let Some((_h, _m)) = parse_hhmm(&sqlite_cfg.wal_checkpoint_time) else {
        warn!(
            "WAL 定时 checkpoint 时刻配置非法：「{}」，跳过（应为 HH:MM）",
            sqlite_cfg.wal_checkpoint_time
        );
        return;
    };

    let now = chrono::Local::now();
    let today = now.date_naive();
    if *last_date == Some(today) {
        return; // 今天已执行过
    }
    let Some((h, m)) = parse_hhmm(&sqlite_cfg.wal_checkpoint_time) else {
        return;
    };
    let Some(sched) = today.and_hms_opt(h, m, 0) else {
        return;
    };
    if now.naive_local() < sched {
        return; // 还没到今天的执行时刻
    }

    do_wal_checkpoint(db, sqlite_cfg, "定时");
    // 无论成功失败都标记今天已执行，避免失败时每轮重复刷日志
    *last_date = Some(today);
}

/// 执行一次 WAL 回写主库（`PRAGMA wal_checkpoint(TRUNCATE)`）
///
/// - `reason` 只用于日志区分触发来源（"启动" / "定时"）
/// - 有并发读连接时 TRUNCATE 会返回 busy 且无法截断文件，此时补做一次 PASSIVE：
///   至少把能回写的页都写回主库，文件留待下次（或重启时）截断
fn do_wal_checkpoint(db: &Connection, sqlite_cfg: &crate::config::SqliteConfig, reason: &str) {
    let wal_path = format!("{}-wal", sqlite_cfg.path);
    let size_of = |p: &str| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
    let before = size_of(&wal_path);

    match wal_checkpoint(db, "TRUNCATE") {
        Ok((busy, log, ckpt)) => {
            let after = size_of(&wal_path);
            if busy != 0 {
                // 有并发读连接时无法截断文件，再以 PASSIVE 方式补一次：至少把能回写的都写回主库
                let passive = wal_checkpoint(db, "PASSIVE");
                warn!(
                    "WAL {} checkpoint 未完全完成（存在并发读连接，busy={}，剩余 {} 页）；\
                     已补做 PASSIVE 回写 {:?}，WAL {} MB → {} MB",
                    reason, busy, log, passive, mb(before), mb(after)
                );
            } else {
                info!(
                    "WAL {} checkpoint 完成：回写 {} 页，WAL 文件 {} MB → {} MB",
                    reason, ckpt, mb(before), mb(after)
                );
            }
        }
        Err(e) => error!("WAL {} checkpoint 执行失败: {}", reason, e),
    }
}

/// 执行 `PRAGMA wal_checkpoint(<mode>)`，返回 (busy, WAL 剩余页数, 本次回写页数)
fn wal_checkpoint(db: &Connection, mode: &str) -> rusqlite::Result<(i32, i32, i32)> {
    let sql = format!("PRAGMA wal_checkpoint({})", mode);
    db.query_row(&sql, [], |row| {
        Ok((row.get(0)?, row.get(1)?, row.get(2)?))
    })
}

/// 字节 → MB（保留 1 位小数）
fn mb(bytes: u64) -> String {
    format!("{:.1}", bytes as f64 / 1024.0 / 1024.0)
}

/// 检查自动清理是否到期，到期则执行一次（每天最多一次）
///
/// 触发条件：功能启用 && 今天还没执行过 && 已过当天配置的执行时刻。
/// 服务重启后当天若已错过时刻会补执行一次（清理是幂等的，重复执行无副作用）。
fn maybe_auto_cleanup(
    db: &mut Connection,
    config: &NginxSourcesConfig,
    last_date: &mut Option<chrono::NaiveDate>,
    sqlite_cfg: &crate::config::SqliteConfig,
) {
    if !config.auto_cleanup_enabled {
        return;
    }
    let Some((h, m)) = parse_hhmm(&config.auto_cleanup_time) else {
        warn!("自动清理时刻配置非法：「{}」，跳过（应为 HH:MM）", config.auto_cleanup_time);
        return;
    };

    let now = chrono::Local::now();
    let today = now.date_naive();
    if *last_date == Some(today) {
        return; // 今天已执行过
    }
    let Some(sched) = today.and_hms_opt(h, m, 0) else {
        return;
    };
    if now.naive_local() < sched {
        return; // 还没到今天的执行时刻
    }

    // 到期：删除 N 天前的日志（与手动清理同一套逻辑，含 VACUUM）
    let cutoff = chrono::Utc::now().timestamp() - config.auto_cleanup_days as i64 * 86400;
    match crate::web::stats::delete_before(db, cutoff) {
        Ok(n) => {
            info!(
                "自动清理：删除 {} 天前的日志 {} 条（计划每天 {} 执行）",
                config.auto_cleanup_days, n, config.auto_cleanup_time
            );
            if sqlite_cfg.vacuum_after_delete && n >= sqlite_cfg.vacuum_min_deleted {
                match crate::web::stats::vacuum(db) {
                    Ok(()) => info!("自动清理后 VACUUM 完成"),
                    Err(e) => warn!("自动清理后 VACUUM 失败: {}", e),
                }
            }
        }
        Err(e) => error!("自动清理执行失败: {}（明日 {} 再试）", e, config.auto_cleanup_time),
    }
    // 无论成功失败都标记今天已执行，避免失败时每轮重复刷日志
    *last_date = Some(today);
}

/// 校验 HH:MM 格式，返回 (时, 分)
fn parse_hhmm(s: &str) -> Option<(u32, u32)> {
    let (h, m) = s.trim().split_once(':')?;
    let h: u32 = h.trim().parse().ok()?;
    let m: u32 = m.trim().parse().ok()?;
    if h < 24 && m < 60 {
        Some((h, m))
    } else {
        None
    }
}

/// 手动重解析单个源：内存状态清零（强制从文件头开始），立即解析一次
fn reparse_source(
    db: &mut Connection,
    source_id: i64,
    config: &NginxSourcesConfig,
    states: &mut HashMap<i64, SourceState>,
    counters: &mut HashMap<i64, (u64, u64)>,
) -> anyhow::Result<()> {
    // 内存进度清零：从文件头开始（initialized=true 跳过 start_at_end 逻辑）
    states.insert(source_id, SourceState { offset: 0, initialized: true });
    counters.insert(source_id, (0, 0));

    let Some(src) = config.sources.iter().find(|s| s.id == source_id) else {
        info!("手动重解析：源 {} 已不在配置中，仅清除了历史数据", source_id);
        return Ok(());
    };

    match parse_source(db, src, states, counters)? {
        Some(summary) => info!(
            "手动重解析源 {}（{}）：解析 {} 行，失败 {} 行",
            source_id,
            if src.name.is_empty() { &src.path } else { &src.name },
            summary.lines_read,
            summary.lines_failed
        ),
        None => warn!("手动重解析源 {}：文件不可读或无内容", source_id),
    }
    Ok(())
}

/// 执行一轮解析：遍历所有启用的源，逐个增量解析并落库
fn parse_round(
    db: &mut Connection,
    config: &NginxSourcesConfig,
    states: &mut HashMap<i64, SourceState>,
    counters: &mut HashMap<i64, (u64, u64)>,
) {
    for src in &config.sources {
        if !src.enabled {
            continue;
        }
        match parse_source(db, src, states, counters) {
            Ok(Some(summary)) => {
                if summary.lines_read > 0 {
                    info!(
                        "解析源 {}（{}）：新增 {} 行，失败 {} 行",
                        src.id,
                        if src.name.is_empty() { &src.path } else { &src.name },
                        summary.lines_read,
                        summary.lines_failed
                    );
                }
                if let Some(err) = &summary.last_error {
                    warn!("解析源 {} 出现错误: {}", src.id, err);
                }
            }
            Ok(None) => {} // 无新增内容
            Err(e) => {
                error!("解析源 {}（{}）失败: {}", src.id, src.path, e);
                let _ = upsert_parse_state(
                    db,
                    src.id,
                    states.get(&src.id).map(|s| s.offset).unwrap_or(0),
                    counters.get(&src.id).copied().unwrap_or((0, 0)),
                    Some(&e.to_string()),
                );
            }
        }
    }
}

/// 增量解析单个日志源
///
/// 返回 Ok(None) 表示文件不可读或无新增内容
fn parse_source(
    db: &mut Connection,
    src: &LogSource,
    states: &mut HashMap<i64, SourceState>,
    counters: &mut HashMap<i64, (u64, u64)>,
) -> anyhow::Result<Option<ParseSummary>> {
    // 确保状态已从库里加载
    if !states.contains_key(&src.id) {
        let offset = load_offset(db, src.id);
        let initialized = offset.is_some();
        states.insert(
            src.id,
            SourceState {
                offset: offset.unwrap_or(0),
                initialized,
            },
        );
    }
    if !counters.contains_key(&src.id) {
        counters.insert(src.id, load_counters(db, src.id));
    }

    let file = match std::fs::File::open(&src.path) {
        Ok(f) => f,
        Err(e) => {
            // 文件暂时不存在（未生成/路径错误）：记录状态，不中断整轮
            let st = states.get(&src.id).copied().unwrap_or(SourceState { offset: 0, initialized: false });
            upsert_parse_state(db, src.id, st.offset, counters.get(&src.id).copied().unwrap_or((0, 0)), Some(&format!("打开文件失败: {}", e)))?;
            return Ok(None);
        }
    };
    let mut file = file;
    let size = file.metadata()?.len();

    let st = states.get_mut(&src.id).expect("状态已初始化");
    let mut offset = st.offset;

    // 首次遇到该源：决定起始位置
    if !st.initialized {
        if src.start_at_end {
            offset = size;
        }
        st.initialized = true;
        st.offset = offset;
    }

    // 轮转/截断检测：文件比已读位置还小说明被切割重写了
    if size < offset {
        info!(
            "源 {} 检测到日志轮转（文件 {} 字节 < 已读偏移 {}），从头解析",
            src.id, size, offset
        );
        offset = 0;
        st.offset = 0;
    }

    if size == offset {
        return Ok(None); // 无新增内容
    }

    // 定位并流式逐行读取
    file.seek(SeekFrom::Start(offset))?;
    let mut reader = BufReader::with_capacity(256 * 1024, file);

    // 解析格式（每轮解析；空 = 自动检测，"error" = nginx error.log，其它按 log_format 编译）
    let mode = match parser::resolve_format(&src.format) {
        Ok(m) => m,
        Err(e) => {
            return Err(anyhow::anyhow!("log_format 编译失败: {}", e));
        }
    };

    let mut summary = ParseSummary::default();
    let mut batch: Vec<super::models::ParsedLine> = Vec::with_capacity(2048);

    loop {
        let mut buf = Vec::with_capacity(512);
        let n = reader.read_until(b'\n', &mut buf)?;
        if n == 0 {
            break; // EOF
        }

        let ends_with_nl = buf.last() == Some(&b'\n');
        if !ends_with_nl {
            // 文件末尾的不完整行（正在写入）：不计入本次消费，下次再读。
            // 因为 new_offset 只累加完整行的字节数，无需 seek 回退。
            break;
        }

        let line = String::from_utf8_lossy(&buf[..buf.len() - 1]); // 去掉 \n
        let line = line.trim_end_matches('\r');
        summary.lines_read += 1;
        summary.bytes_consumed += n as u64;

        match parser::parse_line_mode(line, &mode) {
            Some(parsed) => batch.push(parsed),
            None => summary.lines_failed += 1,
        }

        // 分批落库，避免单事务过大
        if batch.len() >= 5000 {
            insert_batch(db, src.id, &batch)?;
            batch.clear();
            // 增量更新进度：大文件（上百 MB）解析需要数分钟，
            // 实时写 parse_state 让页面进度可见，也能据此判断解析是否卡住
            let c = counters.get(&src.id).copied().unwrap_or((0, 0));
            upsert_parse_state(
                db,
                src.id,
                offset + summary.bytes_consumed,
                (c.0 + summary.lines_read, c.1 + summary.lines_failed),
                summary.last_error.as_deref(),
            )?;
        }
    }

    // 剩余批次
    if !batch.is_empty() {
        insert_batch(db, src.id, &batch)?;
    }

    let new_offset = offset + summary.bytes_consumed;
    st.offset = new_offset;

    // 更新累计行数统计
    let entry = counters.entry(src.id).or_insert((0, 0));
    entry.0 += summary.lines_read;
    entry.1 += summary.lines_failed;

    upsert_parse_state(db, src.id, new_offset, *counters.get(&src.id).expect("已插入"), summary.last_error.as_deref())?;

    Ok(Some(summary))
}

/// 批量插入解析结果（单事务）
fn insert_batch(
    db: &mut Connection,
    source_id: i64,
    batch: &[super::models::ParsedLine],
) -> Result<(), rusqlite::Error> {
    let tx = db.transaction()?;
    {
        let mut stmt = tx.prepare(
            "INSERT INTO access_logs
                (source_id, ts, remote_addr, method, path, query, protocol,
                 status, bytes, referer, user_agent, request_time, extra, raw)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
        )?;
        for p in batch {
            stmt.execute(rusqlite::params![
                source_id,
                p.ts,
                p.remote_addr,
                p.method,
                p.path,
                p.query,
                p.protocol,
                p.status,
                p.bytes,
                p.referer,
                p.user_agent,
                p.request_time,
                p.extra,
                p.raw,
            ])?;
        }
    }
    tx.commit()
}

/// 从 parse_state 表读取已保存的偏移
fn load_offset(db: &Connection, source_id: i64) -> Option<u64> {
    db.query_row(
        "SELECT offset FROM parse_state WHERE source_id = ?1",
        rusqlite::params![source_id],
        |row| row.get::<_, i64>(0),
    )
    .ok()
    .map(|v| v.max(0) as u64)
}

/// 读取累计行数统计
fn load_counters(db: &Connection, source_id: i64) -> (u64, u64) {
    db.query_row(
        "SELECT lines_total, lines_failed FROM parse_state WHERE source_id = ?1",
        rusqlite::params![source_id],
        |row| {
            Ok((
                row.get::<_, i64>(0)?.max(0) as u64,
                row.get::<_, i64>(1)?.max(0) as u64,
            ))
        },
    )
    .unwrap_or((0, 0))
}

/// 更新解析进度（含统计），upsert
fn upsert_parse_state(
    db: &Connection,
    source_id: i64,
    offset: u64,
    counters: (u64, u64),
    last_error: Option<&str>,
) -> Result<(), rusqlite::Error> {
    let now = chrono::Utc::now().to_rfc3339();
    db.execute(
        "INSERT INTO parse_state (source_id, offset, last_parse_at, lines_total, lines_failed, last_error)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(source_id) DO UPDATE SET
            offset = excluded.offset,
            last_parse_at = excluded.last_parse_at,
            lines_total = excluded.lines_total,
            lines_failed = excluded.lines_failed,
            last_error = excluded.last_error",
        rusqlite::params![
            source_id,
            offset as i64,
            now,
            counters.0 as i64,
            counters.1 as i64,
            last_error
        ],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{mb, maybe_wal_checkpoint, parse_hhmm, wal_checkpoint};
    use crate::config::SqliteConfig;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn tmp_path(tag: &str) -> String {
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        std::env::temp_dir()
            .join(format!("nlr_wal_{}_{}.db", tag, nanos))
            .to_string_lossy()
            .to_string()
    }

    fn test_sqlite_cfg(path: &str) -> SqliteConfig {
        SqliteConfig {
            path: path.to_string(),
            vacuum_after_delete: true,
            vacuum_min_deleted: 1000,
            wal_autocheckpoint: 0, // 关闭自动 checkpoint，保证 WAL 一直增长
            journal_size_limit: 0,
            wal_checkpoint_enabled: true,
            wal_checkpoint_time: "00:01".to_string(), // 已过时刻 = 立即执行
        }
    }

    #[test]
    fn test_parse_hhmm() {
        assert_eq!(parse_hhmm("04:30"), Some((4, 30)));
        assert_eq!(parse_hhmm("23:59"), Some((23, 59)));
        assert_eq!(parse_hhmm("24:00"), None);
        assert_eq!(parse_hhmm("04:60"), None);
        assert_eq!(parse_hhmm("0430"), None);
    }

    #[test]
    fn test_mb_format() {
        assert_eq!(mb(0), "0.0");
        assert_eq!(mb(1024 * 1024), "1.0");
        assert_eq!(mb(5 * 1024 * 1024), "5.0");
    }

    /// 定时 checkpoint：把 WAL 回写主库并截断 -wal 文件
    #[test]
    fn test_wal_checkpoint_truncates() {
        let path = tmp_path("trunc");
        let wal = format!("{}-wal", path);
        let mut db = rusqlite::Connection::open(&path).unwrap();
        db.pragma_update(None, "journal_mode", "WAL").unwrap();
        db.pragma_update(None, "wal_autocheckpoint", 0i32).unwrap();
        db.execute_batch(
            "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT);
             CREATE TABLE access_logs (id INTEGER PRIMARY KEY, ts INTEGER, remote_addr TEXT,
                 method TEXT, path TEXT, protocol TEXT, status INTEGER, bytes INTEGER);",
        )
        .unwrap();
        // 写入足够多的数据让 WAL 明显膨胀
        let tx = db.transaction().unwrap();
        for i in 0..2000 {
            tx.execute(
                "INSERT INTO t (v) VALUES (?1)",
                [format!("row-{}-{}", i, "x".repeat(200))],
            )
            .unwrap();
        }
        tx.commit().unwrap();

        let before = std::fs::metadata(&wal).map(|m| m.len()).unwrap_or(0);
        assert!(before > 100_000, "WAL 应已膨胀，实际 {} 字节", before);

        let cfg = test_sqlite_cfg(&path);
        let mut last = None;
        maybe_wal_checkpoint(&mut db, &cfg, &mut last);

        let after = std::fs::metadata(&wal).map(|m| m.len()).unwrap_or(0);
        assert!(
            after < before / 2,
            "checkpoint 后 WAL 应显著减小：{} → {}",
            before,
            after
        );
        // 同一天不会重复执行（last_date 已标记）
        assert!(last.is_some());

        // 数据完整性：回写后主库仍可读到全部行
        let n: i64 = db.query_row("SELECT COUNT(*) FROM t", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 2000);

        let _ = db.close();
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&wal);
        let _ = std::fs::remove_file(format!("{}-shm", path));
    }

    /// 关闭开关时不执行 checkpoint
    #[test]
    fn test_wal_checkpoint_disabled() {
        let path = tmp_path("off");
        let wal = format!("{}-wal", path);
        let mut db = rusqlite::Connection::open(&path).unwrap();
        db.pragma_update(None, "journal_mode", "WAL").unwrap();
        db.pragma_update(None, "wal_autocheckpoint", 0i32).unwrap();
        db.execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT);").unwrap();
        for i in 0..500 {
            db.execute("INSERT INTO t (v) VALUES (?1)", [format!("r{}", i)])
                .unwrap();
        }
        let before = std::fs::metadata(&wal).map(|m| m.len()).unwrap_or(0);

        let mut cfg = test_sqlite_cfg(&path);
        cfg.wal_checkpoint_enabled = false;
        let mut last = None;
        maybe_wal_checkpoint(&mut db, &cfg, &mut last);

        let after = std::fs::metadata(&wal).map(|m| m.len()).unwrap_or(0);
        assert_eq!(before, after, "未启用时不应改动 WAL");
        assert!(last.is_none(), "未启用时不应标记执行日期");

        let _ = db.close();
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&wal);
        let _ = std::fs::remove_file(format!("{}-shm", path));
    }

    /// wal_checkpoint 直接调用：返回 (busy, 剩余页数, 回写页数)
    #[test]
    fn test_wal_checkpoint_pragma() {
        let path = tmp_path("pragma");
        let wal = format!("{}-wal", path);
        let db = rusqlite::Connection::open(&path).unwrap();
        db.pragma_update(None, "journal_mode", "WAL").unwrap();
        db.execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT);").unwrap();
        db.execute("INSERT INTO t (v) VALUES ('a')", []).unwrap();

        let (busy, _log, _ckpt) = wal_checkpoint(&db, "TRUNCATE").unwrap();
        assert_eq!(busy, 0, "无并发读者时不应 busy");

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&wal);
        let _ = std::fs::remove_file(format!("{}-shm", path));
    }
}
