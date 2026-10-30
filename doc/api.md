# Nginx 日志分析服务接口文档

> 项目：`nginx_log_rs`（Rust + Axum + SQLite/WAL）
> 功能：后台线程周期性增量解析 nginx 日志文件（log_format 正则编译 / JSON 自动检测），结构化存入 SQLite，Web 侧边栏多页面仪表盘展示统计，支持多站点数据隔离。

---

## 1. 概述

- **数据来源**：在网页「站点与来源」页中添加站点与 nginx 日志文件（存 SQLite `settings` 表，**热生效**，无需重启）。
- **多站点**：站点（site）→ 日志源（source）两级结构，一个站点可挂多个日志源；统计/明细按站点隔离（站点 → 其下 source_id 集合过滤，日志表无需站点列）。`site_id=0` 表示未分组，只在「全部站点」中出现。
- **后台解析**：tokio 任务按配置间隔（默认 60s）增量读取每个日志源（记录字节偏移断点，自动处理日志轮转与文件末尾半行），解析结果批量写入 `access_logs` 表。**解析进度持久化在 `parse_state` 表，重启服务后从断点继续，不会从头重读**；大文件解析期间**每 5000 行实时更新一次进度**（页面进度可见）。解析轮次/手动重解析均带 panic 防护——单轮崩溃只记 error 日志，后台任务不会静默死亡。
- **鉴权**：Web 登录（session cookie）保护页面与所有查询 API；`config.toml [web]` username/password 任一为空时关闭登录保护。**会话滑动续期**：任何已登录请求（打开页面 / 调 API）在令牌剩余有效期不足一半时，服务端自动在响应头续发新 cookie——只要持续使用就无需定期重新登录；`session_days` 只约束「最长不活跃时间」。
- **配置**：`config.toml` 只保留 `[server] [sqlite] [web]`；站点与日志源配置全部在网页管理。

## 2. 数据表

| 表 | 用途 |
|----|------|
| `settings` | KV 配置（`nginx_sources` = 站点 + 日志源配置 JSON） |
| `access_logs` | 解析后的访问日志：`source_id, ts(unix秒), remote_addr, method, path, query, protocol, status, bytes, referer, user_agent, request_time, extra(JSON), raw`（索引 `(source_id, ts)` 支撑站点隔离过滤） |
| `parse_state` | 每源解析进度：`offset, last_parse_at, lines_total, lines_failed, last_error` |
| `ip_geo` | IP 归属地缓存：`ip(PK), country, region, city, isp, queried_at`，**查到即永久有效，不重复外查** |

## 3. 接口列表

### 3.1 页面

| 方法 | 路径 | 说明 |
|------|------|------|
| GET | `/` | 统计仪表盘（未登录 302 → /login） |
| GET | `/login` / POST `/login` | 登录页 / 提交登录 |
| POST | `/logout` | 退出登录 |
| GET | `/health` `/version` | 健康检查 / 版本（免登录） |

### 3.2 统计

**GET /api/stats/overview?range=24h&site_id=1**

`range`：`1h` `6h` `24h` `7d` `30d`（默认 24h）；
`site_id` 可选，只统计该站点（其下全部日志源）；`source_id` 可选，精确过滤单个日志源（优先级高于 site_id）。

响应 `data`：

```json
{
  "start_ts": 1792488636,
  "bucket_secs": 900,
  "kpi": { "pv": 12, "uv": 7, "bytes": 11324, "errors": 4,
           "avg_request_time": 0.064, "p95_request_time": 0.123 },
  "status_dist": [{ "status": 200, "count": 6 }],
  "top_paths":  [{ "key": "GET /health", "count": 2, "bytes": 30 }],
  "top_ips":    [{ "key": "203.0.113.5", "count": 2, "bytes": 2468 }],
  "series":     [{ "ts": 1792571400, "pv": 10, "errors": 3, "bytes": 10419 }],
  "recent_errors": [{ "ts": 0, "remote_addr": "...", "method": "...", "path": "...", "query": null, "status": 502, "user_agent": "..." }]
}
```

### 3.3 站点与日志源配置

**GET /api/sources** — 当前配置 + 各源解析状态：

```json
{
  "code": 0,
  "data": {
    "interval_secs": 60,
    "auto_cleanup_enabled": true,
    "auto_cleanup_time": "04:00",
    "auto_cleanup_days": 90,
    "sites":   [{ "id": 1, "name": "example.com", "note": "主站" }],
    "sources": [{ "id": 1, "site_id": 1, "name": "站点A", "path": "/var/log/nginx/access.log",
                  "format": "", "enabled": true, "start_at_end": false }],
    "states": [{ "source_id": 1, "offset": 901, "last_parse_at": "...",
                 "lines_total": 8, "lines_failed": 0, "last_error": null }]
  }
}
```

**POST /api/sources** — 保存配置并热生效（后台下一轮立即按新配置解析）：

```json
{
  "interval_secs": 60,
  "sites":   [ ... 新增站点 id 传 0 由后端分配 ... ],
  "sources": [ ... 新增源 id 传 0 由后端分配，site_id 引用站点 id（0=未分组）... ]
}
```

- 校验：站点名称非空且不重名（忽略大小写）；path 非空且**不能与其它日志源重复**（同一文件重复添加会对同一文件重复解析、数据翻倍）；source.site_id 必须存在于 sites（或 0）；format 非空时必须合法；interval 限 5–86400 秒
- **自动清理**（配置随本接口保存，存于 `nginx_sources` JSON）：`auto_cleanup_enabled`（启用开关）、`auto_cleanup_time`（每天执行时刻，HH:MM，默认 04:00）、`auto_cleanup_days`（保留最近 N 天，1–3650，默认 90）。启用后由后台解析任务每天在配置时刻删除 N 天前的日志（删除 ≥ 阈值自动 VACUUM，与手动清理一致）；同一天只执行一次，服务重启时若已错过当天时刻会补执行一次；执行记录 INFO 日志「自动清理：删除 N 天前的日志 X 条」
- 保存时自动清理已删除源留下的孤儿数据（`parse_state` 进度 + `access_logs` 残留日志行）
- 删除站点前应先移走其下日志源（前端会自动把源归入「未分组」）
- **format 三选一**：
  - 留空 `""` = 自动检测：JSON 行 → nginx 原生 error.log → combined
  - `"error"` = 强制按 nginx 原生 error.log 解析（`2026/10/22 09:00:00 [error] 123#456: ... client: IP, request: "GET /x HTTP/1.1"`；时间按本地时区；无状态码，级别/服务器/域名存 `extra`）
  - 其它 = access log 的 log_format 字符串
- **同一站点可挂多个源、各用各的格式**（如 access.log 配 combined、error.log 配 `error`、自定义格式日志配各自的 log_format），站点统计自动合并名下全部源
- `start_at_end`：新增源首次解析是否从文件末尾开始（true 不解析历史）

### 3.4 访问明细

**GET /api/logs?range=24h&site_id=1&status=-400&q=missing&page=1&page_size=50**

| 参数 | 说明 |
|------|------|
| `range` | 同统计接口；提供 `start_ts` 后忽略 |
| `start_ts` / `end_ts` | 自定义起止时间（unix 秒，可选），**优先于 range**；只传其一则另一端不限 |
| `site_id` / `source_id` | 站点 / 日志源过滤（可选） |
| `status` | `-400` = 仅错误（≥400）；也可传精确状态码如 `404` |
| `q` | 模糊搜索 path / remote_addr / user_agent（LIKE %q%） |
| `page` / `page_size` | 分页，page_size 最大 200，默认 50 |

响应 `data`：`{ "rows": [{ "id", "ts", "source_id", "remote_addr", "method", "path", "query", "protocol", "status", "bytes", "referer", "user_agent", "request_time" }], "total": 42 }`（按 ts 倒序）

### 3.5 IP 归属地

**GET /api/ipgeo?ips=8.8.8.8,1.2.3.4** — 批量查询（最多 200 个）

- 缓存于 `ip_geo` 表，**查到即永久有效**：命中缓存直接复用不再外查（`queried_at` 仅记录首次查询时间）
- 外查用 ip9.com.cn 接口 `https://ip9.com.cn/get?ip=<ip>`（单 IP 查询，UTF-8 JSON，直连无需代理，**支持 IPv4/IPv6**）
  - `country`=国家（中国/美国/保留…）、`prov`=省、`city`=市、`isp`=运营商
- **限速兼容（官方限速 60 次/秒，本地留余量）**：
  - 本地滑动窗口限 **50 次/秒** + 相邻请求最小间隔 20ms
  - 单次接口调用最多同步外查 **50 个** IP（明细页一页可全部补齐；更多下次请求/刷新自动补查）
  - 请求失败（非 200 / 空响应 / 非 JSON）视为疑似触发对方限速：进入**冷却期**（60s 起，连续失败指数翻倍，上限 15 分钟），冷却期内不外查、不写缓存，到期自动重试
  - 失败的 IP **不写缓存**（区别于查询成功但无结果的空值缓存），下次请求自动补查
- 私网 / 保留地址（127/8、10/8、172.16/12、192.168/16、::1、fc00::/7 等）不外查，直接标注「内网/保留地址」，且不受外查失败影响

```json
{ "code": 0, "data": { "map": {
    "240e:471:eb0:cc03:704d:8ff:fe0e:1af4": { "country": "中国", "region": "浙江", "city": "杭州", "isp": "中国电信" },
    "8.8.8.8": { "country": "美国", "region": null, "city": null, "isp": "Google Cloud" },
    "192.168.1.10": { "country": null, "region": null, "city": null, "isp": "内网/保留地址" }
} } }
```

### 3.6 测试解析

**POST /api/sources/test** — 测试解析（读取文件末尾 256KB 试解析）：

```json
// 请求
{ "path": "/var/log/nginx/access.log", "format": "$remote_addr - ... [$time_local] ...", "max_lines": 50 }
// 响应
{ "code": 0, "message": "8/8 行解析成功", "total_lines": 8, "matched": 8,
  "failed_samples": [], "parsed_samples": [{ "ts": 1792571400, "remote_addr": "203.0.113.5", ... }] }
```

**POST /api/sources/{id}/reparse** — 手动重解析单个源：

- 删除该源已入库的全部数据（`access_logs WHERE source_id = id`）
- 重置解析进度（`parse_state` 该源记录清空，offset 归零）
- 通知后台解析任务**立即**从文件头解析整个文件（不用等下一轮间隔；`start_at_end` 设置在手动重解析时被忽略）
- 响应：`{ "code": 0, "deleted": 123 }`（deleted = 本次清除的行数）

### 3.7 清理与状态

| 方法 | 路径 | 说明 |
|------|------|------|
| DELETE | `/api/logs/cleanup?days=90` | 删除 N 天前的日志（达到阈值自动 VACUUM，全站点） |
| GET | `/api/db/size` | 数据库文件大小（db + wal 字节数） |

## 4. log_format 解析说明

格式串中的 `$var` / `${var}` 按上下文编译为正则捕获组：

| 上下文 | 匹配模式 | 示例 |
|--------|----------|------|
| 引号内（后随 `"`） | `[^"]*` | `"$request"` `"$http_user_agent"` |
| 方括号内（后随 `]`） | `[^\]]*` | `[$time_local]` |
| 数值变量 status/body_bytes_sent/bytes_sent | `\d+` | `$status` |
| 其余 | `\S+` | `$remote_addr` `$request_time` |

已知变量直接映射到结构化列；**其它任意自定义变量**（如 `$upstream_addr`）存入 `extra` JSON 列。JSON 日志（`escape=json`）按 key 同名提取。

`$time_local`（`%d/%b/%Y:%H:%M:%S %z`）、`$time_iso8601`、`$msec` 均可解析为时间戳；都缺失时取当前时间。

## 5. 配置文件（config.toml）

只保留 `[server] [sqlite] [web]` 三节，**全部字段均在代码中使用**，无可清理项：

| 节 | 字段 | 用途 | 使用位置 |
|----|------|------|----------|
| `[server]` | `host` / `port` | HTTP 服务监听地址/端口 | `main.rs` 启动绑定 |
| `[sqlite]` | `path` | 数据库文件路径 | `database::init_db`、`/api/db/size` |
| `[sqlite]` | `vacuum_after_delete` | 删除日志后是否自动 VACUUM 整理碎片 | `delete_logs_before` |
| `[sqlite]` | `vacuum_min_deleted` | 删除条数达到该值才触发 VACUUM | 同上 |
| `[sqlite]` | `wal_autocheckpoint` | WAL 自动 checkpoint 阈值（页） | `apply_wal_tuning`（写/读连接） |
| `[sqlite]` | `journal_size_limit` | WAL 文件字节硬上限 | 同上 |
| `[sqlite]` | `wal_checkpoint_enabled` | WAL 全量回写主库并截断 `-wal` 文件的总开关（默认 true）；关闭则启动与定时都不执行 | `watcher` 启动 + 每轮检查 |
| `[sqlite]` | `wal_checkpoint_time` | 每天定时执行上述 checkpoint 的时刻 `HH:MM`（默认 `04:30`，建议与自动清理错开） | `watcher` 每轮检查 |

> **WAL 合并时机**：① 服务启动时立即执行一次（此时尚未产生业务读连接，截断成功率最高，可收敛上次异常退出留下的大 `-wal`）；② 之后每天在 `wal_checkpoint_time` 时刻执行一次。两者都是 `PRAGMA wal_checkpoint(TRUNCATE)`，遇并发读连接无法截断时会补做一次 PASSIVE 回写。
| `[web]` | `username` / `password` | 登录凭据，任一为空 = 关闭登录保护 | `auth.rs` 登录/中间件 |
| `[web]` | `session_days` | 登录态有效期（天）；带滑动续期，持续使用不强制重登 | `auth.rs` 签发 cookie |

站点与日志源配置**不在本文件**，全部在网页「站点与来源」页管理（存 SQLite `settings` 表，热生效）。
测试环境：`RUN_MODE=test` 叠加 `config.test.toml`（端口 18080、独立数据库 `data/test_e2e.db`）。

## 6. 运行

```bash
cargo run                    # 读取 config.toml
RUN_MODE=test cargo run      # 叠加 config.test.toml（如测试端口/独立数据库）
cargo test                   # 单元测试（parser 等）
```

后端构建（musl 静态）见 `build.sh` / `docker-compose.yml`。

## 7. 重要操作日志（logs/app_*.log）

以下操作在后台记录日志（flexi_logger，文件在 `logs/` 目录，同时输出到终端）：

| 操作 | 级别 | 内容 |
|------|------|------|
| 登录成功 / 失败 | INFO / WARN | 用户名、会话有效期 / 失败时记录提交的用户名 |
| 保存日志源配置 | INFO | 站点数 / 源数 / 解析间隔（热生效） |
| 手动重解析 | INFO | 清除的历史行数、重新解析的行数与失败数 |
| 日志清理 | INFO | 删除条数、VACUUM 前后 freelist |
| 自动清理（定时） | INFO/ERROR | 删除天数与条数、执行时刻；失败记 ERROR 并次日重试 |
| WAL 定时 checkpoint | INFO/WARN | 回写页数、`-wal` 文件前后大小；遇并发读连接无法截断时 WARN 并补做 PASSIVE |
| 解析轮次 | INFO/WARN/ERROR | 每源新增/失败行数、解析错误、panic（已捕获） |

## 8. 前端展示约定

- **明细页**：滚动只发生在表格区域内（筛选条与分页固定，表头吸顶）；UA 列窄列截断、鼠标悬浮 `title` 显示完整内容；归属地只显示「国家 省 市」（中文）；「来源」列直接显示所属站点名
- **概览页**：「最近错误请求」卡片与图表卡片等宽等高，表格在卡片内部滚动
- 品牌名 NginxLog，深色/浅色主题（localStorage `nginxlog.theme.v1`），明细每页条数记忆（`nginxlog.logPageSize`）
