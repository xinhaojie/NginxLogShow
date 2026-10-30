// src/nginx/parser.rs
//! nginx 日志行解析器
//!
//! 三条解析路径：
//! 1. log_format 字符串 → 正则编译 → 命名捕获提取字段（支持任意 $var / ${var}）
//! 2. JSON 行（escape=json 日志）→ serde_json 直接按 key 提取
//! 3. nginx 原生 error.log（`2026/10/22 09:00:00 [error] 123#456: *789 ...`）→ 内置正则
//!
//! 格式选择（resolve_format）：
//! - format 为空 → Auto：逐行自动检测（JSON → error.log → combined）
//! - format = "error" → 强制按 nginx error.log 解析
//! - 其它 → 按 log_format 编译正则
//!
//! log_format 编译规则：
//! - 字面字符按正则转义原样保留
//! - `$var` / `${var}` 按后随字面字符选择匹配模式：
//!   - 后随 `"` → `[^"]*`（引号内变量，如 "$request"、"$http_user_agent"）
//!   - 后随 `]` → `[^\]]*`（方括号内变量，如 [$time_local]）
//!   - 已知数值变量（status/body_bytes_sent/bytes_sent）→ `\d+`
//!   - 其余 → `\S+`（非空白，兼容 IPv6、time_iso8601 等）
//! - 无法解析的行回退：ts 取当前时间，尽量保留已提取字段（不丢整行）

use regex::Regex;
use chrono::TimeZone;

use super::models::ParsedLine;

/// nginx 默认 combined 格式（format 为空时的回退解析模板）
pub const DEFAULT_COMBINED: &str = concat!(
    "$remote_addr - $remote_user [$time_local] ",
    "\"$request\" $status $body_bytes_sent \"$http_referer\" \"$http_user_agent\""
);

/// 编译后的日志格式
#[derive(Debug, Clone)]
pub struct CompiledFormat {
    pub regex: Regex,
    /// 格式串中出现的变量名（按出现顺序）
    pub vars: Vec<String>,
}

/// 日志源格式模式
#[derive(Debug, Clone)]
pub enum FormatMode {
    /// 自动检测：JSON → error.log → combined
    Auto,
    /// 强制按 nginx 原生 error.log 解析（format = "error"）
    Error,
    /// 按 log_format 编译的正则解析
    Compiled(CompiledFormat),
}

/// 把用户配置的 format 字符串解析为格式模式
pub fn resolve_format(format: &str) -> Result<FormatMode, String> {
    let fmt = format.trim();
    if fmt.is_empty() {
        return Ok(FormatMode::Auto);
    }
    if fmt.eq_ignore_ascii_case("error") {
        return Ok(FormatMode::Error);
    }
    Ok(FormatMode::Compiled(compile_format(fmt)?))
}

/// 数值型已知变量：按 `\d+` 匹配
const NUMERIC_VARS: &[&str] = &["status", "body_bytes_sent", "bytes_sent", "connection"];

/// 编译 log_format 字符串为正则。
/// 格式非法（空、无变量、正则构建失败）返回 Err。
pub fn compile_format(format: &str) -> Result<CompiledFormat, String> {
    let fmt = format.trim();
    if fmt.is_empty() {
        return Err("log_format 为空".to_string());
    }

    let mut pattern = String::with_capacity(fmt.len() * 2);
    let mut vars: Vec<String> = Vec::new();
    let bytes = fmt.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        let b = bytes[i];
        if b == b'$' {
            // 读取变量名：${name} 或 $name（name = 字母数字下划线）
            let (name, next) = if i + 1 < bytes.len() && bytes[i + 1] == b'{' {
                match fmt[i + 2..].find('}') {
                    Some(end) => (fmt[i + 2..i + 2 + end].to_string(), i + 2 + end + 1),
                    None => return Err(format!("未闭合的 ${{ 位于偏移 {}", i)),
                }
            } else {
                let start = i + 1;
                let mut j = start;
                while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_') {
                    j += 1;
                }
                if j == start {
                    return Err(format!("偏移 {} 处的 $ 后未跟有效变量名", i));
                }
                (fmt[start..j].to_string(), j)
            };

            if name.is_empty() {
                return Err("空变量名".to_string());
            }

            // 按后随字面字符选择捕获模式
            let next_char = fmt[next..].chars().next();
            let var_pattern = if NUMERIC_VARS.contains(&name.as_str()) {
                r"\d+".to_string()
            } else {
                match next_char {
                    Some('"') => r#"[^"]*"#.to_string(),
                    Some(']') => r"[^\]]*".to_string(),
                    _ => r"\S+".to_string(),
                }
            };

            pattern.push_str(&format!("(?P<{}>{})", name, var_pattern));
            vars.push(name);
            i = next;
        } else {
            // 字面字符：按 UTF-8 字符边界转义追加
            let ch_len = utf8_char_len(b);
            let seg = &fmt[i..(i + ch_len).min(fmt.len())];
            pattern.push_str(&regex::escape(seg));
            i += ch_len;
        }
    }

    if vars.is_empty() {
        return Err("log_format 中不含任何 $变量，无法解析".to_string());
    }

    let regex = Regex::new(&format!("^{}$", pattern))
        .map_err(|e| format!("log_format 编译失败: {}", e))?;

    Ok(CompiledFormat { regex, vars })
}

/// UTF-8 首字节对应的字符长度
fn utf8_char_len(b: u8) -> usize {
    if b < 0x80 {
        1
    } else if b >> 5 == 0b110 {
        2
    } else if b >> 4 == 0b1110 {
        3
    } else if b >> 3 == 0b11110 {
        4
    } else {
        1
    }
}

/// 按格式模式解析单行日志（失败返回 None，计入 lines_failed）
pub fn parse_line_mode(line: &str, mode: &FormatMode) -> Option<ParsedLine> {
    let line = line.trim_end_matches('\r');
    if line.is_empty() {
        return None;
    }

    match mode {
        FormatMode::Compiled(c) => parse_by_regex(line, c),
        FormatMode::Error => parse_error_line(line),
        FormatMode::Auto => {
            // 1) JSON 行（escape=json）
            if line.starts_with('{') {
                if let Some(p) = parse_json_line(line) {
                    return Some(p);
                }
            }
            // 2) nginx 原生 error.log
            if looks_like_error_log(line) {
                if let Some(p) = parse_error_line(line) {
                    return Some(p);
                }
            }
            // 3) 懒初始化 combined 编译结果（进程内缓存）
            static COMBINED: std::sync::LazyLock<CompiledFormat> =
                std::sync::LazyLock::new(|| {
                    compile_format(DEFAULT_COMBINED).expect("默认 combined 格式编译失败")
                });
            parse_by_regex(line, &COMBINED)
        }
    }
}

// ============================================================
// nginx 原生 error.log 解析
// ============================================================

/// error.log 行结构：
/// `2026/10/22 09:00:00 [error] 1234#5678: *789 open() "/x" failed (2: ...), client: 1.2.3.4, server: example.com, request: "GET /x HTTP/1.1", host: "example.com"`
/// 其中 `*789`（连接号）可能缺失；message 部分到行尾
static ERROR_LINE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r"^(\d{4}/\d{2}/\d{2} \d{2}:\d{2}:\d{2}) \[(\w+)\] (\d+#\d+): (?:\*(\d+) )?(.*)$")
        .expect("error.log 正则编译失败")
});

fn looks_like_error_log(line: &str) -> bool {
    ERROR_LINE.is_match(line)
}

/// 解析 nginx error.log 行
///
/// 字段映射：time → ts；client → remote_addr；request → method/path/query/protocol；
/// level/server/host/upstream → extra。error.log 无状态码，status 保持 NULL；
/// 时间为 nginx error_log 的本地时间（无时区后缀），按解析端本地时区换算。
fn parse_error_line(line: &str) -> Option<ParsedLine> {
    let caps = ERROR_LINE.captures(line)?;
    let mut out = ParsedLine::default();
    let mut extra = serde_json::Map::new();

    // 时间：本地时间（无时区后缀）
    let t = caps.get(1)?.as_str();
    out.ts = chrono::NaiveDateTime::parse_from_str(t, "%Y/%m/%d %H:%M:%S")
        .ok()
        .and_then(|naive| chrono::Local.from_local_datetime(&naive).single())
        .map(|dt| dt.timestamp())
        .unwrap_or_else(chrono_now);

    // 级别（error / warn / crit / alert / notice / info / debug）
    extra.insert(
        "level".to_string(),
        serde_json::Value::String(caps.get(2)?.as_str().to_string()),
    );

    let msg = caps.get(5).map(|m| m.as_str()).unwrap_or("");

    // 尾部 key: value 字段提取
    let find_str = |key: &str| -> Option<String> {
        let idx = msg.find(key)? + key.len();
        let rest = &msg[idx..];
        let end = rest.find(", ").unwrap_or(rest.len());
        let val = rest[..end].trim();
        if val.is_empty() || val == "-" {
            None
        } else {
            Some(val.to_string())
        }
    };
    let find_quoted = |key: &str| -> Option<String> {
        let idx = msg.find(key)? + key.len();
        let rest = &msg[idx..];
        if !rest.starts_with('"') {
            return None;
        }
        rest[1..].find('"').map(|end| rest[1..1 + end].to_string())
    };

    // client → remote_addr
    if let Some(client) = find_str("client: ") {
        out.remote_addr = Some(client);
    }
    // request → method/path/query/protocol
    if let Some(request) = find_quoted("request: ") {
        apply_request(&mut out, &request);
    }
    // server / host / upstream → extra
    for (key, val) in [
        ("server", find_str("server: ")),
        ("host", find_quoted("host: ")),
        ("upstream", find_quoted("upstream: ")),
    ] {
        if let Some(v) = val {
            extra.insert(key.to_string(), serde_json::Value::String(v));
        }
    }

    if !extra.is_empty() {
        out.extra = Some(serde_json::Value::Object(extra).to_string());
    }
    out.raw = Some(line.to_string());
    Some(out)
}

/// 按编译好的正则解析
fn parse_by_regex(line: &str, compiled: &CompiledFormat) -> Option<ParsedLine> {
    let caps = compiled.regex.captures(line)?;
    let get = |name: &str| -> Option<String> {
        caps.name(name).map(|m| m.as_str().to_string())
    };

    let mut out = ParsedLine::default();
    let mut extra = serde_json::Map::new();

    for var in &compiled.vars {
        let Some(val) = get(var) else { continue };
        // 跳过占位值
        let val = val.as_str();
        if val == "-" {
            continue;
        }
        match var.as_str() {
            "remote_addr" => out.remote_addr = Some(val.to_string()),
            "time_local" => out.ts = parse_time_local(val).unwrap_or_else(chrono_now),
            "time_iso8601" => {
                out.ts = parse_time_iso8601(val).unwrap_or_else(chrono_now)
            }
            "msec" => {
                // msec = "秒.毫秒"
                out.ts = val
                    .split('.')
                    .next()
                    .and_then(|s| s.parse::<i64>().ok())
                    .unwrap_or_else(chrono_now);
            }
            "request" => apply_request(&mut out, val),
            "status" => out.status = val.parse().ok(),
            "body_bytes_sent" | "bytes_sent" => out.bytes = val.parse().unwrap_or(0),
            "request_time" => out.request_time = val.parse().ok(),
            "http_referer" => out.referer = Some(val.to_string()),
            "http_user_agent" => out.user_agent = Some(val.to_string()),
            _ => {
                // 其它自定义变量存入 extra JSON
                extra.insert(var.clone(), serde_json::Value::String(val.to_string()));
            }
        }
    }

    if out.ts == 0 {
        out.ts = chrono_now();
    }
    if !extra.is_empty() {
        out.extra = Some(serde_json::Value::Object(extra).to_string());
    }
    out.raw = Some(line.to_string());
    Some(out)
}

/// 解析 JSON 行（nginx escape=json 格式，key 即变量名，不带 $）
fn parse_json_line(line: &str) -> Option<ParsedLine> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    let obj = v.as_object()?;

    let mut out = ParsedLine::default();
    let mut extra = serde_json::Map::new();

    let get_str = |key: &str| -> Option<String> {
        obj.get(key)
            .and_then(|x| x.as_str())
            .map(|s| s.to_string())
            .or_else(|| obj.get(key).and_then(|x| x.as_i64()).map(|n| n.to_string()))
            .or_else(|| {
                obj.get(key)
                    .and_then(|x| x.as_f64())
                    .map(|n| n.to_string())
            })
    };

    for (key, value) in obj {
        if value.is_null() {
            continue;
        }
        let s = match value {
            serde_json::Value::String(s) => s.clone(),
            serde_json::Value::Number(n) => n.to_string(),
            _ => continue,
        };
        let s = s.as_str();
        if s == "-" {
            continue;
        }
        match key.as_str() {
            "remote_addr" => out.remote_addr = Some(s.to_string()),
            "time_local" => out.ts = parse_time_local(s).unwrap_or_else(chrono_now),
            "time_iso8601" => out.ts = parse_time_iso8601(s).unwrap_or_else(chrono_now),
            "msec" => {
                out.ts = s
                    .split('.')
                    .next()
                    .and_then(|x| x.parse::<i64>().ok())
                    .unwrap_or_else(chrono_now);
            }
            "request" => apply_request(&mut out, s),
            "status" => out.status = get_str(key).and_then(|x| x.parse().ok()),
            "body_bytes_sent" | "bytes_sent" => out.bytes = s.parse().unwrap_or(0),
            "request_time" => out.request_time = s.parse().ok(),
            "http_referer" => out.referer = Some(s.to_string()),
            "http_user_agent" => out.user_agent = Some(s.to_string()),
            _ => {
                extra.insert(key.clone(), serde_json::Value::String(s.to_string()));
            }
        }
    }

    if out.ts == 0 {
        out.ts = chrono_now();
    }
    if !extra.is_empty() {
        out.extra = Some(serde_json::Value::Object(extra).to_string());
    }
    out.raw = Some(line.to_string());
    Some(out)
}

/// 拆分 $request 为 method / path / query / protocol
fn apply_request(out: &mut ParsedLine, request: &str) {
    let mut it = request.split_whitespace();
    let method = it.next();
    let uri = it.next();
    // protocol 取最后一个 HTTP/x.x token
    let protocol = request
        .split_whitespace()
        .filter(|p| p.starts_with("HTTP/"))
        .last();

    out.method = method.map(|s| s.to_string());
    if let Some(uri) = uri {
        match uri.split_once('?') {
            Some((p, q)) => {
                out.path = Some(p.to_string());
                out.query = Some(q.to_string());
            }
            None => out.path = Some(uri.to_string()),
        }
    }
    out.protocol = protocol.map(|s| s.to_string());
}

/// 解析 nginx $time_local（如 `21/Oct/2026:16:30:00 +0800`）
fn parse_time_local(s: &str) -> Option<i64> {
    // 用 DateTime 解析以正确应用 %z 时区偏移
    chrono::DateTime::parse_from_str(s, "%d/%b/%Y:%H:%M:%S %z")
        .ok()
        .map(|dt| dt.timestamp())
}

/// 解析 nginx $time_iso8601（如 `2026-10-21T16:30:00+08:00`）
fn parse_time_iso8601(s: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.timestamp())
}

/// 当前时间戳（秒），解析失败时的时间回退值
fn chrono_now() -> i64 {
    chrono::Utc::now().timestamp()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试辅助：Some(compiled) 按正则解析，None 走自动检测
    fn parse_line(line: &str, compiled: Option<&CompiledFormat>) -> Option<ParsedLine> {
        match compiled {
            Some(c) => parse_line_mode(line, &FormatMode::Compiled(c.clone())),
            None => parse_line_mode(line, &FormatMode::Auto),
        }
    }

    #[test]
    fn test_compile_combined() {
        let c = compile_format(DEFAULT_COMBINED).unwrap();
        assert!(c.vars.contains(&"remote_addr".to_string()));
        assert!(c.vars.contains(&"status".to_string()));
    }

    #[test]
    fn test_parse_combined_line() {
        let c = compile_format(DEFAULT_COMBINED).unwrap();
        let line = r#"203.0.113.5 - - [21/Oct/2026:16:30:00 +0800] "GET /api/users?id=3 HTTP/1.1" 200 1234 "https://example.com/" "Mozilla/5.0""#;
        let p = parse_line(line, Some(&c)).unwrap();
        assert_eq!(p.remote_addr.as_deref(), Some("203.0.113.5"));
        assert_eq!(p.method.as_deref(), Some("GET"));
        assert_eq!(p.path.as_deref(), Some("/api/users"));
        assert_eq!(p.query.as_deref(), Some("id=3"));
        assert_eq!(p.protocol.as_deref(), Some("HTTP/1.1"));
        assert_eq!(p.status, Some(200));
        assert_eq!(p.bytes, 1234);
        assert!(p.ts > 0);
    }

    #[test]
    fn test_parse_json_line() {
        let line = r#"{"time":"2026-10-21T16:30:00+08:00","remote_addr":"10.0.0.1","request":"POST /login HTTP/2.0","status":401,"body_bytes_sent":88,"http_user_agent":"curl/8.0","request_time":0.123,"upstream_addr":"10.0.0.9:8080"}"#;
        let p = parse_line(line, None).unwrap();
        assert_eq!(p.remote_addr.as_deref(), Some("10.0.0.1"));
        assert_eq!(p.status, Some(401));
        assert_eq!(p.request_time, Some(0.123));
        assert!(p.extra.is_some());
    }

    #[test]
    fn test_custom_format() {
        let fmt = "$remote_addr [$time_local] $request_time $upstream_response_time \"$request\" $status";
        let c = compile_format(fmt).unwrap();
        let line = r#"1.2.3.4 [21/Oct/2026:16:30:00 +0800] 0.050 0.049 "GET /x HTTP/1.1" 500"#;
        let p = parse_line(line, Some(&c)).unwrap();
        assert_eq!(p.status, Some(500));
        assert_eq!(p.request_time, Some(0.050));
        assert_eq!(p.path.as_deref(), Some("/x"));
    }

    #[test]
    fn test_bad_format() {
        assert!(compile_format("").is_err());
        assert!(compile_format("no vars here").is_err());
        assert!(compile_format("${unclosed").is_err());
    }

    #[test]
    fn test_parse_error_log_auto() {
        let line = r#"2026/10/22 09:00:00 [error] 1234#5678: *789 open() "/var/www/x" failed (2: No such file or directory), client: 192.168.1.5, server: example.com, request: "GET /x?y=1 HTTP/1.1", upstream: "http://127.0.0.1:8080", host: "example.com""#;
        let p = parse_line(line, None).unwrap();
        assert_eq!(p.remote_addr.as_deref(), Some("192.168.1.5"));
        assert_eq!(p.method.as_deref(), Some("GET"));
        assert_eq!(p.path.as_deref(), Some("/x"));
        assert_eq!(p.query.as_deref(), Some("y=1"));
        assert_eq!(p.protocol.as_deref(), Some("HTTP/1.1"));
        assert!(p.ts > 0);
        let extra = p.extra.unwrap();
        assert!(extra.contains(r#""level":"error""#));
        assert!(extra.contains(r#""server":"example.com""#));
        assert!(extra.contains(r#""host":"example.com""#));
    }

    #[test]
    fn test_parse_error_log_no_conn_id() {
        // 无 *连接号 的行（如 upstream 提前关闭）
        let line = r#"2026/10/22 09:00:01 [warn] 1234#5678: upstream server temporarily disabled"#;
        let p = parse_line(line, None).unwrap();
        assert!(p.remote_addr.is_none());
        let extra = p.extra.unwrap();
        assert!(extra.contains(r#""level":"warn""#));
    }

    #[test]
    fn test_error_preset_mode() {
        let mode = resolve_format("Error").unwrap();
        assert!(matches!(mode, FormatMode::Error));
        let line = r#"2026/10/22 09:00:00 [error] 1#2: *3 connect() failed, client: 10.0.0.9, request: "POST /api HTTP/1.1""#;
        let p = parse_line_mode(line, &mode).unwrap();
        assert_eq!(p.remote_addr.as_deref(), Some("10.0.0.9"));
        assert_eq!(p.method.as_deref(), Some("POST"));
        assert_eq!(p.status, None); // error.log 无状态码
        // "error" 不是合法 log_format，确认不会被当编译格式处理
        assert!(resolve_format("error ").is_ok() && matches!(resolve_format("error ").unwrap(), FormatMode::Error));
    }

    #[test]
    fn test_combined_line_not_detected_as_error_log() {
        let line = r#"203.0.113.5 - - [21/Oct/2026:16:30:00 +0800] "GET / HTTP/1.1" 200 1234 "-" "curl/8.0""#;
        let p = parse_line(line, None).unwrap();
        assert_eq!(p.status, Some(200));
        assert!(p.extra.is_none()); // combined 走正则路径，没有 error.log 的 level/server extra
    }
}
