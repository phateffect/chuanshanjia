# chuanshanjia

chuanshanjia 是面向 Agent 的日志查询 CLI。它把命令行条件转换为统一的 HTTP 查询请求，并将响应以 JSON 输出，方便 Agent 继续组合处理。

**本仓库只维护 `chuanshanjia` CLI。** HTTP 服务端由其他项目实现；CLI 通过 `base_url` 连接兼容服务，不在本地直接访问 Elasticsearch、阿里云 SLS 或火山云 TLS。

```text
Agent ──▶ chuanshanjia CLI ──HTTP──▶ 外部日志查询服务 ──▶ Elasticsearch / SLS / TLS
             │
             └─ ~/.config/chuanshanjia/config.toml + credentials.toml
```

- CLI 架构与设计：[docs/design.md](docs/design.md)
- 服务端行为契约与 Python 重实现说明：[docs/backend-migration.md](docs/backend-migration.md)

## 构建

使用 [mise](https://mise.jdx.dev/) 安装 Rust 工具链：

```sh
mise install
mise run build
```

构建 release：

```sh
mise exec -- cargo build --release
```

产物只有 `chuanshanjia`，位于 `target/debug/` 或 `target/release/`；Windows 产物带 `.exe` 后缀。下文假设它已加入 `PATH`，也可以直接使用上述目录中的路径。

## 配置

CLI 使用 `~/.config/chuanshanjia/config.toml`；Windows 下是 `%USERPROFILE%\.config\chuanshanjia\config.toml`。运行以下命令可创建配置并用 `EDITOR` 打开：

```sh
chuanshanjia config
```

最小配置只需填写外部 HTTP 服务的根地址和查询所需 token：

```toml
base_url = "http://127.0.0.1:3000"
token = "your-jwt"
```

如果希望通过 `chuanshanjia login` 获取和续期 token，则配置 OIDC 参数：

```toml
base_url = "http://127.0.0.1:3000"
issuer = "https://oidc.example.com"
client_id = "chuanshanjia"
resource = "https://logs.example.com"
```

`base_url` 可以包含路径前缀，CLI 会在其后追加 `/api/services`、`/api/sellers`、`/api/services/{service}/_search` 或 `/api/services/{service}/_trace`。`config` 不会覆盖已有文件；`EDITOR` 支持带参数的命令。

## 列出服务

```sh
chuanshanjia services [--token JWT]
```

请求 `GET /api/services`，成功时输出按名称排序的服务 JSON 数组，每项包含 `name` 和 `description`。

## 查询卖家

```sh
chuanshanjia sellers <SELLER> [--platform PLATFORM] [--token JWT]
```

请求 `GET /api/sellers`，成功时输出卖家 JSON 数组，每项包含 `leyan_seller_id`、`platform`、`seller_id`、`seller_nick`、`store_title` 和 `enabled`（由服务端按 `organization_id,platform,seller_id` 排序）。`SELLER` 按形状自动识别：

- 纯 ASCII 数字（sellerId 或 storeId）：发送 `seller_id=<SELLER>`；
- UUID 形状（leyanSellerId）：发送 `leyan_seller_id=<SELLER>`；
- 其他（sellerNick）：发送 `seller_nick=<SELLER>`，此时 `--platform` 必填，服务端只接受 sellerNick 与 platform 一起查询。

`--platform PLATFORM` 会额外发送 `platform=<PLATFORM>`，因此也可以和 sellerId 或 leyanSellerId 组合，把结果限定在某个平台。

## 查询日志

```sh
chuanshanjia search foo \
  -f service=order \
  -k "connection timeout" \
  --start="2026-10-14T10:01:02+08" \
  --end=1792994561
```

命令形式：

```text
chuanshanjia services [--token JWT]
chuanshanjia sellers <SELLER> [--platform PLATFORM] [--token JWT]
chuanshanjia search <SERVICE> [-f KEY=VALUE]... [-k KEYWORD] [--token JWT] [--start TIME] [--end TIME]
chuanshanjia trace <SERVICE> <TRACE_ID> [--token JWT] [--start TIME] [--end TIME]
chuanshanjia login [--issuer URL] [--client-id ID] [--redirect-uri URI] [--resource URI] [--scope SCOPES] [--no-browser]
chuanshanjia logout [--local]
chuanshanjia config
```

- `SERVICE` 必填，对应外部服务中的 service 名称。
- `-f, --filter KEY=VALUE` 可重复，key 不能为空，value 按字符串处理。
- `-k, --keyword KEYWORD` 最多出现一次；`-f` 和 `-k` 至少提供一个。
- `--token JWT` 可选，覆盖本地登录会话和配置文件中的 `token`，并以 `Authorization: Bearer <JWT>` 发送。
- 外部服务可配置预置 filter 和 keyword；这些服务端条件是否优先由服务端契约决定，CLI 不在本地合并。
- 查询结果的具体匹配、分页、排序和字段清洗规则由外部服务及其 provider 决定。

`services`、`sellers`、`search` 和 `trace` 成功时输出格式化 JSON；服务返回非 2xx 时，CLI 输出状态码与响应体并以非零状态退出。输出写 stdout，`| head` 这类提前关闭管道的情况不会 panic。

### 时间语义

`--start` 和 `--end` 支持三种格式：

1. `YYYY-MM-DD`，固定按 Asia/Shanghai（UTC+8）当天 00:00:00 解释，如 `2026-10-14`。
2. Epoch seconds，如 `1792994561`。
3. 带显式时区的 ISO 8601/RFC 3339，如 `2026-10-14T10:01:02+08`、`2026-10-14T10:01:02+08:00`、`2026-10-14T02:01:02Z`，也接受其他合法偏移。

`--start` 省略时默认为 UTC+8 当天 00:00:00。只显式提供 `--start` 时，`--end` 自动取 `start` 之后的下一个 UTC+8 日界；即使 `start` 正好是当天零点，也取次日零点。两者都省略时，`--end` 默认为当前时间。始终要求 `start < end`。

不再接受空格分隔且不带时区的 `YYYY-MM-DD HH:mm:ss`。CLI 将时间统一转换为 epoch seconds，按 `[start, end)` 请求服务。对 `localhost`、`127.0.0.1`、`::1` 目标自动绕过系统代理。

## 跨服务追踪

```sh
chuanshanjia trace kt-mixer-taobao 84096828bfc209f708187f4be5a793de
```

请求 `POST /api/services/{SERVICE}/_trace`，发送 `trace_id`、`start` 和 `end`，时间语义与 `search` 相同。服务端会把锚点 service 及其声明的 related 服务（传递闭包）并行查询，按时间排序合并后返回：响应在搜索响应之上增加 `sources`，记录每个源的服务名、条数、耗时与失败原因；单个源查询失败不会拖垮整体。

## OIDC 登录与登出

CLI 直接向 OIDC IdP 登录并获取 token；外部 HTTP 服务负责验证 token，CLI 不签发也不验证服务端权限。

```sh
chuanshanjia login                 # 打开浏览器完成登录
chuanshanjia login --no-browser    # 只打印授权 URL，不自动打开浏览器
chuanshanjia logout                # 尽力吊销会话并删除本地 token
chuanshanjia logout --local        # 只删除本地 token
```

登录使用**授权码 + PKCE（S256）+ 公开客户端**，不保存 client_secret。回调地址为 `http://127.0.0.1:<port>/callback`。

- `issuer`、`client_id`、`resource` 可写在配置中，也可用同名参数覆盖。
- IdP 需要注册公开客户端（token 认证方式 `none`，启用 PKCE）。按 RFC 8252，默认使用 `http://127.0.0.1:0/callback`，由系统选择空闲端口；需要精确匹配固定 URI 的 IdP 可通过 `redirect_uri` 或 `--redirect-uri` 指定已注册地址。
- CLI 默认请求 `scope = "openid offline_access"`，不请求业务 scope；配置项 `scope` 或 `--scope` 可整体覆盖。带 `offline_access` 时授权请求附加 `prompt=consent`。
- 业务 scope 由 IdP 按 `resource` 授予。
- `resource` 没有内置默认值，必须配置或通过 `--resource` 传入。它必须与外部服务的资源标识及 IdP 的资源登记一致。
- 登录前只校验本地 issuer、client、scope 和 resource 配置，不请求后端元数据，因此首次登录不需要已有后端 token。
- OIDC 本地登录会话只接受 JWT 形态的 access token；不透明或格式不符时报错。

token 保存在 `~/.config/chuanshanjia/credentials.toml`，Unix 下权限为 `0600`，不与手写的 `config.toml` 混合。

`services`、`sellers`、`search` 和 `trace` 的 token 优先级为 `--token` > 本地登录会话 > 配置文件 `token`。会话进入过期窗口时先用 refresh token 静默续期并回写文件，续期失败则提示重新 `login`。过期时间优先使用 IdP 返回的 `expires_in`；授权码兑换未返回该字段时，退回按 access token 的 JWT `exp` 判断。

`logout` 默认先按 RFC 7009 使用 refresh token 调 IdP 的 revocation endpoint，再删除本地凭证。IdP 不可达时只打印警告，仍删除本地文件，保证离线也能登出。

## 外部 HTTP 服务

CLI 依赖四个受 Bearer token 保护的服务端接口：

- `GET /api/services`：列出服务名称与说明；
- `GET /api/sellers`：按 sellerNick、sellerId/storeId 或 leyanSellerId 查卖家，可用 `platform` 限定平台；
- `POST /api/services/{service}/_search`：发送封闭扁平 schema 的等值 filter 字段（`env`/`version`/`level`/`host`/`trace_id`/`span_id`）、`keyword`、`start`、`end` 和 Bearer token，接收 JSON 响应；
- `POST /api/services/{service}/_trace`：发送 `trace_id`、`start`、`end`，返回 `sources` 与按时间合并后的 `logs`。

原 Rust 服务端已经移出本仓库。Python 重实现所需的完整请求、响应、认证、provider 和错误契约见 [docs/backend-migration.md](docs/backend-migration.md)。

## 开发与验证

```sh
mise run fmt-check
mise run test
mise run lint
mise run build
```

只运行 CLI 端到端测试：

```sh
cargo test --test cli
```

测试使用 assert_cmd 驱动真实 CLI 二进制，以 mockito 模拟外部 HTTP 服务和 IdP；覆盖参数、时间、token 传递、错误输出、OIDC 登录、state 校验、PKCE、静默续期与登出，不读取真实用户配置，也不访问真实云服务。

GitHub Actions 在 macOS、Ubuntu、Windows 上执行格式检查、测试、clippy 和构建，并只发布三个平台的 CLI 产物。推送 `vX.Y.Z` tag 时，release 构建将 Cargo 包版本设为 `X.Y.Z`，并在上传前检查 `-V` 和 `--version` 均输出 `chuanshanjia X.Y.Z`。

## 当前限制

- 本仓库没有服务端实现，不能单独完成日志存储查询；需要部署兼容的外部 HTTP 服务。
- CLI 的 `-f` 值全部是字符串；数字、布尔等 JSON 标量只能由其他 HTTP 客户端直接表达。
- CLI 不提供完整 ES DSL、聚合或服务端 provider 配置能力。
- 登录未实现 device code flow，token 未存入系统钥匙串。
