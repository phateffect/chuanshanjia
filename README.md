# chuanshanjia

微服务的日志犹如一团乱麻。chuanshanjia 为 Agent 提供一条可组合的日志查询链路：

- **`chuanshanjia`**：命令行工具，负责解析查询条件、调用后端并输出 JSON。
- **`chuanshanjia-server`**：独立 HTTP 后端，负责 service 路由、预置查询条件和 provider 调用。

```
Agent ──chuanshanjia CLI──▶ chuanshanjia-server ──▶ Elasticsearch / 阿里云 SLS / 火山云 TLS
        ~/.config/.../          server.toml
        config.toml             oidc_issuer ──▶ 自建 OIDC (JWKS)
```

当前支持 Elasticsearch、阿里云 SLS 和火山云 TLS。

- 设计文档：[docs/design.md](docs/design.md)

## 快速开始

### 1. 构建

使用 [mise](https://mise.jdx.dev/) 安装 Rust 工具链：

```sh
mise install
mise run build
```

构建 release：

```sh
mise exec -- cargo build --release --workspace
```

产物分别是 `chuanshanjia` 和 `chuanshanjia-server`，位于 `target/debug/` 或 `target/release/`；Windows 产物带 `.exe` 后缀。下文假设它们已加入 `PATH`，也可以直接使用上述目录中的路径。

### 2. 配置并启动 Server

Server **只读取进程当前工作目录下的 `server.toml`**，不会搜索用户主目录或上级目录。从仓库根目录创建配置：

```sh
cp backend/server.example.toml server.toml
```

[完整配置示例](backend/server.example.toml)默认启用 Elasticsearch。以下是一个最小 SLS 配置，替换 endpoint、Project、Logstore 和凭证环境变量名即可：

```toml
[server]
host = "0.0.0.0"
port = 3000
oidc_issuer = "https://oidc.example.com"

[storages.sls]
provider = "aliyun-sls"
endpoint = "https://cn-hangzhou.log.aliyuncs.com"
project = "your-project"
access_key_id = { env = "SLS_ACCESS_KEY_ID" }
access_key_secret = { env = "SLS_ACCESS_KEY_SECRET" }
# security_token = { env = "SLS_SECURITY_TOKEN" }

[services.foo]
storage = "sls"
index = "your-logstore"
```

凭证也可以直接写字符串，但不建议把真实凭证提交到仓库。Server 不会自动读取 `.env`。使用环境变量时，先设置再启动：

```sh
export SLS_ACCESS_KEY_ID="your-access-key-id"
export SLS_ACCESS_KEY_SECRET="your-access-key-secret"
chuanshanjia-server
```

`server.host` 默认 `0.0.0.0`，`server.port` 默认 `3000`，`server.oidc_issuer` 必填且启动时必须可访问。仅本机访问时设置 `host = "127.0.0.1"`。搜索接口始终要求 JWT；公网部署还应使用 HTTPS，并配置防火墙和安全组。

检查服务：

```sh
curl http://127.0.0.1:3000/
# hello world
```

使用官方 Rust 和 Debian 镜像构建、运行 Server：

```sh
docker build -f deploy/Dockerfile -t chuanshanjia-server .
docker run --rm -p 3000:3000 \
  -v "$PWD/server.toml:/app/server.toml:ro" \
  chuanshanjia-server
```

### 3. 配置 CLI 并查询

CLI 使用 `~/.config/chuanshanjia/config.toml`；Windows 下是 `%USERPROFILE%\.config\chuanshanjia\config.toml`。运行以下命令可创建配置并用 `EDITOR` 打开：

```sh
chuanshanjia config
```

填写 Server 根地址和查询所需的 `token`，CLI 会自动追加 `/{service}/_search`：

```toml
base_url = "http://127.0.0.1:3000"
token = "your-jwt"
```

Server 在其他机器上时，将地址换成对应主机地址。然后查询上面配置的 `foo` service：

```sh
chuanshanjia search foo \
  -f service=order \
  -k "connection timeout" \
  --start="2026-10-14 10:01:02" \
  --end=1792994561
```

成功时输出格式化 JSON；Server 返回非 2xx 时，CLI 输出错误并以非零状态退出。

## CLI 使用

```text
chuanshanjia search <SERVICE> [-f KEY=VALUE]... [-k KEYWORDS] [--token JWT] [--start TIME] [--end TIME]
chuanshanjia login [--issuer URL] [--client-id ID] [--redirect-uri URI] [--resource URI] [--scope SCOPES] [--no-browser]
chuanshanjia logout
chuanshanjia config
```

- `SERVICE` 必填，对应 Server 中的 service 名称。
- `-f, --filter KEY=VALUE` 可重复，值按字符串处理。
- `-k, --keywords KEYWORDS` 最多出现一次；`-f` 和 `-k` 至少提供一个。
- `--token JWT` 可选，覆盖本地登录会话和配置文件中的 `token`，并以 `Authorization: Bearer <JWT>` 发送。
- Server 预置的同名 filter 和 keyword 优先，CLI 的冲突值不会覆盖预置值。
- `--start` 默认为 UTC+8 当天 00:00:00，`--end` 默认为当前时间，要求 `start < end`。

时间支持三种格式：

1. 带时区的 ISO 8601/RFC 3339，如 `2026-10-14T10:01:02+08:00`、`2026-10-14T02:01:02Z`。
2. `YYYY-MM-DD HH:mm:ss`，固定按 UTC+8 解释。
3. Epoch seconds，如 `1792994561`。

CLI 将时间统一转换为 epoch seconds，按 `[start, end)` 查询。CLI 对 `localhost`、`127.0.0.1`、`::1` 目标自动绕过系统代理。输出写 stdout，`| head` 这类提前关闭管道的情况不会 panic。

`chuanshanjia logout` 会先用 refresh token 调 IdP 的 `revocation_endpoint`（RFC 7009）真正吊销会话，再删除本地凭证；IdP 不可达时只打印警告并照常删除本地文件（离线也能登出）。`--local` 跳过吊销，只删本地凭证。

`chuanshanjia config` 子命令：配置文件不存在时写入默认模板（不会覆盖已有文件），随后用 `EDITOR`（支持命令行参数）打开。

## CLI 登录（OIDC）

CLI 直接向 OIDC 登录并拿 token，Server 只负责验，不签发。

```sh
chuanshanjia login                 # 打开浏览器完成登录
chuanshanjia logout                # 吊销会话并删除本地 token（--local 只删本地）
```

登录使用**授权码 + PKCE**、公开客户端（无 client_secret），回调走 `http://127.0.0.1:<port>/callback`。`issuer`、`client_id` 与 `resource` 可写在 CLI 配置里，也可用参数覆盖：

```toml
# ~/.config/chuanshanjia/config.toml
base_url = "http://127.0.0.1:3000"
issuer = "https://oidc.example.com"
client_id = "chuanshanjia"
resource = "https://logs.example.com"
```

```sh
chuanshanjia login --issuer https://oidc.example.com \
  --client-id chuanshanjia \
  --resource https://logs.example.com
chuanshanjia login --no-browser      # 只打印授权 URL，不自动开浏览器
```

- **需要在 IdP 上注册一个公开客户端**（token 认证方式 `none`，启用 PKCE），重定向 URI 写 `http://127.0.0.1/callback` 即可——按 RFC 8252，IdP 会忽略 loopback 回调的端口，因此 CLI 默认使用 `http://127.0.0.1:0/callback`（由系统挑空闲端口，避免并发登录抢端口）。
- 少数 IdP 仍对重定向 URI 做精确匹配；那时需在配置项 `redirect_uri` 或用 `--redirect-uri` 指定一个注册过的固定端口（如 `http://127.0.0.1:8765/callback`）。
- **scope 缺省交给 IdP**：未配置且未传 `--scope` 时，授权请求不携带 `scope` 参数，由 IdP 根据 client 和 resource 决定默认授权；需要覆盖时用配置项 `scope` 或 `--scope` 指定整串 scope。
- 显式 scope 带 `offline_access` 时会附加 `prompt=consent`；缺省时是否下发 `refresh_token` 由 IdP 的默认授权决定。无论 scope 是否显式配置，IdP 实际授予的 scope 都必须满足 Server 的 `oidc_required_scopes`，否则查询仍会返回 401。
- token 存在 `~/.config/chuanshanjia/credentials.toml`，权限 `0600`。
- **resource（RFC 8707）没有内置默认值**：必须在配置中设置 `resource` 或传入 `--resource`。登录时会把 `resource` 传给授权端点与 token 端点，IdP 据此把 access token 的 `aud` 设成该值；**必须与 `server.oidc_audiences` 和 IdP 上的资源登记完全一致**。
- **登录前检查配置**：CLI 先请求 `base_url` 下的 `/.well-known/oauth-protected-resource`，检查 `resource` 是否匹配、`issuer` 是否属于 `authorization_servers`；只有显式指定 scope 时，才检查它是否覆盖 `scopes_supported`，缺省则交给 IdP 默认授权。检查失败或 Server 元数据不可用时不会进入 OIDC 授权流程。
- **access_token 必须是 JWT**：OIDC 本地登录会话只使用 JWT 形态的 `access_token`；不透明或格式不符时报错。
- `search` 的 token 优先级：`--token` > 本地登录会话 > 配置文件的 `token`；会话过期时先用 `refresh_token` 静默续期并回写文件，续不了则提示重新 `login`。

## Server 配置

配置结构分三层：

- **storage** 定义实际日志服务的连接和凭证。当前 provider 为 `aliyun-sls`、`elasticsearch` 和 `volces-tls`。
- **service** 是搜索入口；`storage` 引用 storage 名称，`index` 是实际 ES 索引、SLS Logstore 或 TLS 日志主题 ID（Topic ID）。
- service 可配置 `filters` 和 `keyword` 作为预置查询条件。请求可以追加其他 filter，但同名字段及 keyword 始终以预置值为准，不会隐式拼接关键词。

Elasticsearch storage 示例：

```toml
[storages.es]
provider = "elasticsearch"
base_url = "http://127.0.0.1:9200"
```

火山云 TLS storage 示例（无官方 Rust SDK，请求按[火山引擎签名机制](https://www.volcengine.com/docs/6470/112168)直接调用 OpenAPI）：

```toml
[storages.tls]
provider = "volces-tls"
region = "cn-beijing"
# 可选；缺省按公网地址 https://tls-{region}.volces.com 连接。
# 内网部署可显式指定 https://tls-{region}.ivolces.com。
# endpoint = "https://tls-cn-beijing.ivolces.com"
access_key_id = { env = "TLS_ACCESS_KEY_ID" }
access_key_secret = { env = "TLS_ACCESS_KEY_SECRET" }
# security_token = { env = "TLS_SECURITY_TOKEN" }   # STS 临时凭证

[services.foo]
storage = "tls"
index = "your-topic-id"   # TLS 日志主题 ID
```

所有配置节均拒绝未知字段；`server.oidc_issuer` 不是绝对 http(s) URL、service 引用不存在的 storage、凭证缺失、`json_fields`/`ignores` 路径非法时，Server 启动失败。

### `json_fields`：JSON 字符串解析

- 默认（空列表）：将每条日志 `_source` 顶层中形如 `{...}` 或 `[...]` 的合法 JSON 字符串转换为 JSON，不递归处理嵌套字符串。
- 配置非空列表后只处理指定路径，且沿路径逐层解析，例如 `json_fields = ["attribute", "attribute.\"kv.legacy\""]`。
- 缺失节点和无效 JSON 保持原样。

### `ignores`：字段删除

在 JSON 解析**之后**对每条日志执行删除：

- 每一级先匹配完整字面键（如顶层字段名本身含点号），再尝试嵌套路径。
- 例如 `otlp.name` 既能删掉名为 `"otlp.name"` 的顶层字段，也能删掉 `otlp` 对象里的 `name`；`attribute."thread.name"` 删除嵌套字段。
- 缺失路径和数组保持原样。

### 对象路径语法

`json_fields` 和 `ignores` 使用对象路径：未加引号的段按 `.` 分隔，包含点号的字段名用双引号包裹，如 `attribute."kv.legacy"`；在 TOML 字符串中需要转义双引号。

`lookups` 仅为配置概念，尚未实现运行时行为。

## HTTP 搜索接口

`POST /{service}/_search` 使用简化的 Elasticsearch/OpenSearch 风格，不支持完整 ES DSL：

```sh
curl http://127.0.0.1:3000/foo/_search \
  -H 'Content-Type: application/json' \
  -H 'Authorization: Bearer <JWT>' \
  -d '{
    "query": {
      "filter": {
        "service": "order",
        "attempt": 2,
        "retryable": true
      },
      "keywords": "connection timeout"
    },
    "start": 1760000000,
    "end": 1760003600
  }'
```

- `query` 必填；`query.filter` 和 `query.keywords` 在 API 中均可省略，service 预置条件仍会应用。
- `filter` 表达字段等值筛选，保留 JSON 字符串、数字和布尔值类型，多个条件按 AND 组合；具体匹配行为由 provider 和索引配置决定。
- `keywords` 表达全文搜索，具体分词和匹配规则由 provider 决定。
- 顶层 `start`、`end` 是 epoch seconds，范围为 `[start, end)`。`end` 缺省为当前时间，`start` 缺省为 `end - 3600`；CLI 总是显式发送它们。
- `GET /` 健康检查不需要 token。
- `GET /.well-known/oauth-protected-resource` 按 RFC 9728 发布资源元数据（`resource`、`authorization_servers`、`scopes_supported`），也不要求 token。

### 成功响应

成功时统一返回简洁结构，不嵌套 ES 风格的 `hits` 包装：

```json
{
  "took": 5,
  "count": 1,
  "complete": true,
  "logs": [{"content": "example"}]
}
```

- `logs` 直接是日志对象数组，`json_fields` 解析和 `ignores` 删除会作用于这些对象。
- `count` 是本次实际返回条数，不是全部匹配数。
- `took` 可选，单位毫秒：SLS 为所有分页请求的总耗时，ES 为上游 `took`，TLS 为各页 `ElapsedMillisecond` 之和。
- `complete` 可选：SLS 表示所有分页均为 `Complete`，ES 表示未发生 `timed_out`，TLS 表示所有分页 `ResultStatus` 均为 `complete` 且最后一页 `ListOver` 为 `true`。

### 错误响应

错误统一为 `{"error": {"type": ..., "reason": ...}, "status": ...}`：

| 状态码 | `error.type` | 说明 |
| --- | --- | --- |
| 400 | `invalid_time_range` | `start >= end` 或时间无法换算 |
| 400 | `unsupported_sls_filter` | SLS filter 值为 null/数组/对象 |
| 400 | `unsupported_tls_filter` | TLS filter 值为 null/数组/对象 |
| 401 | `invalid_token` | JWT 缺失或无效（缺失时无响应体） |
| 404 | `service_not_found` | 未配置的 service |
| 501 | `provider_not_configured` | Server 未配置 provider |
| 502 | `es_request_failed` / `invalid_es_response` / `sls_request_failed` / `tls_request_failed` / `invalid_tls_response` | 上游网络/凭证/解码失败 |

ES 与 TLS 的服务端错误保留上游 HTTP 状态码和错误体；SLS 与 TLS 附带 request ID。请求体 JSON 非法返回 400，包含未知字段返回 422。

## JWT 认证（OIDC）

`server.oidc_issuer` 是必填配置，指向自建 OIDC 的 issuer，例如：

```toml
[server]
oidc_issuer = "https://oidc.example.com"
```

Server 启动时请求 `{issuer}/.well-known/openid-configuration` 做发现，再按 `jwks_uri` 拉取公钥，两者都只允许 `http`/`https`，且发现文档中的 `issuer` 必须与配置值一致（末尾斜杠会被忽略）。任一环节失败，Server 启动失败。

- 只接受 `alg = EdDSA` 的 JWT，公钥必须是 `kty = OKP`、`crv = Ed25519`。存在多把 key 时 token 必须提供能匹配的 `kid`，只有一把 key 时 `kid` 可省略。
- 签名、`exp` 必须有效，存在的 `nbf` 也会验证；`iss` 必须与 `server.oidc_issuer` 完全一致。
- **`aud` 必须命中 `server.oidc_audiences`**，缺失 `aud` 直接拒绝。
- **`scope` 校验**：配置 `server.oidc_required_scopes` 后，token 的 `scope` 必须包含其中每一项（支持空格分隔字符串或数组两种编码）；留空不检查。
- 遇到未知 `kid` 时自动重新拉取 JWKS 并重试一次，因此 IdP 轮换密钥无需重启；刷新有 30 秒最小间隔，限制无效 token 造成的额外请求。
- 验证结果（无论成败）在内存中缓存，30 分钟滑动过期：每次读取续期，连续 30 分钟未读取才失效。成功缓存命中时仍检查 token 自身 `exp`，过期即改判失败。
- `POST /{service}/_search` 要求 `Authorization: Bearer <JWT>`。缺少凭证或使用不支持的认证方式时返回 401（`WWW-Authenticate: Bearer realm="chuanshanjia"`，无 error code、无响应体）；token 无效或过期时返回 401 并带 `error="invalid_token"`。`GET /` 不需要 token。

CLI 侧用 `chuanshanjia login` 获取 token（见上文），也可在配置文件中写死 `token`，或在查询时传入 `--token`。

## Provider 行为

### Elasticsearch

- filter 转换为 `term` 查询，字段名原样传递，不自动添加 `.keyword`。
- keywords 转换为 `multi_match`，使用 `fields: ["*"]` 和 `fuzziness: "AUTO"`。
- 时间过滤固定使用 `@timestamp` 字段，以 `epoch_second` 格式执行 `gte`/`lt`。
- 响应中的 `logs` 取每条 hit 的 `_source`（缺失时用 hit 本身）。

### 阿里云 SLS

使用[阿里云官方 Rust SDK](https://github.com/aliyun/aliyun-log-rust-sdk)。

- keywords 和 filter 都转换为 SLS 原生查询语法，空查询使用 `*`，并追加 `| with_pack_meta` 保留原始元数据。多个条件按 AND 组合，例如：

  ```text
  "connection timeout" AND "attempt": 2 AND "retryable": true AND "service": "order" | with_pack_meta
  ```

- 字符串（含字段名）使用双引号并转义 `\` 和 `"`；数字和布尔值保留原生标量形式。
- 字段需要配置 [SLS 索引](https://help.aliyun.com/zh/sls/data-types)；分词和匹配行为由索引配置决定。
- 自动翻页：每页 100 条（`line`/`offset`），最多 10 页、单次最多返回 1000 条。
- `_source` 保留 SDK 返回的原始字段名和大小写，不补空列；`attribute`、`resource` 等字段再按 `json_fields` 规则尝试 JSON 转换。

更多语法参考：[SLS 查询语法](https://help.aliyun.com/zh/sls/query-syntax/)、[GetLogs 分页方式](https://help.aliyun.com/zh/sls/paged-query)。

### 火山云 TLS

TLS 没有官方 Rust SDK，Server 用 reqwest 直接调用 `POST /SearchLogs`（`x-tls-apiversion: 0.3.0`），并按[火山引擎签名机制](https://www.volcengine.com/docs/6470/112168)（HMAC-SHA256，服务名 `TLS`）对请求签名；`service.index` 填日志主题 ID（Topic ID）。

- keywords 和 filter 都转换为 [TLS 检索语法](https://www.volcengine.com/docs/6470/1206705)，空查询使用 `*`。多个条件按 AND 组合，例如：

  ```text
  "connection timeout" AND attempt:2 AND retryable:true AND service:"order"
  ```

- 字符串值（含 keywords）使用双引号并转义 `\` 和 `"`；纯字母数字下划线的字段名原样传递，含其他字符的字段名按官方文档用双引号包裹；数字和布尔值保留原生标量形式。
- 字段需要配置键值索引；分词和匹配行为由索引配置决定。
- 时间范围为 `[start, end)`，请求时转换为毫秒时间戳；`Sort` 固定为 `desc`（时间逆序，新日志在前）。
- 自动翻页：每页 `Limit=100`，未返回全部结果（`ListOver=false`）且页满时携带上一页 `Context` 续查，最多 10 页、单次最多返回 1000 条。
- `logs` 保留返回的原始字段名和取值；`__path__`、`__source__` 等元数据字段可用 `ignores` 清理。

## 开发与验证

```sh
mise run fmt-check
mise run test
mise run lint
mise run build
```

测试覆盖 CLI 参数、Bearer token 传递、OIDC 发现与 JWKS 处理（含 `iss` 校验、未知 kid 刷新、刷限频）、Ed25519 JWT 验证、验证结果缓存、RFC 6750 401 响应、时间解析、provider 查询转换（含 TLS 签名参考向量、分页与错误映射），以及 CLI → Server → 模拟 ES 的端到端链路；不读取真实用户配置，也不请求真实云服务。GitHub Actions 检查配置见 [.github/workflows/ci.yml](.github/workflows/ci.yml)。

同一 GitHub Actions workflow 会并行构建 Windows/Linux/macOS CLI 与 Linux Server 的 release binary，并上传为 workflow artifacts。Windows/Linux 产物为 x86_64；macOS 产物为 ARM64（Apple Silicon）。

## 当前限制

- `lookups` 仅为配置概念，尚未实现；`ignores` 不支持删除数组元素。
- API 未提供分页、排序和聚合参数，也不支持 `bool`、`must` 等完整 ES 查询语法；SLS 与 TLS 每次最多返回 1000 条，TLS 固定按时间逆序返回。
