use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use axum::{
    extract::{ConnectInfo, Request, State},
    http::StatusCode,
    middleware::Next,
    response::Response,
};
use log::warn;

/// 单个IP的请求记录
#[derive(Debug, Clone)]
struct IpRecord {
    /// 请求时间戳列表（滑动窗口）
    requests: Vec<Instant>,
}

impl IpRecord {
    fn new() -> Self {
        Self {
            requests: Vec::new(),
        }
    }

    /// 清理过期请求，只保留窗口内的请求
    fn cleanup(&mut self, window: Duration) {
        let now = Instant::now();
        self.requests.retain(|&t| now.duration_since(t) < window);
    }

    /// 获取当前窗口内的请求数量
    fn request_count(&self) -> usize {
        self.requests.len()
    }

    /// 添加新的请求记录
    fn add_request(&mut self) {
        self.requests.push(Instant::now());
    }
}

/// 单个接口的限速配置
#[derive(Debug, Clone)]
pub struct EndpointRateConfig {
    /// 时间窗口大小
    pub window: Duration,
    /// 窗口内允许的最大请求数
    pub max_requests: usize,
}

impl EndpointRateConfig {
    pub fn new(window: Duration, max_requests: usize) -> Self {
        Self {
            window,
            max_requests,
        }
    }
}

/// 限速器核心结构
#[derive(Clone)]
pub struct RateLimiter {
    /// 每个接口的限速配置
    endpoint_configs: Arc<HashMap<String, EndpointRateConfig>>,
    /// 每个接口每个IP的请求记录: endpoint_path -> ip -> record
    records: Arc<Mutex<HashMap<String, HashMap<IpAddr, IpRecord>>>>,
}

impl RateLimiter {
    pub fn new(endpoint_configs: HashMap<String, EndpointRateConfig>) -> Self {
        Self {
            endpoint_configs: Arc::new(endpoint_configs),
            records: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// 检查IP在指定接口上是否允许请求
    pub async fn check_rate_limit(&self, endpoint_path: &str, ip: IpAddr) -> bool {
        // 获取该接口的限速配置
        let config = match self.endpoint_configs.get(endpoint_path) {
            Some(config) => config,
            None => return true, // 没有配置限速的接口直接放行
        };

        let mut all_records = self.records.lock().await;
        let endpoint_records = all_records.entry(endpoint_path.to_string()).or_insert_with(HashMap::new);
        
        let record = endpoint_records.entry(ip).or_insert_with(IpRecord::new);
        
        // 清理过期请求
        record.cleanup(config.window);
        
        // 检查是否超过限制
        if record.request_count() >= config.max_requests {
            warn!("Rate limit exceeded for IP: {} on endpoint: {}", ip, endpoint_path);
            return false;
        }
        
        // 记录本次请求
        record.add_request();
        true
    }

}

/// 从请求中提取真实客户端IP
/// 优先从代理请求头获取，如果没有则使用直连IP
fn extract_real_ip(
    request: &Request,
    connect_info_ip: IpAddr,
) -> IpAddr {
    // 优先级: X-Real-IP > X-Forwarded-For(第一个) > ConnectInfo
    
    // 尝试 X-Real-IP
    if let Some(x_real_ip) = request.headers().get("X-Real-IP") {
        if let Ok(ip_str) = x_real_ip.to_str() {
            if let Ok(ip) = ip_str.trim().parse::<IpAddr>() {
                return ip;
            }
        }
    }
    
    // 尝试 X-Forwarded-For (取第一个IP，即客户端真实IP)
    if let Some(x_forwarded_for) = request.headers().get("X-Forwarded-For") {
        if let Ok(header_value) = x_forwarded_for.to_str() {
            // X-Forwarded-For 格式: client, proxy1, proxy2
            if let Some(first_ip) = header_value.split(',').next() {
                if let Ok(ip) = first_ip.trim().parse::<IpAddr>() {
                    return ip;
                }
            }
        }
    }
    
    // 回退到直连IP
    connect_info_ip
}

/// Axum中间件：限速检查（全局）
pub async fn rate_limit_middleware(
    State(limiter): State<Arc<RateLimiter>>,
    ConnectInfo(addr): ConnectInfo<std::net::SocketAddr>,
    request: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    // 提取真实客户端IP（支持反向代理场景）
    let ip = extract_real_ip(&request, addr.ip());
    let path = request.uri().path().to_string();
    let limit_key = path.clone();

    if !limiter.check_rate_limit(&limit_key, ip).await {
        warn!("Rate limit exceeded for IP: {} on path: {}", ip, path);
        return Err(StatusCode::TOO_MANY_REQUESTS);
    }
    
    Ok(next.run(request).await)
}

pub fn rate_limit_config() -> HashMap<String, EndpointRateConfig> {
    let mut config = HashMap::new();
    // 配置保存：每个 IP 每 10 秒最多 20 次
    config.insert(
        "/api/sources".to_string(),
        EndpointRateConfig::new(Duration::from_secs(10), 20),
    );
    config
}