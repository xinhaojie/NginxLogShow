# nginx_log_rs 项目笔记

## 项目定位
原为「HTTP JSON 日志上报 + 查看」服务；2026-10-21 改造为 **nginx 日志分析服务**（类似 nginx.futuremorph.cn）：
后台线程周期增量解析 nginx 日志文件（log_format 编译正则 / JSON 自动检测）→ SQLite → Web 仪表盘统计。
日志源配置存 settings 表、网页管理、热生效；config.toml 只有 [server] [sqlite] [web]。

## 构建 / 运行环境（重要）
- **Windows 本机没有 cargo，必须在 WSL Debian 编译**：
  `wsl.exe -e bash -lc "cd /mnt/e/self_develop/rust/nginx_log_rs && cargo build"`
- WSL /tmp 随会话结束清空；git-bash 与 WSL 的 /tmp 互不相通
- WSL 中 pkill/pgrep -f 会匹配自身命令行，用 `-x nginx_log_rs`

## 代码结构
- `src/nginx/`：parser.rs（log_format→正则 + JSON 检测）、watcher.rs（增量解析后台任务）
- `src/web/`：auth.rs（HMAC cookie session）、handler.rs、stats.rs（SQL 聚合）、ip_geo.rs（归属地）
- `src/database/`：settings / access_logs / parse_state / ip_geo 四表，WAL 调优
- 前端 `static/index.html` 仪表盘（ECharts），登录页 login.html；主题 localStorage key `nginxlog.theme.v1`
- Web 登录：alik_morph / Aa@1qaz2wsx（config.toml [web]）
- IP 归属地：ip9.com.cn（HTTPS 直连、UTF-8 JSON、支持 IPv6，无需代理）；官方限速 60 次/秒，本地 50 次/秒滑动窗口 + 失败冷却指数退避（60s→15min）；**缓存查到即永久有效，无 TTL**（2026-10-23 用户要求取消 10 天有效期）
- 定时后台任务（都在 watcher 每轮解析后检查，同天幂等、重启补执行）：自动清理旧日志（配置在网页/库）、WAL checkpoint（`config.toml [sqlite] wal_checkpoint_*`）
- 测试示例日志在 `testdata/`；接口文档 `doc/api.md`
