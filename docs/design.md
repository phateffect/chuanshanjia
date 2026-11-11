# chuanshanjia 设计文档

本文描述当前纯 CLI 仓库的架构、关键设计决策及其动机。使用方法见 [README](../README.md)；外部 HTTP 服务的完整行为契约及 Python 重实现说明见 [backend-migration](backend-migration.md)。

## 1. 背景与目标

微服务日志分散在不同存储中。当前项目只承担其中一层：为 Agent 和人提供稳定、可组合、少惊扰的命令行入口。

- 用 `chuanshanjia services` 列出后端支持的服务；
- 用 `chuanshanjia sellers` 按 sellerNick、sellerId、storeId 或 leyanSellerId 查卖家；
- 用统一的 `chuanshanjia search <service> -f k=v -k kw` 表达查询；
- 用 `chuanshanjia trace <service> <trace_id>` 做跨服务追踪；
- 将参数、时间和 token 归一化后调用外部 HTTP 服务；
- 将服务端 JSON 原样作为 CLI 的结构化结果输出；
- 用标准 OIDC 流程降低手工分发和刷新 token 的成本。

### 非目标

- 本仓库不实现 HTTP 服务端、storage provider、查询翻译或日志清洗；
- 不直接连接 Elasticsearch、SLS、TLS，不保存云服务凭证；
- 不做完整 ES DSL、日志采集、存储、告警或管理界面；
- CLI 不签发 token，也不代替服务端做权限判定。

## 2. 总体架构

```text
┌──────────────┐   GET /api/services               ┌────────────────────┐
│ chuanshanjia │   GET /api/sellers                │ 外部 HTTP 查询服务 │
│ CLI          │   POST /api/services/{s}/_search  │ （其他项目实现）   │
│              │   POST /api/services/{s}/_trace   │                    │
│              │ ────────────────────────────▶ │                    │
└──────────────┘   Authorization: Bearer JWT       └────────────────────┘
       │
       ▼
~/.config/chuanshanjia/
  config.toml
  credentials.toml
       │
       └──────── OIDC authorization code + PKCE ──▶ IdP
```

仓库是单一 Rust package，只产生 `chuanshanjia` 二进制：

| 文件 | 职责 |
| --- | --- |
| `src/main.rs` | usage-rs 参数、配置、时间归一化、HTTP 调用、输出及子命令编排 |
| `src/oidc.rs` | OIDC discovery、授权码 + PKCE、loopback 回调、token 存储/刷新/吊销 |
| `tests/cli.rs` | 真实 CLI 二进制的端到端测试，外部服务与 IdP 均使用 mock |

服务端职责已经迁出。为避免重写时丢失语义，迁移前的配置、认证、provider、响应和错误行为独立记录在 `docs/backend-migration.md`，不再混入当前 CLI 架构。

## 3. 查询设计

### 3.1 CLI 查询模型

CLI 保留最小查询模型：service、可重复的等值 filter、单个 keyword 和时间范围。

- `-f KEY=VALUE` 在第一个 `=` 分割，key 不能为空，value 始终按字符串发送；
- `-f` 与 `-k` 至少一个，keyword 最多出现一次；
- 请求体是封闭扁平 schema：等值 filter 键（`env`/`version`/`level`/`host`/`trace_id`/`span_id`）作为顶层字段发送，另加 `keyword`、`start`、`end`；未知字段由服务端返回 422 拒绝；
- CLI 不在本地执行 service 预置合并、provider 查询或响应清洗，这些都是外部服务的职责。

卖家查询是独立的只读命令：`sellers <SELLER> [--platform PLATFORM]` 只把参数形状映射成 `/api/sellers` 的查询参数，不在本地做模糊匹配或结果过滤。纯 ASCII 数字视为 sellerId/storeId（`seller_id`），UUID 形状视为 leyanSellerId（`leyan_seller_id`），其他视为 sellerNick（`seller_nick`，此时 `--platform` 必填），`--platform` 对前两种形状作为附加约束发送。

服务端契约仍要求预置 filters/keyword 优先，但该安全语义由服务端测试保证，不能再由纯 CLI 仓库的测试代替。

跨服务追踪是独立的只读命令：`trace <SERVICE> <TRACE_ID>` 生成 `POST /api/services/{service}/_trace` 请求体 `{"trace_id","start","end"}`。related 扇出、并行查询与按时间升序合并都是服务端职责，CLI 只透传响应。

### 3.2 时间与 URL

时间输入支持带时区 ISO 8601/RFC 3339、`YYYY-MM-DD HH:mm:ss` 和 epoch seconds，统一转换为秒级时间戳。naive 时间固定按 UTC+8 解释，保证同一命令在不同机器上语义一致。

默认区间是 UTC+8 当天零点到当前时刻；要求 `start < end`，对外语义为 `[start, end)`。

`base_url` 必须是 HTTP(S) URL。CLI 保留其已有路径前缀；业务端点统一位于 `api` 下：服务列表追加 `api/services`，卖家查询追加 `api/sellers`，查询与追踪追加 `api/services`、service 与 `_search`/`_trace` URL segment，不自行实现 provider 路由。

### 3.3 HTTP、输出与错误

- 服务列表、卖家查询、搜索和追踪使用 reqwest blocking client，CLI 自身没有运行时服务和后台状态；
- 卖家参数作为 URL query 发送（`seller_id`、`leyan_seller_id`、`seller_nick` 与 `platform`），由服务端完成互斥校验；
- token 以 `Authorization: Bearer <JWT>` 发送；
- localhost、127.0.0.1、::1 自动禁用系统代理，避免企业代理拦截本机请求；
- 2xx 且响应为 JSON 时 pretty-print，否则原样输出；
- 非 2xx 输出状态码与响应体并非零退出；
- 错误与提示统一用中文输出，便于使用者直接阅读；URL、命令名、参数名、HTTP 状态码与上游返回内容保持原样；
- stdout 的 EPIPE 被忽略，使 `| head` 正常结束而不 panic。

CLI 不负责把服务端错误转换成另一套模型，因此 Python 重实现需要保持响应和错误的 HTTP 语义。

## 4. 本地配置与凭证

`config.toml` 是用户维护的配置，包含 `base_url`、可选静态 token 和 OIDC 参数。默认路径是用户主目录下的 `.config/chuanshanjia/`；Windows 优先使用非空 `USERPROFILE` 定位主目录。

`chuanshanjia config` 幂等创建默认模板，不覆盖已有文件，然后调用 `EDITOR`。编辑器命令通过 shell-words 解析，支持携带参数。

`credentials.toml` 是程序产物，与配置分开：

- 保存 access token、refresh token、过期时间和 OIDC 会话所需信息；
- Unix 下必须创建为 `0600`；
- 不保存 client_secret，不将凭证内容写入日志；
- `logout` 只需删除该文件即可清理本地会话。

服务列表、卖家查询、搜索和追踪的 token 优先级为 `--token` > 本地登录会话 > 配置文件静态 token。显式参数优先便于一次性覆盖，本地会话优先于静态配置则避免过期配置遮蔽自动续期结果。

## 5. OIDC 设计

### 5.1 信任链

信任链是 **CLI 向 IdP 获取 token，外部服务验证 token**。服务端不通过 CLI 签发 token，CLI 也不自行判断业务权限。

登录只校验本地 issuer、client、scope 和 resource 配置，然后直接访问 IdP。后端除 `/health` 外的接口（包括 RFC 9728 资源元数据）都要求 Bearer token，因此登录流程不再依赖未认证的后端元数据请求。业务 scope 仍由 IdP 按 resource 授予，而不是由客户端请求决定。

### 5.2 授权码 + PKCE

CLI 是无法保守 secret 的公开客户端，因此使用：

- `token_endpoint_auth_method = none`；
- authorization code + PKCE S256；
- 每次登录随机生成 `state` 和 code verifier，回调必须先校验 state；
- loopback callback，不开放非本机监听。

默认 redirect URI 为 `http://127.0.0.1:0/callback`，实际监听端口由系统分配并写入授权请求，避免并发登录争用固定端口。IdP 若不支持 RFC 8252 的 loopback 端口匹配，可显式配置固定的已注册 redirect URI。

本地登录会话只接受 JWT 形态 access token，与外部服务的离线 JWT 验签约定一致。

### 5.3 Scope、resource 与 audience

CLI 默认只请求 `openid offline_access`，不内置业务 scope。`offline_access` 用于取得 refresh token，存在时附加 `prompt=consent`。配置或参数可整体覆盖 scope 字符串。

`resource` 没有默认值，必须显式提供，并同时发往授权端点与 token 端点（RFC 8707）。IdP 据此决定 access token 的 audience 和实际业务 scope。

CLI 的 resource、外部服务接受的 audience 和 IdP 资源登记必须一致。验签、audience 和 scope 的最终强制执行属于服务端；其不可放宽的规则见迁移契约。

### 5.4 续期

本地会话进入 60 秒过期窗口时，搜索先使用 refresh token 静默续期：

- 成功后回写凭证文件；
- IdP 未轮换 refresh token 时保留旧值；
- 失败则提示重新登录，不静默退回一个已知过期 token；
- `expires_at` 优先由 token 响应的 `expires_in` 计算；
- 授权码兑换未返回 `expires_in` 时，退回读取 JWT `exp`，避免把短命 token 当作永久有效。

### 5.5 登出与吊销

默认登出先按 RFC 7009 调 IdP revocation endpoint 吊销 refresh token，再删除本地凭证。公开客户端不需要 client_secret。

吊销是尽力而为：IdP 不可达时给出警告仍删除本地文件，保证离线可登出；`--local` 明确表示跳过吊销。

未实现 device code flow 和系统钥匙串存储。

## 6. 测试策略

- `src/main.rs` 和 `src/oidc.rs` 内单元测试覆盖时间、URL、配置辅助逻辑、JWT 过期解析和 OIDC 细节；
- `tests/cli.rs` 用 assert_cmd 执行真实二进制，以 mockito 模拟外部 HTTP 服务与 IdP；
- 服务列表测试验证 GET 路径、Bearer token 和 JSON 输出；搜索测试验证请求 body、Bearer token 优先级、响应输出和非 2xx 错误；
- 卖家测试验证参数形状到 query 的映射（数字/UUID/昵称+platform）及缺少 `--platform` 的报错；
- 登录测试覆盖本地配置校验、不请求后端元数据、授权 URL、PKCE、state、loopback 回调、`0600` 凭证、静默续期和登出；
- 测试主目录与外部命令均使用临时目录，不读取真实用户配置；
- 不启动原 Rust backend，不请求真实 IdP 或云服务；
- CI 在 macOS、Ubuntu、Windows 运行 `fmt-check → test → lint → build`，clippy 使用 `-D warnings`；release 只构建 CLI，并从 `vX.Y.Z` tag 设置 Cargo 包版本，逐平台校验 `-V` 和 `--version` 输出 `chuanshanjia X.Y.Z` 后才上传产物。

## 7. 演进边界

- 修改 CLI 外部行为时，同步 README、设计文档和端到端测试；
- 修改外部 HTTP/OIDC 契约时，同步 `docs/backend-migration.md`，并在 Python 服务项目落实兼容测试；
- 新增 provider、服务端认证或清洗能力不属于本仓库代码变更；
- 只有在明确决定改变产品边界时，才考虑让 CLI 直连存储；当前设计保持 HTTP 解耦。
