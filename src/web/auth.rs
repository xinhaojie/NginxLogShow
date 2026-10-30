// src/web/auth.rs
//! Web 登录认证模块
//!
//! 设计要点：
//! - session 采用「签名 cookie」方案：token = `<username>.<过期时间戳>.<HMAC-SHA256 签名>`
//!   - 服务端无 session 存储，重启不丢登录态
//!   - 签名密钥 = `username:password`（改密码即全部失效，符合预期）
//!   - 有效期由配置 `[web] session_days` 控制，token 内嵌过期时间
//! - 保护范围：除 /login、/logout、/health、/version 外全部需要登录
//! - [web] username/password 任一为空时，登录保护整体关闭（内网调试用）

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::{
    extract::{Request, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    middleware::Next,
    response::{Html, IntoResponse, Redirect, Response},
    Json,
};
use hmac::{Hmac, Mac};
use serde::Deserialize;
use serde_json::json;
use sha2::Sha256;

use crate::state::AppState;

/// cookie 名
const COOKIE_NAME: &str = "nginxlog_session";

type HmacSha256 = Hmac<Sha256>;

// ============================================================
// session token 签发 / 校验
// ============================================================

/// 当前 Unix 时间戳（秒）
fn now_ts() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// HMAC-SHA256 签名
fn sign(secret: &str, payload: &str) -> [u8; 32] {
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC key 初始化失败");
    mac.update(payload.as_bytes());
    mac.finalize().into_bytes().into()
}

/// 小写 hex 编码
fn hex(data: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(data.len() * 2);
    for b in data {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0x0f) as usize] as char);
    }
    s
}

/// 恒定时间比较
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// 签发 session token：`<username>.<exp_ts>.<sig>`
pub fn issue_token(username: &str, secret: &str, max_age_secs: i64) -> String {
    let exp = now_ts() + max_age_secs;
    let payload = format!("{}.{}", username, exp);
    format!("{}.{}", payload, hex(&sign(secret, &payload)))
}

/// 取出 token 内嵌的过期时间戳（不校验签名，签名由 `verify_token` 负责）
///
/// token 形如 `<username>.<exp>.<sig>`：先从右拆掉签名，再从右拆出 exp
pub fn token_exp(token: &str) -> Option<i64> {
    let (payload, _sig) = token.rsplit_once('.')?;
    let (_, exp_str) = payload.rsplit_once('.')?;
    exp_str.parse::<i64>().ok()
}

/// 校验 session token：格式合法、未过期、签名匹配
pub fn verify_token(token: &str, secret: &str) -> bool {
    // token = `<payload>.<sig>`，payload = `<username>.<exp>`
    let mut parts = token.rsplitn(2, '.');
    let Some(sig) = parts.next() else { return false };
    let Some(payload) = parts.next() else { return false };

    // payload 拆出过期时间（username 含 '.' 也不影响，从右拆分）
    let Some((_, exp_str)) = payload.rsplit_once('.') else {
        return false;
    };
    let Ok(exp) = exp_str.parse::<i64>() else {
        return false;
    };
    if exp < now_ts() {
        return false;
    }

    let expected = sign(secret, payload);
    match decode_hex(sig) {
        Some(sig_bytes) => ct_eq(&sig_bytes, &expected),
        None => false,
    }
}

/// 单个 hex 字符转数值
fn hex_val(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// hex 解码，非法输入返回 None
fn decode_hex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len() / 2);
    let mut i = 0;
    while i < bytes.len() {
        let hi = hex_val(bytes[i])?;
        let lo = hex_val(bytes[i + 1])?;
        out.push((hi << 4) | lo);
        i += 2;
    }
    Some(out)
}

/// 从 Cookie header 中提取指定 cookie 的值
fn extract_cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    let cookie_str = headers.get(header::COOKIE)?.to_str().ok()?;
    for pair in cookie_str.split(';') {
        let pair = pair.trim();
        if let Some((k, v)) = pair.split_once('=') {
            if k.trim() == name {
                return Some(v.trim().to_string());
            }
        }
    }
    None
}

/// 构造 Set-Cookie header（登录/登出共用）
fn cookie_header(token: &str, max_age: i64) -> HeaderValue {
    let cookie = format!(
        "{}={}; Path=/; HttpOnly; SameSite=Lax; Max-Age={}",
        COOKIE_NAME, token, max_age
    );
    HeaderValue::from_str(&cookie).expect("cookie header 构造失败")
}

// ============================================================
// 登录 / 登出 handler
// ============================================================

/// GET /login — 登录页
pub async fn login_page() -> Html<&'static str> {
    Html(include_str!("../../static/login.html"))
}

#[derive(Debug, Deserialize)]
pub struct LoginReq {
    pub username: String,
    pub password: String,
}

/// POST /login — 校验用户名密码，成功下发 session cookie
pub async fn login(
    State(state): State<Arc<AppState>>,
    Json(req): Json<LoginReq>,
) -> Response {
    let web = &state.app_config.web;

    // 未启用登录保护时，任何请求直接视为登录成功
    if web.username.is_empty() || web.password.is_empty() {
        return (
            StatusCode::OK,
            Json(json!({"code": 0, "message": "success"})),
        )
            .into_response();
    }

    // 用户名密码校验
    if req.username != web.username || req.password != web.password {
        log::warn!("登录失败：用户名或密码错误（提交用户名：{}）", req.username);
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"code": 401, "message": "用户名或密码错误"})),
        )
            .into_response();
    }

    // 签名密钥 = 用户名:密码（改密码后所有旧 session 立即失效）
    let secret = format!("{}:{}", web.username, web.password);
    let max_age = web.session_days.max(1) * 86400;
    let token = issue_token(&web.username, &secret, max_age);

    let mut headers = HeaderMap::new();
    headers.insert(header::SET_COOKIE, cookie_header(&token, max_age));
    log::info!("用户 {} 登录成功（会话有效期 {} 天）", web.username, web.session_days.max(1));

    (
        StatusCode::OK,
        headers,
        Json(json!({"code": 0, "message": "success", "username": web.username})),
    )
        .into_response()
}

/// POST /logout — 清除 session cookie
pub async fn logout() -> Response {
    let mut headers = HeaderMap::new();
    headers.insert(header::SET_COOKIE, cookie_header("", 0));
    (
        StatusCode::OK,
        headers,
        Json(json!({"code": 0, "message": "success"})),
    )
        .into_response()
}

// ============================================================
// 登录保护中间件
// ============================================================

/// 未登录时页面 302 到 /login，API 返回 401
pub async fn require_login(State(state): State<Arc<AppState>>, req: Request, next: Next) -> Response {
    let web = &state.app_config.web;

    // 未启用登录保护（用户名或密码为空）→ 全部放行
    if web.username.is_empty() || web.password.is_empty() {
        return next.run(req).await;
    }

    let path = req.uri().path().to_string();

    // 放行：登录页 / 登录提交 / 登出 / 健康检查 / 版本
    if path == "/login" || path == "/logout" || path == "/health" || path == "/version" {
        return next.run(req).await;
    }

    // 其余路径校验 session cookie
    let secret = format!("{}:{}", web.username, web.password);
    let token = extract_cookie(req.headers(), COOKIE_NAME);
    let authed = token
        .as_deref()
        .map(|t| verify_token(t, &secret))
        .unwrap_or(false);

    if authed {
        let mut resp = next.run(req).await;
        // ---- 滑动续期 ----
        // 只要持续使用（打开页面 / 任何 API 请求）就自动延长会话：
        // 剩余有效期不足一半时重新下发 cookie，避免「每过 N 天就要重新登录」。
        if let Some(t) = token {
            let max_age = web.session_days.max(1) * 86400;
            let remaining = token_exp(&t)
                .map(|exp| exp - now_ts())
                .unwrap_or(0);
            if remaining < max_age / 2 {
                let new_token = issue_token(&web.username, &secret, max_age);
                resp.headers_mut()
                    .insert(header::SET_COOKIE, cookie_header(&new_token, max_age));
                log::debug!(
                    "会话自动续期（剩余 {}s < 有效期一半 {}s），新有效期 {} 天",
                    remaining,
                    max_age / 2,
                    web.session_days.max(1)
                );
            }
        }
        return resp;
    }

    // 未登录
    if path.starts_with("/api/") {
        // API 请求返回 401 JSON
        (
            StatusCode::UNAUTHORIZED,
            Json(json!({"code": 401, "message": "未登录或登录已过期"})),
        )
            .into_response()
    } else {
        // 页面请求重定向到登录页
        Redirect::to("/login").into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::{issue_token, now_ts, token_exp, verify_token};

    const SECRET: &str = "alik:pwd";

    #[test]
    fn test_issue_and_verify() {
        let t = issue_token("alik", SECRET, 3600);
        assert!(verify_token(&t, SECRET), "自身签发的 token 应校验通过");
        assert!(!verify_token(&t, "alik:wrong"), "密钥不同应校验失败");
    }

    #[test]
    fn test_token_exp() {
        let max_age = 3600;
        let t = issue_token("alik", SECRET, max_age);
        let exp = token_exp(&t).expect("应能解析出过期时间");
        let now = now_ts();
        assert!(
            (exp - now - max_age).abs() <= 2,
            "过期时间应约为 now+max_age，实际差 {}",
            exp - now - max_age
        );

        // 用户名含 '.' 也能正确取到最后一段作为 exp
        let t2 = issue_token("a.b.c", SECRET, max_age);
        let exp2 = token_exp(&t2).expect("用户名含点时也应解析成功");
        assert!((exp2 - now_ts() - max_age).abs() <= 2);

        // 非法 token
        assert_eq!(token_exp("abc"), None);
        assert_eq!(token_exp("abc.notanum.sig"), None);
    }

    #[test]
    fn test_expired_token_rejected() {
        // 手动构造一个已过期的 token（exp = now - 10）
        let exp = now_ts() - 10;
        let payload = format!("alik.{}", exp);
        let sig = super::hex(&super::sign(SECRET, &payload));
        let t = format!("{}.{}", payload, sig);
        assert!(!verify_token(&t, SECRET), "已过期的 token 应被拒绝");
        assert_eq!(token_exp(&t), Some(exp), "过期 token 仍能读出 exp（供续期判断）");
    }
}
