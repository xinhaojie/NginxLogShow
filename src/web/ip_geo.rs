// src/web/ip_geo.rs
//! IP 归属地查询（ip9.com.cn 接口）+ SQLite 永久缓存
//!
//! 缓存策略：
//! - `ip_geo` 表记录每个 IP 的归属地与首次查询时间 `queried_at`（仅作记录用）
//! - **查到即永久有效**：缓存命中的 IP 直接复用，不再外查（归属地基本不变，无过期概念）
//! - 只有从未查过的 IP 才调用 ip9.com.cn（单 IP 查询，无 batch，支持 IPv4/IPv6）
//! - 私网 / 保留地址不发起 HTTP 请求，直接记为「内网/保留地址」
//!
//! 限速兼容（官方限速 60 次/秒，本地留余量保护）：
//! - 本地滑动窗口限速 50 次/秒 + 相邻请求最小间隔 20ms
//! - 单次接口调用最多同步外查 MAX_SYNC_FETCH 个（其余下次请求/刷新再补查）
//! - 请求失败（非 200 / 空响应 / 非 JSON）视为疑似触发对方限速：进入冷却期
//!   （60s 起指数翻倍，上限 15 分钟），冷却期内不外查、不写缓存，自动恢复重试

use std::net::IpAddr;
use std::time::{Duration, Instant};

use rusqlite::{types::Value as SqlValue, Connection, ToSql};
use serde::{Deserialize, Serialize};

/// 滑动窗口限速：官方 60 次/秒，本地留余量取 50 次/秒
const RATE_LIMIT: usize = 50;
/// 限速滑动窗口
const RATE_WINDOW: Duration = Duration::from_secs(1);
/// 相邻两次外查的最小间隔
const MIN_INTERVAL: Duration = Duration::from_millis(20);
/// 单次接口调用最多同步外查的 IP 数（明细页最大 50 条/页，一页可全部补齐）
const MAX_SYNC_FETCH: usize = 50;
/// 失败冷却基数：首次失败暂停 60s
const COOLDOWN_BASE: Duration = Duration::from_secs(60);
/// 冷却上限：15 分钟
const COOLDOWN_MAX: Duration = Duration::from_secs(15 * 60);

/* ============================================================
 * 外查闸门：冷却 + 滑动窗口限速 + 最小间隔（全局状态）
 * ============================================================ */
struct FetchGate {
    /// 滑动窗口内的请求时刻
    sent_times: Vec<Instant>,
    /// 上一次请求时刻（最小间隔用）
    last_sent: Option<Instant>,
    /// 冷却截止时刻（疑似触发对方限速时设置）
    cooldown_until: Option<Instant>,
    /// 连续失败次数（驱动冷却指数翻倍；成功一次清零）
    fail_streak: u32,
}

static GATE: std::sync::LazyLock<std::sync::Mutex<FetchGate>> =
    std::sync::LazyLock::new(|| {
        std::sync::Mutex::new(FetchGate {
            sent_times: Vec::new(),
            last_sent: None,
            cooldown_until: None,
            fail_streak: 0,
        })
    });

/// 冷却时长：60s 起按连续失败次数指数翻倍，封顶 15 分钟
fn cooldown_duration(fail_streak: u32) -> Duration {
    let exp = fail_streak.saturating_sub(1).min(8);
    let d = COOLDOWN_BASE.saturating_mul(1u32 << exp);
    d.min(COOLDOWN_MAX)
}

/// 申请一次外查机会；false = 冷却中 / 窗口限速已满（本次不外查，不写缓存）
fn gate_allow() -> bool {
    let mut g = GATE.lock().unwrap_or_else(|p| p.into_inner());
    let now = Instant::now();

    // 冷却期内直接拒绝
    if let Some(until) = g.cooldown_until {
        if now < until {
            return false;
        }
    }
    // 滑动窗口限速
    g.sent_times.retain(|t| now.duration_since(*t) < RATE_WINDOW);
    if g.sent_times.len() >= RATE_LIMIT {
        return false;
    }
    // 最小间隔：间隔不够时短暂 sleep 补齐（外查本身在 spawn_blocking 线程，不阻塞异步）
    if let Some(last) = g.last_sent {
        let elapsed = now.duration_since(last);
        if elapsed < MIN_INTERVAL {
            std::thread::sleep(MIN_INTERVAL - elapsed);
        }
    }
    let now = Instant::now();
    g.sent_times.push(now);
    g.last_sent = Some(now);
    true
}

/// 一次请求失败：进入冷却（指数翻倍）
fn gate_fail(reason: &str) {
    let mut g = GATE.lock().unwrap_or_else(|p| p.into_inner());
    g.fail_streak += 1;
    let d = cooldown_duration(g.fail_streak);
    g.cooldown_until = Some(Instant::now() + d);
    log::warn!(
        "ip9 外查失败（{}），疑似触发对方限速，暂停 {}s 后自动重试（连续失败 {} 次）",
        reason,
        d.as_secs(),
        g.fail_streak
    );
}

/// 一次请求成功：清零失败计数
fn gate_success() {
    let mut g = GATE.lock().unwrap_or_else(|p| p.into_inner());
    g.fail_streak = 0;
}

/* ============================================================
 * 查询与缓存
 * ============================================================ */

/// 单个 IP 的归属地
#[derive(Debug, Clone, Serialize)]
pub struct GeoInfo {
    pub ip: String,
    pub country: Option<String>,
    pub region: Option<String>,
    pub city: Option<String>,
    pub isp: Option<String>,
}

/// ip9.com.cn 响应（UTF-8 JSON）：{"ret":200,"data":{"country":"中国","prov":"浙江","city":"杭州","isp":"中国电信",...}}
#[derive(Debug, Deserialize)]
struct Ip9Resp {
    ret: i32,
    /// 查不到时 data 可能为 null
    #[serde(default)]
    data: Option<Ip9Data>,
}

#[derive(Debug, Deserialize, Default)]
struct Ip9Data {
    #[serde(default)]
    country: String,
    #[serde(default)]
    prov: String,
    #[serde(default)]
    city: String,
    #[serde(default)]
    isp: String,
}

/// 批量查询 IP 归属地（带缓存，命中即永久有效）
///
/// 返回与输入 IP 一一对应的结果（去重后查询，输入顺序不保留重复项）。
/// 网络失败 / 限速冷却 / 单次配额用完时，未查到的 IP 不返回（调用方展示 "-"），
/// 也不写缓存——下次请求自动补查。
pub fn lookup_geo(db: &Connection, ips: &[String]) -> Vec<GeoInfo> {
    // 去重、去空
    let mut uniq: Vec<String> = Vec::new();
    for ip in ips {
        let ip = ip.trim();
        if !ip.is_empty() && !uniq.iter().any(|x| x == ip) {
            uniq.push(ip.to_string());
        }
    }
    if uniq.is_empty() {
        return Vec::new();
    }

    let now = chrono::Utc::now().timestamp();

    // ---- 1. 读缓存（命中即有效，无过期概念）----
    let mut fresh: Vec<GeoInfo> = Vec::new();
    let mut miss: Vec<String> = Vec::new();
    let placeholders: Vec<String> = (1..=uniq.len()).map(|i| format!("?{i}")).collect();
    let sql = format!(
        "SELECT ip, country, region, city, isp FROM ip_geo
         WHERE ip IN ({})",
        placeholders.join(",")
    );
    let params: Vec<SqlValue> = uniq.iter().map(|ip| SqlValue::Text(ip.clone())).collect();
    let refs: Vec<&dyn ToSql> = params.iter().map(|p| p as &dyn ToSql).collect();
    if let Ok(mut stmt) = db.prepare(&sql) {
        if let Ok(rows) = stmt.query_map(refs.as_slice(), |row| {
            Ok(GeoInfo {
                ip: row.get(0)?,
                country: row.get(1)?,
                region: row.get(2)?,
                city: row.get(3)?,
                isp: row.get(4)?,
            })
        }) {
            let mut cached: Vec<GeoInfo> = rows.filter_map(|r| r.ok()).collect();
            // 缓存未命中的 IP 进入待查询列表
            for ip in &uniq {
                match cached.iter().position(|g| &g.ip == ip) {
                    Some(i) => fresh.push(cached.swap_remove(i)),
                    None => miss.push(ip.clone()),
                }
            }
        }
    }

    // ---- 2. 查询未缓存的 IP ----
    if !miss.is_empty() {
        let fetched = fetch_and_store(db, &miss, now);
        fresh.extend(fetched);
    }

    fresh
}

/// 逐个调用 ip9 接口查询并写缓存
///
/// - 私网 / 保留地址直接本地标注，不占外查配额（不受外查失败影响）
/// - 单次调用最多外查 MAX_SYNC_FETCH 个 IP（限速保护，剩余下次补查）
/// - 失败立即进入冷却并中止本轮（剩余 IP 不处理、不写缓存）
fn fetch_and_store(db: &Connection, ips: &[String], now: i64) -> Vec<GeoInfo> {
    let mut results: Vec<GeoInfo> = Vec::with_capacity(ips.len().min(MAX_SYNC_FETCH) + 2);
    let mut remote: Vec<&String> = Vec::new();

    // 第一遍：私网 / 保留地址直接本地生成
    for ip in ips {
        if is_private(ip) {
            let info = GeoInfo {
                ip: ip.clone(),
                country: None,
                region: None,
                city: None,
                isp: Some("内网/保留地址".to_string()),
            };
            store(db, &info, now);
            results.push(info);
        } else {
            remote.push(ip);
        }
    }

    // 第二遍：外查（冷却中 / 窗口限速已满 / 失败时中止，剩余不写缓存下次自动重试）
    let mut budget = MAX_SYNC_FETCH;
    for ip in remote {
        if budget == 0 || !gate_allow() {
            break;
        }
        budget -= 1;

        match fetch_one(ip) {
            Ok(resp) => {
                gate_success();
                let info = to_geo_info(ip, &resp);
                store(db, &info, now);
                results.push(info);
            }
            Err(e) => {
                gate_fail(&e);
                break;
            }
        }
    }

    results
}

/// 调用 ip9.com.cn 接口查询单个 IP（HTTPS + UTF-8 JSON，支持 IPv4/IPv6）
fn fetch_one(ip: &str) -> Result<Ip9Resp, String> {
    let resp = ureq::get("https://ip9.com.cn/get")
        .query("ip", ip)
        .timeout(Duration::from_secs(6))
        .call()
        .map_err(|e| format!("HTTP 请求失败: {e}"))?;

    let text = resp
        .into_string()
        .map_err(|e| format!("读取响应失败: {e}"))?;
    let parsed: Ip9Resp = serde_json::from_str(text.trim())
        .map_err(|_| format!("响应不是有效 JSON（可能是限速页）: {:.80}", text.trim()))?;
    if parsed.ret != 200 {
        return Err(format!("接口返回 ret={}（非 200）", parsed.ret));
    }
    Ok(parsed)
}

/// ip9 字段 → GeoInfo：country（中国/美国/保留…）、prov=省、city=市、isp=运营商
fn to_geo_info(ip: &str, r: &Ip9Resp) -> GeoInfo {
    let d = r.data.as_ref();
    GeoInfo {
        ip: ip.to_string(),
        country: d.and_then(|d| non_empty(&d.country)),
        region: d.and_then(|d| non_empty(&d.prov)),
        city: d.and_then(|d| non_empty(&d.city)),
        isp: d.and_then(|d| non_empty(&d.isp)),
    }
}

/// 写缓存（UPSERT）
fn store(db: &Connection, info: &GeoInfo, now: i64) {
    if let Err(e) = db.execute(
        "INSERT INTO ip_geo (ip, country, region, city, isp, queried_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(ip) DO UPDATE SET
            country = excluded.country, region = excluded.region,
            city = excluded.city, isp = excluded.isp, queried_at = excluded.queried_at",
        rusqlite::params![info.ip, info.country, info.region, info.city, info.isp, now],
    ) {
        log::warn!("ip_geo 缓存写入失败（{}）: {}", info.ip, e);
    }
}

fn non_empty(s: &str) -> Option<String> {
    let t = s.trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}

/// 私网 / 保留地址判断（这些地址外查也查不到，直接本地标注）
fn is_private(ip: &str) -> bool {
    match ip.parse::<IpAddr>() {
        Ok(IpAddr::V4(v4)) => {
            let o = v4.octets();
            o[0] == 10
                || o[0] == 127
                || (o[0] == 172 && (16..=31).contains(&o[1]))
                || (o[0] == 192 && o[1] == 168)
                || (o[0] == 169 && o[1] == 254)
                || o[0] == 0
                || o[0] >= 224 // 组播 + 保留
        }
        Ok(IpAddr::V6(v6)) => {
            let seg = v6.segments();
            seg[0] == 0 && seg[1] == 0 && seg[2] == 0 && seg[3] == 0 && seg[4] == 0 && seg[5] == 0
                && (seg[6] == 0 || seg[6] == 1) // :: 与 ::1
                || (seg[0] & 0xfe00) == 0xfc00 // fc00::/7 ULA
                || (seg[0] & 0xffc0) == 0xfe80 // fe80::/10 链路本地
        }
        Err(_) => true, // 非 IP 字符串（如域名、'-')，不外查
    }
}

#[cfg(test)]
mod tests {
    use super::{cooldown_duration, is_private, lookup_geo, to_geo_info, Ip9Resp};
    use std::time::Duration;

    #[test]
    fn test_cache_hit_forever() {
        // 很久以前（400 天前）查过的 IP 也应直接命中缓存，不再外查
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute(
            "CREATE TABLE ip_geo (
                ip TEXT PRIMARY KEY, country TEXT, region TEXT, city TEXT,
                isp TEXT, queried_at INTEGER NOT NULL)",
            [],
        )
        .unwrap();
        let old = chrono::Utc::now().timestamp() - 400 * 86400;
        conn.execute(
            "INSERT INTO ip_geo (ip, country, region, city, isp, queried_at)
             VALUES ('8.8.8.8', '缓存命中', '测试省', '测试市', '测试ISP', ?1)",
            rusqlite::params![old],
        )
        .unwrap();

        let res = lookup_geo(&conn, &["8.8.8.8".to_string()]);
        assert_eq!(res.len(), 1);
        assert_eq!(res[0].country.as_deref(), Some("缓存命中"));
        assert_eq!(res[0].city.as_deref(), Some("测试市"));
        // 库里的 queried_at 不应被改动（未触发外查回写）
        let q: i64 = conn
            .query_row("SELECT queried_at FROM ip_geo WHERE ip = '8.8.8.8'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(q, old);
    }

    #[test]
    fn test_is_private() {
        assert!(is_private("127.0.0.1"));
        assert!(is_private("192.168.1.10"));
        assert!(is_private("10.0.0.1"));
        assert!(is_private("172.16.0.1"));
        assert!(is_private("172.31.255.255"));
        assert!(is_private("172.32.0.1") == false);
        assert!(is_private("8.8.8.8") == false);
        assert!(is_private("224.0.0.1"));
        assert!(is_private("::1"));
        assert!(is_private("fe80::1"));
        assert!(is_private("fd00::1"));
        assert!(is_private("2606:4700::1111") == false);
        assert!(is_private("not-an-ip"));
    }

    #[test]
    fn test_cooldown_backoff() {
        // 首次 60s，指数翻倍，封顶 15 分钟
        assert_eq!(cooldown_duration(1), Duration::from_secs(60));
        assert_eq!(cooldown_duration(2), Duration::from_secs(120));
        assert_eq!(cooldown_duration(3), Duration::from_secs(240));
        assert_eq!(cooldown_duration(9), Duration::from_secs(15 * 60));
        assert_eq!(cooldown_duration(100), Duration::from_secs(15 * 60));
    }

    #[test]
    fn test_parse_ip9_json() {
        // 国内 IPv6
        let r: Ip9Resp = serde_json::from_str(
            r#"{"ret":200,"data":{"ip":"240e:471:eb0:cc03:704d:8ff:fe0e:1af4","country":"中国","country_code":"cn","prov":"浙江","city":"杭州","city_code":"hangzhou","city_short_code":"hz","area":"","post_code":"310000","area_code":"0571","isp":"中国电信","lng":"120.15","lat":"30.29","long_ip":0,"big_area":"华东"},"qt":0.001}"#,
        )
        .unwrap();
        let g = to_geo_info("240e:471:eb0:cc03:704d:8ff:fe0e:1af4", &r);
        assert_eq!(g.country.as_deref(), Some("中国"));
        assert_eq!(g.region.as_deref(), Some("浙江"));
        assert_eq!(g.city.as_deref(), Some("杭州"));
        assert_eq!(g.isp.as_deref(), Some("中国电信"));

        // 海外 IPv4：省/市为空
        let r: Ip9Resp = serde_json::from_str(
            r#"{"ret":200,"data":{"ip":"8.8.8.8","country":"美国","country_code":"us","prov":"","city":"","isp":"Google Cloud","big_area":""},"qt":0.001}"#,
        )
        .unwrap();
        let g = to_geo_info("8.8.8.8", &r);
        assert_eq!(g.country.as_deref(), Some("美国"));
        assert!(g.region.is_none());
        assert!(g.city.is_none());
        assert_eq!(g.isp.as_deref(), Some("Google Cloud"));

        // ret 非 200 也能解析（fetch_one 中判定为失败）
        let r: Ip9Resp = serde_json::from_str(r#"{"ret":404,"data":null}"#).unwrap();
        assert_eq!(r.ret, 404);
    }
}
