# Backend 行为契约与 Python 迁移说明

本文是 `chuanshanjia-server` 的行为规格，供在其他项目中用 Python 重新实现。重点是对外契约、处理顺序和容易遗漏的边界条件，而不是逐行翻译 Rust。

参考实现最后保留在 Git 提交 `e8a549e`。本文中的 `backend/src/...` 路径均指该提交；当前仓库已经移除服务端，只维护 CLI。

## 1. 系统边界

服务端负责：

- 从本地配置加载 storage、service、预置查询条件和清洗规则；
- 发布 OIDC 资源元数据，验证 CLI 发来的 JWT access token；
- 把统一的 `filter + keywords + 时间范围` 查询翻译成 Elasticsearch、阿里云 SLS 或火山云 TLS 请求；
- 把不同上游的响应归一化为统一 JSON，再执行日志清洗。

服务端不负责：

- 签发 token；CLI 直接向 IdP 登录，服务端只验证；
- 完整透传 Elasticsearch DSL 或各日志系统的分析语法；
- 日志采集、存储、告警、多租户管理界面；
- `lookups` 关联查询。该字段只被配置解析，没有运行时行为。

CLI 仍是独立进程，通过 `base_url` 调用外部 HTTP 服务。Python 重实现不需要、也不应把 provider 逻辑移入 CLI。

### 1.1 请求主链路

```text
CLI
  │ POST /{service}/_search
  │ Authorization: Bearer <JWT>
  ▼
Bearer 提取与 JWT 验证
  ▼
SearchRequest JSON 解析与时间范围归一化
  ▼
按 service 名称找到 ConfiguredService
  ▼
合并请求与预置 filters / keyword（预置值优先）
  ▼
调用该 service 引用的 storage provider
  ▼
上游响应归一化为 took / count / complete / logs
  ▼
json_fields 解析 → ignores 删除
  ▼
统一 JSON 响应或 ProviderError
```

时间范围实际在各 provider 构造查询时归一化，但语义相同。未知 service 在调用任何上游之前返回 404；清洗只处理成功的 provider 响应。

### 1.2 原 Rust 模块职责

| 模块 | 职责 |
| --- | --- |
| `backend/src/main.rs` | 启动顺序、配置加载、OIDC 初始化、监听地址与 Router 组装 |
| `backend/src/config.rs` | 配置数据模型、未知字段拒绝、基础校验、凭证解析 |
| `backend/src/auth.rs` | OIDC discovery、JWKS、Ed25519 验签、scope/audience 校验、缓存与 401 |
| `backend/src/search.rs` | 搜索请求模型、默认时间范围、搜索 handler |
| `backend/src/lib.rs` | 路由、健康检查、RFC 9728 资源元数据、认证中间件挂载 |
| `backend/src/provider/service.rs` | storage/service 构造、预置条件合并、对象路径与日志清洗 |
| `backend/src/provider/es.rs` | Elasticsearch 查询和响应转换 |
| `backend/src/provider/sls.rs` | SLS 查询、分页和错误转换 |
| `backend/src/provider/tls.rs` | TLS 查询、请求签名、Context 分页和错误转换 |
| `backend/src/provider/mod.rs` | provider 抽象与统一错误响应 |

## 2. 启动与配置

### 2.1 配置文件位置与启动顺序

服务端只读取**进程当前工作目录**下的 `./server.toml`，不搜索用户主目录或上级目录，也不自动读取 `.env`。

原实现的启动顺序：

1. 读取并反序列化 `server.toml`；
2. 校验 server 基础配置；
3. 根据 `oidc_issuer` 做 OIDC discovery 并拉取 JWKS；
4. 用第一个 audience 构造资源元数据；
5. 构造 storage providers，解析凭证，校验 service 引用和对象路径；
6. 绑定监听地址并开始服务。

上述问题都应在启动期报错，不能带着部分无效配置运行。host 默认 `0.0.0.0`，port 默认 `3000`。

### 2.2 配置结构

```toml
[server]
host = "0.0.0.0"                         # 可选
port = 3000                              # 可选
oidc_issuer = "https://oidc.example.com"
oidc_audiences = ["https://logs.example.com"]
oidc_required_scopes = ["logs:read"]     # 可选，默认空列表

[storages.es]
provider = "elasticsearch"
base_url = "http://127.0.0.1:9200"

# [storages.sls]
# provider = "aliyun-sls"
# endpoint = "https://cn-hangzhou.log.aliyuncs.com"
# project = "your-project"
# access_key_id = { env = "SLS_ACCESS_KEY_ID" }
# access_key_secret = { env = "SLS_ACCESS_KEY_SECRET" }
# security_token = { env = "SLS_SECURITY_TOKEN" }  # 可选 STS

# [storages.tls]
# provider = "volces-tls"
# region = "cn-beijing"
# endpoint = "https://tls-cn-beijing.ivolces.com" # 可选
# access_key_id = { env = "TLS_ACCESS_KEY_ID" }
# access_key_secret = { env = "TLS_ACCESS_KEY_SECRET" }
# security_token = { env = "TLS_SECURITY_TOKEN" } # 可选 STS

[services.foo]
storage = "es"
index = "app-logs"
keyword = "preset keyword"               # 可选
json_fields = ["attribute", "attribute.\"kv.legacy\""]
ignores = ["__pack_meta__", "attribute.\"thread.name\""]
lookups = []                              # 仅解析，不执行

[services.foo.filters]
service = "order"
environment = "pre"
attempt = 2
retryable = true
```

`storages` 和 `services` 缺省为空表。多个 service 可以复用一个 storage。`index` 的实际含义由 provider 决定：ES 索引名、SLS Logstore 或 TLS Topic ID。

### 2.3 严格校验

所有配置结构都拒绝未知字段，包括各 storage 变体和 `{ env = "..." }` 凭证对象。启动时至少校验：

- `server` 和 `oidc_issuer` 必填；issuer 必须是绝对的 HTTP(S) URL；
- `oidc_audiences` 至少一项，所有 audience 去除首尾空白后不能为空；
- `oidc_required_scopes` 默认空，配置项不能是空白字符串；空列表表示不检查业务 scope；
- storage 名不能为空，provider 只接受 `elasticsearch`、`aliyun-sls`、`volces-tls`；
- ES `base_url` 必须是 HTTP(S) URL；
- SLS `endpoint`、`project` 不能为空，SDK 配置和客户端必须能成功构造；
- TLS `region` 不能为空；未配置 endpoint 时使用 `https://tls-{region}.volces.com`，显式 endpoint 必须是 HTTP(S) URL；
- service 名不能为空且不能含 `/`，`index` 不能为空，`storage` 必须引用已定义 storage；
- `json_fields` 和 `ignores` 必须是合法对象路径；
- 所有必填凭证必须能解析且值非空。

凭证有两种形式：

```toml
access_key_id = "literal-value"
access_key_secret = { env = "ACCESS_KEY_SECRET" }
```

环境变量形式在启动时读取；变量名为空、变量不存在或值为空均启动失败。Python 实现不得把凭证值写入日志或错误信息。

## 3. HTTP 契约

### 3.1 公开端点

`GET /`

- 不需要认证；
- 返回 `200`，正文为 `hello world`。

`GET /.well-known/oauth-protected-resource`

- 不需要认证；
- 按 RFC 9728 返回：

```json
{
  "resource": "https://logs.example.com",
  "authorization_servers": ["https://oidc.example.com"],
  "scopes_supported": ["logs:read"],
  "bearer_methods_supported": ["header"]
}
```

其中 `resource = oidc_audiences[0]`，`authorization_servers` 只含配置的 issuer，`scopes_supported = oidc_required_scopes`。CLI 登录前依赖此端点核对 `resource` 和 issuer，但不会用 `scopes_supported` 限制客户端请求的 scope。

### 3.2 搜索请求

`POST /{service}/_search` 需要 Bearer JWT，请求体：

```json
{
  "filter": {
    "service": "order",
    "attempt": 2,
    "retryable": true
  },
  "keywords": "connection timeout",
  "start": 1760000000,
  "end": 1760003600
}
```

规则：

- 请求必须使用 JSON Content-Type（CLI 发送 `application/json`）；不支持的 Content-Type 由 HTTP 框架在 handler 前拒绝。
- 顶层只允许 `filter`、`keywords`、`start`、`end`。
- `filter` 缺省为空对象，`keywords` 可省略。
- `filter` 是字段等值条件，多个条件按 AND 组合。字符串、数字和布尔值保留 JSON 类型；SLS/TLS 额外拒绝 null、数组和对象。
- `keywords` 是一个完整字符串，不在统一层拆词；分词和匹配语义由上游及索引配置决定。
- `start`、`end` 是必传的有符号 epoch seconds，有效区间是 `[start, end)`，允许负时间戳。
- `start >= end` 属于请求结构校验失败，返回 422；CLI 也会在发送前拒绝该区间。
- CLI 总是显式发送 `start` 和 `end`，并负责生成默认时间；Python 服务端不补时间默认值。
- 请求 JSON 语法错误返回 400；结构反序列化失败（包括未知字段）返回 422。这两类错误发生在搜索 handler 之前，原实现不保证使用统一错误 JSON 外形。

### 3.3 成功响应

```json
{
  "took": 5,
  "count": 1,
  "complete": true,
  "logs": [{"content": "example"}]
}
```

- `logs` 是本次实际日志对象数组，不包含 ES `hits` 包装。
- `count` 始终等于本次 `logs` 长度，不是全部匹配数。
- `took` 是可选毫秒耗时，各 provider 的来源不同。
- `complete` 是可选完整性标记，各 provider 的来源不同；不要虚构上游没有提供的信息。
- 成功响应随后经过 service 的 `json_fields` 和 `ignores` 清洗。

### 3.4 错误响应

服务端主动生成的 provider/auth 错误使用：

```json
{
  "error": {"type": "invalid_time_range", "reason": "start must be earlier than end"},
  "status": 400
}
```

HTTP 状态必须与 body 中的 `status` 一致。主要错误：

| HTTP | `error.type` | 条件 |
| --- | --- | --- |
| 400 | `invalid_time_range` | 时间为空区间、反向区间或默认回推下溢 |
| 400 | `unsupported_sls_filter` | SLS filter 为 null、数组或对象 |
| 400 | `unsupported_tls_filter` | TLS filter 为 null、数组或对象 |
| 401 | `invalid_token` | token 无效、过期、issuer/audience/scope 不符 |
| 404 | `service_not_found` | service 未配置 |
| 500 | `invalid_es_base_url` | ES base URL 不能作为分段拼接基准 |
| 500 | `invalid_tls_endpoint` | TLS endpoint 不能作为分段拼接基准 |
| 501 | `provider_not_configured` | Router 使用未配置 provider；正常完整启动不进入此状态 |
| 502 | `es_request_failed` | ES 网络或客户端错误 |
| 502 | `invalid_es_response` | ES 响应不是可解析 JSON |
| 502 | `sls_request_failed` | SLS 非服务端响应类错误 |
| 502 | `tls_request_failed` | TLS 请求序列化、网络或客户端错误 |
| 502 | `invalid_tls_response` | TLS 响应不是合法 JSON 或成功响应结构无法解码 |

上游错误有特殊透传规则，见各 provider 章节。缺少 Bearer 凭证时的 401 没有响应体，详见认证章节。

## 4. OIDC 与 JWT 认证

### 4.1 Discovery 与 JWKS

启动时：

1. 将 issuer 末尾所有 `/` 去掉，要求 HTTP(S) URL；
2. 请求 `{issuer}/.well-known/openid-configuration`；
3. 要求 discovery 文档的 `issuer` 去掉末尾 `/` 后与配置一致；
4. 校验 `jwks_uri` 是 HTTP(S) URL并拉取 JWKS；
5. 从 JWKS 选择 Ed25519 验签 key，至少一把，否则启动失败。

discovery/JWKS HTTP 请求超时为 10 秒。只选择满足以下条件的 JWK：

- `kty = OKP`、`crv = Ed25519`；
- `alg` 缺省或为 `EdDSA`；
- `use` 缺省或为 `sig`；
- `key_ops` 缺省或包含 `verify`；
- 公钥解码后必须为 32 字节。

候选 key 的 `kid` 若存在则不能为空且不能重复。其他算法 key 可忽略。

### 4.2 Token 验证

只接受 JWT access token，规则如下：

- header `alg` 必须是 `EdDSA`，不接受 RSA、HMAC 或其他算法；
- token 有 `kid` 时必须匹配同 kid 的 key；无 `kid` 时仅当 JWKS 恰好一把候选 key 才使用该 key；
- 必须验签成功，`exp` 必填且在有效期内，`nbf` 若存在必须有效，时钟 leeway 为 0；
- `iss` 必须存在且与规范化后的 issuer 完全一致；
- `aud` 必须存在，字符串或数组中至少一项命中 `oidc_audiences`；
- 若配置了 `oidc_required_scopes`，token 的 `scope` 必须包含所有必需项；scope 可为空格分隔字符串或字符串数组，缺失视为空集合；
- 所有验签或授权失败都统一作为 `invalid_token`，不要向客户端泄露详细内部原因。

### 4.3 密钥轮换与缓存

- 找不到可用 key（包括未知 kid）时重新拉取一次 JWKS并重试选 key；
- JWKS 刷新最小间隔为 30 秒；刷新失败只记录日志并保留旧 key 集；
- 验签成功和失败结果都按原始 token 字符串缓存，空闲 30 分钟后淘汰，读取会续期；
- 成功缓存仍记录 token 的 `exp`，缓存命中时必须再次检查 exp；过期后改判失败并覆盖为失败缓存；
- 缓存不能绕过 issuer、audience、scope 或签名验证，也不能把过期 token 延长为 30 分钟有效。

### 4.4 Bearer 提取与 401

只保护搜索端点，健康检查和资源元数据公开。

- 缺少 `Authorization`、非 Bearer scheme、缺少分隔空格或 token 为空：401、空响应体，响应头：

  ```text
  WWW-Authenticate: Bearer realm="chuanshanjia"
  ```

- Bearer 值含额外空白、header 非合法文本、token 无效或过期：401，响应头：

  ```text
  WWW-Authenticate: Bearer realm="chuanshanjia", error="invalid_token", error_description="The access token is invalid or expired"
  ```

  并返回 `invalid_token` 统一 JSON 错误。

scheme 匹配不区分大小写。Python 框架若自动处理 401，需要显式核对这些响应头和 body 差异。

## 5. Service 路由与查询语义

### 5.1 预置条件优先

对请求 `filter` 与 service 预置 `filters` 做合并，同名字段使用预置值：

```text
effective_filter = request.filter overlaid by service.filters
```

若 service 配置了 `keyword`（包括空字符串），整体替换请求的 `keywords`；不要拼接两段自由文本。未配置时使用请求 keywords。

这是安全边界：调用方不能通过同名 filter 或 keywords 绕过环境隔离和数据裁剪。预置合并后再交给 provider，时间字段保持原值。

### 5.2 对象路径

`json_fields` 和 `ignores` 使用同一种路径：

- 未加引号的段以 `.` 分隔；
- 含点号或其他需要转义的段用 JSON 双引号字符串表示，例如 `attribute."kv.legacy"`；
- 引号内按 JSON 字符串规则处理反斜杠和转义；
- 空路径、空段、末尾点号、未闭合引号、引号后直接出现非 `.` 字符均非法。

例如：

```text
attribute."kv.legacy"       -> ["attribute", "kv.legacy"]
attribute."say\"hello"    -> ["attribute", "say\"hello"]
```

路径必须在启动时解析，不要在每次请求时才发现配置错误。数组下标不属于该语法。

### 5.3 清洗顺序与适用范围

清洗严格按以下顺序执行：

1. `json_fields` 字符串解析；
2. `ignores` 字段删除。

只处理响应中 `logs` 数组里的对象元素；非对象日志、缺失 logs 或 logs 非数组均保持原样。

### 5.4 `json_fields`

默认空列表时：

- 遍历每条日志对象的顶层值；
- 仅当字符串 trim 后以 `{...}` 或 `[...]` 为外形时尝试 JSON 解析；
- 合法对象或数组字符串替换原字符串；非法 JSON 保持原样；
- 不扫描嵌套对象，不递归解析刚解析出来的字符串，也不把 `"42"` 这类顶层数字字符串转换为数字。

配置非空列表时只处理指定路径：

- 沿路径逐段查找对象字段；
- 每到一个字段先尝试 JSON 解析，再继续下钻，因此父级 JSON 字符串解析后可继续解析子级；
- 指定路径上的字符串只要是合法 JSON 就替换，不限于对象或数组；
- 缺失节点、非对象中间节点、数组路径和非法 JSON 均静默保持原状。

嵌套字段通常同时配置父路径和子路径，例如：

```toml
json_fields = ["attribute", "attribute.\"kv.legacy\""]
```

### 5.5 `ignores`

对每个忽略路径，在当前对象每一级：

1. 先把所有剩余段用 `.` 连接，尝试删除一个完整的字面 key；
2. 若未删除，再取第一段作为嵌套 key，进入下一级继续。

因此 `otlp.name` 和 `"otlp.name"` 都能删除顶层字面 key `"otlp.name"`；若同时存在字面 key 和嵌套路径，字面 key 优先。`attribute."thread.name"` 删除 `attribute` 对象中的 `"thread.name"`。

缺失路径和非对象中间节点静默跳过，不删除数组元素。由于忽略在 JSON 解析之后执行，可以删除解析产物中的嵌套字段。

## 6. Provider 公共约定

每个 provider 接收 `(index, effective_search_request)`，返回统一成功响应或统一错误。filter 在原实现中按字段名排序遍历；Python 若需要查询字符串或测试请求体与参考实现逐字一致，应使用排序后的字段顺序，避免依赖普通映射的插入顺序。

所有 provider 都使用 `[start, end)`，但上游时间单位和字段不同。任何上游缺失的可选统计信息都应省略，而不是填造默认值。

## 7. Elasticsearch

### 7.1 请求

向 `{base_url}/{index}/_search` 发 POST JSON。若 base URL 已含路径，保留该路径前缀；index 和 `_search` 按 URL path segment 追加。

请求体：

```json
{
  "query": {
    "bool": {
      "filter": [
        {"term": {"attempt": 2}},
        {"term": {"retryable": true}},
        {"term": {"service": "order"}},
        {
          "range": {
            "@timestamp": {
              "gte": 1760000000,
              "lt": 1760003600,
              "format": "epoch_second"
            }
          }
        }
      ],
      "must": [
        {
          "multi_match": {
            "query": "connection timeout",
            "fields": ["*"],
            "fuzziness": "AUTO"
          }
        }
      ]
    }
  }
}
```

- 每个 filter 转换成一个 `term`，字段名原样传递，不自动加 `.keyword`；ES 不额外限制 filter 的 JSON 值类型。
- 时间 range 永远存在，固定在 filter 数组末尾。
- `keywords` 存在时加入单项 `must.multi_match`。与 SLS/TLS 不同，空字符串 keywords 在 ES 中仍会生成 `multi_match`。
- 不设置分页、排序或 size，上游返回多少就处理多少。

### 7.2 响应与错误

成功响应：

- `logs = hits.hits[*]._source`；某条 hit 缺 `_source` 时使用整个 hit；
- `count = len(logs)`；
- 上游存在 `took` 时原样复制；
- 上游 `timed_out` 为布尔值时，`complete = !timed_out`，否则省略 complete。

网络错误映射 502 `es_request_failed`。无论上游状态是否成功，body 无法解析为 JSON 都映射 502 `invalid_es_response`。上游非 2xx 且 body 是合法 JSON 时，保留上游 HTTP 状态和原始 JSON body，不再包一层统一错误。

## 8. 阿里云 SLS

### 8.1 查询转换

使用 SLS GetLogs，参数：

- `project` 来自 storage，`logstore = service.index`；
- `from = start`、`to = end`，均为 epoch seconds；
- `line = 100`，首请求 `offset = 0`；
- 查询末尾始终追加 `| with_pack_meta`。

查询构造：

```text
"connection timeout" AND "attempt": 2 AND "retryable": true AND "service": "order" | with_pack_meta
```

- keywords 非空时作为第一段，使用双引号；为空或缺失时第一段为 `*`。因此 filter-only 查询仍是 `* AND "service": "order" | with_pack_meta`。
- 字段名也用双引号，冒号后有一个空格。
- 字符串、字段名和 keywords 中的 `\` 转为 `\\`，`"` 转为 `\"`。
- 数字和布尔值使用原生标量文本，布尔为小写 `true`/`false`。
- null、数组、对象返回 400 `unsupported_sls_filter`。
- 多段按 ` AND ` 连接，字段按名称排序。

### 8.2 分页与响应

- 每页请求 100 条；页面恰好 100 条且累计不足 1000 条时，`offset += 100` 续查；
- 页面不足 100 条或累计达到 1000 条即停止，通常最多 10 页；
- 汇总所有页日志，不补空列、不改字段名大小写；SDK 返回的字段值保持字符串形式；
- `complete` 是所有已请求页面 `Complete` 状态的逻辑 AND；达到本地条数上限本身不额外改写该值；
- `took` 是服务端本地测量的所有分页请求总耗时，毫秒整数；
- `count` 是汇总后的实际条数。

### 8.3 错误

SLS SDK 的服务端错误包含错误码、消息、HTTP 状态和可选 request ID：

- 保留合法上游 HTTP 状态，`error.type = error_code`；
- reason 使用上游错误消息；有 request ID 时追加 ` (request ID: <id>)`；
- 无合法上游状态或属于其他 SDK/网络错误时，返回 502 `sls_request_failed`。

## 9. 火山云 TLS

### 9.1 查询转换

空查询为 `*`。非空查询按 ` AND ` 连接：

```text
"connection timeout" AND attempt:2 AND retryable:true AND service:"order"
```

- keywords 为空或缺失时不生成关键词段；只有 filter 时不额外添加 `*`；
- 字段名只含 ASCII 字母、数字、下划线时原样使用，否则用双引号包裹；
- 字符串值和 keywords 使用双引号；数字、布尔值使用原生文本；冒号后无空格；
- 字段名、字符串和 keywords 中的 `\` 与 `"` 分别转义为 `\\`、`\"`；
- null、数组、对象返回 400 `unsupported_tls_filter`；
- 字段按名称排序。

### 9.2 SearchLogs 请求

向 `{endpoint}/SearchLogs` 发 POST，保留 endpoint 已有路径前缀。请求头：

```text
Content-Type: application/json
X-Tls-Apiversion: 0.3.0
X-Date: <UTC YYYYMMDDTHHMMSSZ>
X-Content-Sha256: <lowercase hex SHA256 of exact body bytes>
Authorization: <signature below>
X-Security-Token: <STS token>       # 仅配置 STS 时
```

请求 body 是紧凑 JSON，字段名和顺序如下；首请求省略 `Context`：

```json
{"TopicId":"topic-1","Query":"*","StartTime":1760000000000,"EndTime":1760003600000,"Limit":100,"Sort":"desc"}
```

- `TopicId = service.index`；
- `StartTime = start * 1000`、`EndTime = end * 1000`，原实现使用饱和乘法避免整数溢出；
- `Limit = 100`，`Sort = "desc"`，固定时间逆序；
- 续查增加 `"Context":"<previous context>"`，位于 `Sort` 后。

### 9.3 火山引擎签名

使用 HMAC-SHA256，服务名固定 `TLS`。签名必须使用**实际发送的 body 字节**和实际 Host 值（含非默认显式端口，IPv6 按 URL 规则加方括号）。

1. 计算 `body_hash = lowercase_hex(SHA256(body))`。
2. 收集并按名称排序签名 headers：`content-type`、`host`、`x-content-sha256`、`x-date`、`x-tls-apiversion`；有 STS 时加入 `x-security-token`。
3. canonical headers 的每项是 `name:value\n`；signed headers 是用 `;` 连接的名称列表。
4. canonical request 依次用换行连接：HTTP 方法 `POST`、URL path、空 query、canonical headers、signed headers、body hash。注意 canonical headers 自身已以换行结尾。
5. `short_date = x_date[0:8]`，`credential_scope = "{short_date}/{region}/TLS/request"`。
6. string to sign：

   ```text
   HMAC-SHA256
   <x_date>
   <credential_scope>
   <lowercase hex SHA256(canonical_request)>
   ```

7. 逐级计算二进制 HMAC key：

   ```text
   kDate = HMAC(access_key_secret, short_date)
   kRegion = HMAC(kDate, region)
   kService = HMAC(kRegion, "TLS")
   kSigning = HMAC(kService, "request")
   signature = lowercase_hex(HMAC(kSigning, string_to_sign))
   ```

8. Authorization：

   ```text
   HMAC-SHA256 Credential=<access_key_id>/<credential_scope>, SignedHeaders=<signed_headers>, Signature=<signature>
   ```

无 STS 参考向量：

```text
access_key_id = AKIDexample
access_key_secret = secret-example
host = tls-cn-beijing.volces.com
x_date = 20260102T030405Z
path = /SearchLogs
region = cn-beijing
body = {"TopicId":"topic-1","Query":"*","StartTime":1760000000000,"EndTime":1760003600000,"Limit":100,"Sort":"desc"}

body_hash = 1f0256ed07294d804a037543118438a1dd0747be867624c6259f1e30d7cabe1b
Authorization = HMAC-SHA256 Credential=AKIDexample/20260102/cn-beijing/TLS/request, SignedHeaders=content-type;host;x-content-sha256;x-date;x-tls-apiversion, Signature=dddc8ba3ddd81d5e3b39563ec28435c41d3f29c97ca329e716046a5820d7f6a9
```

有 STS 参考向量：

```text
access_key_id = AKIDexample
access_key_secret = secret-example
security_token = STS-token-1
host = tls-ap-southeast-1.volces.com
x_date = 19991231T235959Z
path = /SearchLogs
region = ap-southeast-1
body = {"TopicId":"t","Query":"error","StartTime":0,"EndTime":1,"Limit":100,"Sort":"desc","Context":"ctx-1"}

Authorization = HMAC-SHA256 Credential=AKIDexample/19991231/ap-southeast-1/TLS/request, SignedHeaders=content-type;host;x-content-sha256;x-date;x-security-token;x-tls-apiversion, Signature=33b8248da454702419ba2ad39400d9988409bb12cc1cc4fc9164eeb3395c521d
```

### 9.4 分页与响应

读取每页的 `Logs`、`ResultStatus`、`ListOver`、`Context`、`ElapsedMillisecond`：

- `Logs` 缺失时按空页处理，日志对象保留原始字段和值；
- 页面恰好 100 条、`ListOver=false`、累计不足 1000 且存在非空 Context 时，携带 Context 续查；
- 其他情况停止；`ListOver` 缺失按 `true` 处理；
- `complete` 要求每页 `ResultStatus` 都严格等于 `"complete"`，并且最终停止时 `ListOver=true`；
- `took` 为出现过 `ElapsedMillisecond` 的各页之和；所有页都缺失时省略该字段；
- `count` 是汇总后的实际条数，单次最多通常 1000 条。

### 9.5 错误

- 请求序列化或网络失败：502 `tls_request_failed`；
- 响应不是合法 JSON，或成功响应无法解码为预期结构：502 `invalid_tls_response`；
- 非 2xx 且 JSON 同时含字符串 `ErrorCode`、`ErrorMessage`：保留上游状态，`error.type = ErrorCode`，reason 为 ErrorMessage；响应有 `X-Tls-Requestid` 时追加 request ID；
- 其他非 2xx JSON：保留上游状态和原始 JSON body。

## 10. Python 重实现建议

不预设 Web 框架或 SDK，建议按职责拆分，避免把 provider 差异重新泄漏到 API 层：

```text
app.py                  启动顺序、依赖组装、监听
config.py               配置模型、未知字段拒绝、启动校验、凭证解析
auth.py                 OIDC discovery、JWKS、JWT 验证、缓存、401
api.py                  公开端点、搜索请求模型、HTTP 错误映射
services.py             service 路由、预置合并、清洗管线、对象路径
errors.py               ProviderError 与统一错误 JSON
providers/base.py       provider 接口
providers/elasticsearch.py
providers/sls.py
providers/tls.py        查询、签名、分页、响应与错误转换
```

推荐实现顺序：

1. 配置模型、严格校验和统一错误；
2. HTTP 契约与 service 预置合并；
3. Elasticsearch 作为最小 provider，打通端到端；
4. 对象路径与清洗；
5. OIDC/JWT 和资源元数据；
6. SLS、TLS 及分页/签名；
7. 用现有 CLI 对 Python 服务做真实 HTTP 兼容性验证。

### 10.1 必须锁定的测试

- 配置：未知字段、缺失 issuer/audience、非法 URL、未知 storage、空名称、非法路径、缺失环境变量均启动失败；
- HTTP：健康检查和元数据公开；搜索必须认证； malformed JSON 为 400、未知请求字段为 422；
- 时间：默认一小时、显式区间、负时间戳、相等/反向区间；
- 认证：只接受 EdDSA/Ed25519，检查 iss/aud/exp/nbf/scope，单 key 可无 kid，多 key 必须匹配 kid，未知 kid 刷新且限频，成功缓存命中后仍检查 exp；
- 401：缺凭证为空 body，无效 token 为统一 JSON，两种 `WWW-Authenticate` 值精确匹配；
- 预置条件：同名 filter 和 keyword 不可被请求覆盖，未知 service 不调用上游；
- 清洗：默认只解析顶层对象/数组字符串，指定路径逐层解析，ignores 在解析后执行，字面点号 key 优先，数组不动；
- ES：查询 body、时间范围、空 keywords 差异、hit fallback、上游错误透传；
- SLS：查询转义、标量类型、非法 filter、100/1000 分页边界、Complete 汇总、request ID 错误；
- TLS：查询转义、紧凑请求体、毫秒时间、Context 分页、complete/took 汇总、上述两组签名参考向量、STS 和错误透传；
- 兼容性：现有 CLI 的 `search`、OIDC 登录前元数据检查、token 刷新后查询均不改变。

测试使用 mock HTTP 上游和内存 JWKS/OIDC fixture，不访问真实云服务。

## 11. 已知限制

- API 不支持客户端分页、排序、聚合或完整 ES DSL；SLS/TLS 内部最多拉取约 1000 条，TLS 固定时间逆序；
- ES filter 字段不自动映射 `.keyword`，全文和等值匹配最终取决于上游索引；
- `ignores` 不支持数组元素，`lookups` 没有运行时行为；
- 认证仅支持 EdDSA/Ed25519 + issuer/audience 校验，不存在本地 JWKS 文件模式；
- CLI naive 时间固定按 UTC+8 解释是客户端行为，Python 服务端收到的始终已是 epoch seconds。
