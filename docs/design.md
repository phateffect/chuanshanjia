# chuanshanjia 设计文档

本文描述 chuanshanjia 的架构、关键设计决策及其动机。使用方法与配置参考见 [README](../README.md)。

## 1. 背景与目标

微服务环境下的日志分散在多个 Elasticsearch、阿里云 SLS 等系统中，检索语法、认证方式和字段结构各不相同。目标是为 Agent（以及人）提供一条**可组合、稳定、少惊扰**的日志查询链路：

- Agent 只需面对一个极简 CLI（`chuanshanjia search <service> -f k=v -k kw`）和一个统一 JSON 响应。
- 运维侧通过 Server 配置固化 service 路由、预置过滤条件和字段清洗规则，Agent 无需了解底层是 ES 还是 SLS。
- 输出适合直接被 Agent 消费：结构统一、噪音字段可删、内嵌 JSON 字符串可展开。

### 非目标

- 不做完整 ES DSL 或 SLS 分析语法的透传；只支持 filter + keywords + 时间范围这一最小查询模型。
- 不做日志采集、存储、告警。
- 不做多租户管理界面；认证只做 JWT 验证，不做签发。

## 2. 总体架构

```
┌──────────────┐   POST /{service}/_search    ┌──────────────────────────────────────┐
│ chuanshanjia │ ───────────────────────────▶ │ chuanshanjia-server (axum)            │
│ (CLI)        │   Authorization: Bearer JWT  │                                      │
└──────────────┘                              │  require_jwt ─▶ search handler       │
      ▲                                       │       │                              │
 ~/.config/chuanshanjia/config.toml            │  ServiceProvider                   │
                                              │   ├─ 预置 filters/keyword 合并       │
                                              │   ├─ json_fields 解析              │
                                              │   ├─ ignores 删除                  │
                                              │   └─ SearchProvider trait           │
                                              │       ├─ EsProvider ─▶ Elasticsearch
                                              │       └─ SlsProvider ─▶ 阿里云 SLS
                                              └───────────┬──────────────────────────┘
                                                          │ 启动发现 + 未知 kid 刷新
                                                          ▼
                                                 自建 OIDC (JWKS)
```

两个 crate 组成 workspace：

| 产物 | crate | 职责 |
| --- | --- | --- |
| `chuanshanjia` | 根 crate（`src/main.rs`） | CLI：参数解析（usage-rs）、时间归一化、调用 Server、打印 JSON |
| `chuanshanjia-server` | `backend`（`src/main.rs` + lib） | HTTP 后端：配置加载、JWT 认证、service 路由、查询转换与清洗 |

backend 库内部模块：

| 模块 | 职责 |
| --- | --- |
| `config.rs` | `AppConfig`（server/storages/services）的反序列化与校验；凭证的字符串/环境变量双形态 |
| `auth.rs` | OIDC 发现与 JWKS 拉取、Ed25519 JWT 验证、验证结果缓存、RFC 6750 401 响应 |
| `search.rs` | `POST /{service}/_search` handler；`SearchRequest`/`SearchQuery` 数据模型与时间范围校验 |
| `provider/mod.rs` | `SearchProvider` trait、`ProviderError`（统一 JSON 错误）、`UnconfiguredProvider` |
| `provider/service.rs` | `ServiceProvider`：service → storage 路由、预置条件合并、`json_fields`/`ignores` 清洗管线、对象路径解析 |
| `provider/es.rs` | ES 查询翻译与响应归一化 |
| `provider/sls.rs` | SLS 原生查询构造、分页拉取与响应归一化 |

## 3. 关键设计决策

### 3.1 查询模型：filter + keywords + 时间范围

请求体刻意保持最小：

```json
{"query": {"filter": {"k": "v"}, "keywords": "text"}, "start": 0, "end": 0}
```

- **filter** 是字段等值筛选，值保留 JSON 标量类型（字符串/数字/布尔）；匹配语义交给底层 provider 与索引。
- **keywords** 是全文搜索，一个字符串，不做词条拆分。
- 时间统一为 epoch seconds，区间 `[start, end)`；`end` 缺省当前时间、`start` 缺省 `end - 3600`。CLI 恒显式传时间。
- 未知字段一律拒绝（`deny_unknown_fields`），避免拼错字段名被静默忽略。

### 3.2 预置条件：Server 是查询语义的权威

service 可配置 `filters` 和 `keyword`。合并规则：

- 请求 filter 与预置 filters 合并，**同名字段以预置值为准**（`filter.extend(service.filters)`）；
- 预置 `keyword` 存在时**整体替换**请求 keywords，不做拼接——拼接两段自由文本几乎必然产生病态查询。

动机：预置条件常用于环境隔离（如 `environment = "pre"`）或数据源裁剪（按 `service` 字段过滤），必须不可被请求方绕过，否则 Agent 可能读到其他环境的数据。

### 3.3 响应归一化

所有 provider 统一输出：

```json
{"took": 5, "count": 1, "complete": true, "logs": [...]}
```

- `logs` 直接是日志对象数组，去掉 ES `hits.hits._source` 包装（无 `_source` 时退用 hit 本身）。
- `count` 是本次返回条数，**不是**总匹配数（不做额外 count 查询）。
- `took`/`complete` 为可选：SLS 的 `took` 是本地计时的所有分页总耗时，ES 直接取上游 `took`；`complete` 在 SLS 表示所有分页 `Complete`，在 ES 表示未 `timed_out`。字段缺失即信息缺失，不造默认值。

### 3.4 输出清洗管线：parse → ignore

日志源（尤其 OTel 采集链路）常见两类噪音：整段 JSON 被压成字符串、大量 SDK 元数据字段。清洗在 `ServiceProvider` 层对每条 log 顺序执行：

1. **`json_fields` JSON 解析**
   - 空列表（默认）：只扫描 `_source` 顶层值，形如 `{...}` / `[...]` 的字符串尝试解析；**不递归**展开嵌套字符串。
   - 非空列表：只处理配置的路径；沿路径逐层解析（父层解析成功后继续深入），因此 `attribute."kv.legacy"` 会先解析 `attribute` 再解析其中的 `kv.legacy`。
   - 解析失败的字符串保持原样，不报错。
2. **`ignores` 字段删除**（在解析之后执行，因此能删除解析产物里的嵌套字段）
   - 每一级先按完整字面键匹配（兼容字段名本身含点号的情况），再按嵌套路径逐级下钻。
   - 缺失路径静默跳过；数组不支持删除元素。

对象路径语法：段以 `.` 分隔，含点号的字段名用双引号包裹并支持 `\"` 转义。路径在**启动时**解析并校验，非法路径直接启动失败（fail-fast，见 3.6）。

### 3.5 认证：自建 OIDC + Ed25519-only JWT

信任链完全来自 OIDC issuer，没有本地密钥文件模式：

- **发现与拉取**：启动时请求 `{issuer}/.well-known/openid-configuration`，校验文档 `issuer` 与配置一致（忽略末尾 `/`），再按 `jwks_uri` 拉取 JWKS；两者都只允许 `http`/`https`。任何一步失败（不可达、格式错误、不一致、无可用 Ed25519 公钥）即启动失败。动机：issuer 就是信任根的声明，配置里再放一份本地公钥文件只会引入“两份真相”和额外的分发步骤。
- **只支持 EdDSA/Ed25519**。单一算法排除 `alg` 混淆攻击与 RSA/HMAC 密钥误用这一整类问题，实现也最简单；自建 IdP 只签 `EdDSA`，不牺牲兼容性。
- **claims**：`exp` 必填，`nbf` 存在则校验，leeway 为 0；**`iss` 必须等于配置的 issuer**（静默接受缺失 `iss` 会让其他 issuer 的 token 也能通过）；`aud` 必须命中 `server.oidc_audiences`（见 5.2）。
- **kid 规则**：多 key 时 token 必须携带能匹配的 `kid`；单 key 时可省略。
- **密钥轮换**：未知 `kid` 会触发一次 JWKS 重新拉取并重试验签，因此 IdP 轮换后**无需重启**。刷新受 30 秒最小间隔限制，避免无效 token 引发对 IdP 的请求放大；刷新失败只记日志、保留旧公钥集。
- **验证结果缓存**（moka，30 分钟滑动过期）：
  - 验签是 CPU 敏感操作，缓存把重复 token 的成本降为一次哈希查找；
  - 失败结果同样缓存，避免被无效 token 打爆；
  - 成功缓存命中时仍检查 token 自身 `exp`，过期即改判失败——否则缓存会把短命 token 变成 30 分钟长效凭证；
  - 滑动过期而非固定 TTL：活跃 token 不必反复验签，闲置条目自动回收。
- JWKS 刷新用可注入的 `JsonFetcher` 抽象，测试里替换为路由队列 mock，无需网络即可覆盖发现失败、issuer 不匹配、未知 kid 刷新、刷新限频等分支。

### 3.6 配置：强校验 + fail-fast

所有配置结构 `deny_unknown_fields`，且一切可在启动时确定的错误都在启动时失败：

- 未知字段、缺失 `server.oidc_issuer` 或它不是绝对 http(s) URL、未知 provider 变体；
- service 引用不存在的 storage、service 名含 `/` 或为空、`index` 为空；
- `json_fields`/`ignores` 路径非法；
- 凭证（含环境变量形态）缺失或为空。

运行期不再有“配置错误”这类错误类别；配置错误绝不让进程带着半套配置上线。

凭证双形态：`access_key_id = "literal"` 或 `access_key_id = { env = "NAME" }`，后者在启动时解析，缺失即启动失败。OIDC 公钥不走配置，而是启动时从 issuer 发现，因此不存在本地密钥文件需要分发或轮换。

### 3.7 错误模型

`ProviderError` 统一输出 `{"error": {"type": ..., "reason": ...}, "status": ...}`：

- **业务错误**（4xx）：`service_not_found`、`invalid_time_range`、`unsupported_sls_filter`，由本层生成。
- **上游错误**：ES 非 2xx 响应原样保留状态码和 JSON 错误体；SLS `Server` 错误保留 HTTP 状态与 `error_code`，`request_id` 拼进 reason 便于找阿里云排查。
- **基础设施错误**：网络失败、凭证无效、响应解码失败统一 502（`es_request_failed`/`invalid_es_response`/`sls_request_failed`），reason 去掉 URL（含凭证信息）。

401 响应遵循 RFC 6750：缺凭证时无响应体，仅 `WWW-Authenticate: Bearer realm="chuanshanjia"`；token 无效时附 `error="invalid_token"` 与 JSON 错误体。

## 4. Provider 实现

`SearchProvider` 是唯一抽象：

```rust
#[async_trait]
pub trait SearchProvider: Send + Sync {
    async fn search(&self, index: &str, request: &SearchRequest) -> Result<Value, ProviderError>;
}
```

`ServiceProvider` 持有 storage 名 → provider 实例的映射；service 引用 storage，多个 service 可复用同一 storage。测试用 `MockProvider`/`MockSlsClient` 替换真实实现。

### 4.1 Elasticsearch

- filter → `term` 查询（字段名原样传递，不加 `.keyword`，由索引 mapping 决定匹配行为）。
- keywords → `multi_match`，`fields: ["*"]`、`fuzziness: "AUTO"`——不知道字段清单时的保守全文搜索。
- 时间 → 固定 `@timestamp` 字段的 `range`，`gte`/`lt` + `format: "epoch_second"`。
- 请求无分页参数；上游返回多少转多少。

### 4.2 阿里云 SLS

- 使用[官方 Rust SDK](https://github.com/aliyun/aliyun-log-rust-sdk) 的 `GetLogs` 接口，`from`/`to` 对应 `[start, end)`。
- 查询构造为原生查询语法：`"keywords" AND "field": value | with_pack_meta`。空查询用 `*`；空字符串 keywords 视为未提供；字符串统一双引号并转义 `\` 与 `"`；数字/布尔保留标量形式；null/数组/对象值直接 400 拒绝（SLS 无对应语义）。字段匹配依赖 SLS 索引配置，分词行为不在本层控制。
- **分页策略**：每页 `line=100`，`offset += 100` 翻页；页面不满 100 条或累计达到 1000 条（10 页）即停。`complete` 汇总所有页的 `Complete` 状态，向调用方诚实暴露“可能没拉全”。
- `__pack_meta__` 等元数据随 `with_pack_meta` 返回，再由 `ignores` 清理。

## 5. CLI 设计

- 依赖面最小：usage-rs 定义参数，reqwest blocking 发请求；无运行时服务、无后台状态。
- 时间输入三形态（带时区 ISO 8601、`YYYY-MM-DD HH:mm:ss` 固定 UTC+8、epoch seconds），统一转 epoch seconds。naive 时间按 UTC+8 解释而不取系统时区，保证跨机器行为一致。
- `--start` 默认 UTC+8 当天零点、`--end` 默认当前时刻，覆盖“查今天的日志”这一最高频场景。
- 对 localhost/127.0.0.1/::1 自动禁用系统代理，避免企业代理拦截本机流量。
- 输出：响应是 JSON 则 pretty-print，否则原样打印；非 2xx 打印状态码与响应体并以非零码退出。stdout 写入忽略 EPIPE，`| head` 不再触发 panic。
- `chuanshanjia config`：幂等创建默认配置后调用 `EDITOR`（经 shell-words 解析，支持带参数的编辑器命令）。

### 5.1 登录：CLI 直接和 IdP 对话

信任链只有一条：**CLI 向 IdP 登录拿 token，Server 拿 IdP 的公钥验 token**。Server 不签发 token——否则它就要自己管签名密钥、轮换、续期、吊销和用户映射，等于重造一个 IdP，还多一跳信任跳跃。

- **登录前配置检查**：`resource` 没有内置默认值，必须由配置或 `--resource` 显式提供。CLI 先读取 `base_url` 下的 RFC 9728 资源元数据，核对 `resource`、`authorization_servers` 中的 issuer，以及申请的 scope 是否覆盖 `scopes_supported`；任一检查失败或元数据不可用都不进入 OIDC 授权流程。这样本地配置错误会在打开浏览器前暴露，但不能替代 IdP 对 resource 的登记检查。
- **授权码 + PKCE（S256）+ 公开客户端**：CLI 无法保守 secret（二进制里必然泄漏），所以用 `token_endpoint_auth_method = none` + PKCE；IdP 侧注册公开客户端，重定向 URI 使用 loopback callback。`state` 与 PKCE `code_verifier` 均每次随机生成，回调里先校 `state` 再换 token。
- **随机 loopback 端口**：按 RFC 8252，IdP 匹配 loopback `redirect_uri` 时忽略端口，因此默认 `http://127.0.0.1:0/callback`——`:0` 让系统挑空闲端口，实际端口再写进授权请求。这样并发登录不会抢端口，也不需要预注册固定端口。监听端口始终从 `redirect_uri` 推导；需要固定端口时用 `--redirect-uri http://127.0.0.1:8765/callback`。
- **JWT access token**：OIDC 本地登录会话只使用 JWT 形态的 `access_token`；不透明或格式不符时报错，因为 Server 只支持离线验签。
- **scope 与 consent**：默认 `openid profile email offline_access logs:read`；带 `offline_access` 时附加 `prompt=consent`，否则不少 IdP 不会返回 `refresh_token`。
- **业务 scope 显式申请并获得授权**：实际签发的 scope 取决于请求和 IdP 配置。漏申请或 client 未获授权时，token 可能通过验签但没有业务权限。因此 CLI 默认包含 `logs:read`，Server 也可用 `oidc_required_scopes` 做纵深防御。
- **token 存储**：`~/.config/chuanshanjia/credentials.toml`，Unix 下 `0600`；不放进 `config.toml`，因为后者是用户手改的配置，凭证应是程序维护的产物（也便于 `logout` 只删一个文件）。
- **续期**：默认 scope 含 `offline_access` 以获取 `refresh_token`；`search` 发现 `expires_at` 进入 60 秒窗口就先用 `refresh_token` 静默续期并回写文件，失败则提示重新 `login`。若 IdP 不轮换 refresh token，则保留旧值。
- **token 选择顺序**：`--token` > 本地会话 > 配置文件 `token`。保留后两者是为了兼容尚未接入 OIDC、手工拿 token 的场景。
- **`--no-browser`**：只打印授权 URL，供 SSH/无桌面环境与自动化测试使用。

未实现：device code flow、token 缓存于系统钥匙串。

### 5.2 `aud` 校验

Server 用 `server.oidc_audiences` 打开 `Validation::set_audience`，并把 `aud` 列为必需 claim：缺失或不在列表内一律 401。多受众 token 按 RFC 7519 处理——只要其中一项命中即接受。

IdP 按 RFC 9068 签发 JWT access token 时（`typ: at+jwt`），`aud` 取自 RFC 8707 的 `resource`，因此配置值使用资源标识，例如 `https://logs.example.com`：验的正是「这张 token 是签给日志服务的」。

### 5.3 `scope` 校验与资源元数据

- **`server.oidc_required_scopes`**：`aud` 只回答「这张 token 是不是给我」，回答不了「允许做什么」。配置后要求 token 的 `scope` 覆盖全部所需项（兼容 RFC 9068 的空格分隔字符串与部分 IdP 的数组编码）。留空则跳过。业务 scope 的可用值和授予规则由实际 IdP 配置决定，discovery 的 `scopes_supported` 不一定包含它们，因此应以实际签发的 token 或 IdP 配置为准。
- **RFC 9728 元数据**：后端发布 `/.well-known/oauth-protected-resource`，声明 `resource`、`authorization_servers` 与 `scopes_supported`，作为 CLI 登录前校验本地配置的依据，而不是由 CLI 内置一个可能不适用的默认资源标识。这里复用 `oidc_audiences[0]` 作为资源标识，避免 Server 再引入一个必须保持同步的配置项。

### 5.4 登出即吊销

`chuanshanjia logout` 先按 RFC 7009 调 IdP 的 `revocation_endpoint` 吊销 refresh token，再删本地凭证。撤销是**尽力而为**：IdP 不可达时给警告并照常删除本地文件，否则离线环境无法登出；`--local` 用于明确表示「只清理本地」。

公开客户端（`token_endpoint_auth_method=none`）可以直接调撤销端点，不需要 client secret。

## 6. 测试策略

- **单元测试在各模块内**（`#[cfg(test)]`）：时间解析与默认值、配置校验（含 `server.example.toml` 必须可解析的回归测试）、OIDC 发现（issuer 不一致、非 http URL）与 JWT 全部分支（算法、kid、exp/nbf/iss、未知 kid 刷新、刷新限频、缓存滑动过期与命中后过期改判）、对象路径解析、JSON 清洗管线、预置条件合并、ES 查询翻译、SLS 查询构造/转义/分页/错误映射。
- **HTTP 层测试**用 axum `oneshot`：路由、认证中间件、错误响应形状。
- **端到端测试** `tests/cli.rs`：assert_cmd 驱动真实 CLI 二进制，mockito 模拟 ES、Server 与 IdP，覆盖 CLI → Server → ES 全链路（含预置条件不可被 CLI 覆盖），以及登录全流程（从 CLI stdout 取授权 URL、取出 `state` 后向 loopback 回调、校 PKCE 参数、换 token、写 `0600` 凭证文件）、伪造 `state` 被拒、过期会话静默续期。
- 原则：不读真实用户配置、不请求真实云服务、无 sleep 依赖竞态（缓存测试用注入的超时与时间）。
- CI（`.github/workflows/ci.yml`）在 macOS/Ubuntu/Windows 三平台跑 `fmt-check → test → lint → build`，clippy 以 `-D warnings` 要求零告警。

## 7. 已知限制与演进方向

| 项 | 现状 | 方向 |
| --- | --- | --- |
| 火山云 TLS | 未实现，`provider = "volces-tls"` 会被拒绝 | 按现有 provider 模式新增 `provider/tls.rs` |
| `lookups` | 仅配置概念，无运行时行为 | 用于声明 trace 关联查询要访问的其他 service |
| API 分页/排序/聚合 | 未提供；SLS 单次最多 1000 条 | 视 Agent 实际使用痛点再决定是否引入 |
| 完整 ES DSL | 不支持 `bool`/`must` 等 | 维持最小查询模型，避免语义泄漏 |
| CLI 登录 | 授权码 + PKCE（loopback 回调） | 若 IdP 支持 device code 可补；token 可选存入系统钥匙串 |
| `ignores` | 不支持删除数组元素 | 如有需求按 JSON Path 下标扩展 |
