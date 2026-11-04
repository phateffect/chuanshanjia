# AGENTS.md

面向在此仓库工作的 coding agent。用户文档见 [README.md](README.md)，架构与设计决策见 [docs/design.md](docs/design.md)。

## 项目是什么

chuanshanjia 是面向 Agent 的日志查询链路：CLI（`chuanshanjia`）+ HTTP 后端（`chuanshanjia-server`），后者把统一查询转发到 Elasticsearch / 阿里云 SLS。Rust workspace，两个 crate：根 crate 是 CLI，`backend/` 是 server（lib + bin）。

## 常用命令

```sh
mise run fmt-check      # cargo fmt --all --check
mise run test           # cargo test --workspace
mise run lint           # cargo clippy --workspace --all-targets -- -D warnings
mise run build          # cargo build --workspace
mise exec -- cargo build --release --workspace   # release 构建
cargo test -p chuanshanjia-backend auth          # 过滤单测
cargo test --test cli                            # 只跑端到端
```

CI（`.github/workflows/ci.yml`）在 macOS/Ubuntu/Windows 三平台执行全部四项；Windows 也必须通过，注意路径拼接、`EDITOR`、文件权限等跨平台写法。

## 仓库结构

```
src/main.rs              # CLI：参数解析、时间归一化、HTTP 调用、config/login/logout
src/oidc.rs              # CLI 登录：OIDC 发现、授权码+PKCE、loopback 回调、token 刷新与存储
backend/src/main.rs      # server 入口：读 ./server.toml、OIDC 发现、起 axum
backend/src/lib.rs       # Router 组装（app / app_with_provider[_and_auth]）
backend/src/config.rs    # AppConfig 反序列化与校验；凭证字符串/env 双形态
backend/src/auth.rs      # OIDC 发现 + JWKS 拉取/刷新、Ed25519 验证、moka 缓存
backend/src/search.rs    # POST /{service}/_search handler 与 SearchRequest
backend/src/provider/mod.rs      # SearchProvider trait、ProviderError、UnconfiguredProvider
backend/src/provider/service.rs  # ServiceProvider：路由、预置条件、json_fields/ignores 清洗
backend/src/provider/es.rs       # Elasticsearch 实现
backend/src/provider/sls.rs      # 阿里云 SLS 实现（原生查询 + 分页）
backend/server.example.toml      # 配置模板（被 config.rs 测试校验）
tests/cli.rs             # CLI 端到端（assert_cmd + mockito）
docs/design.md           # 设计文档
```

## 硬性约定

- **行为变更必须同步文档**：改动查询语义、配置、认证、响应或错误时，更新 README.md 和 docs/design.md 对应章节。
- **`server.example.toml` 有回归测试**（`config.rs::example_config_is_valid`）。改配置结构时同步更新：示例文件、`config.rs` 测试、README「Server 配置」、design.md 第 3.6 节。
- **所有 serde 结构 `deny_unknown_fields`**（配置与请求体）。新增字段是有意收紧的 API 变更，不是随手加。
- **配置错误 fail-fast**：一切能在启动时发现的问题（未知字段、引用缺失、路径非法、凭证缺失）都必须在启动时报错，不允许带病运行。
- **`server.toml` 已在 .gitignore**，可能含真实凭证，绝不提交、绝不写入日志或错误信息。认证只有 OIDC 一种方式，不存在本地 JWKS 文件；`server.oidc_issuer` 是唯一信任配置。
- **认证只支持 EdDSA/Ed25519 + issuer 校验**。不要为“兼容”放开算法或放宽 `iss` 校验（缺失 `iss` 必须拒绝）；认证单测用 `MockFetcher` 注入路由，不联网。
- Server 只读**进程当前工作目录**的 `server.toml`；本地调试从仓库根目录启动。
- 预置条件优先级是安全语义：请求不能覆盖 service 预置的 filters/keyword，相关测试（`preset_query_takes_precedence_over_request`、`cli_cannot_override_backend_preset_query`）不可弱化。
- 时间语义统一 epoch seconds、区间 `[start, end)`；CLI 侧 naive 时间固定按 UTC+8 解释，不要改成取系统时区。
- 测试不访问真实云服务：ES 用 mockito，SLS 用 `MockSlsClient`，外部命令用临时目录。新增行为先补测试。
- **CLI 不保存 client_secret**：登录必须是公开客户端 + PKCE（S256）+ loopback 回调，`state` 必须校验。不要把 secret 打进二进制。
- **OIDC 能力按实际 IdP 验证**：改动认证前通过 discovery 和测试确认 JWT access token、RFC 8707 resource、loopback redirect、offline access/refresh token 等支持；示例域名只是占位符。
- **scope 策略**：CLI 默认只请求标准 OIDC scope（`openid offline_access`），不请求业务 scope；业务 scope 由 IdP 按 `resource` 授予，Server 用 `oidc_required_scopes` 校验实际 token。不要为了“兼容”把业务 scope 写进 CLI 默认值，也不要用 `scopes_supported` 做登录前置条件（会让按 resource 授予的 IdP 全部误报）。验签通过不代表拥有业务权限。
- **`aud` 校验不可放宽**：`server.oidc_audiences` 必须与 CLI 的 `resource`、IdP 的资源登记三处一致；测试覆盖了「缺失 aud」「其他 client 的 aud」被拒。
- **凭证文件 `credentials.toml` 是程序产物**，Unix 下必须 `0600`；不要把它合进 `config.toml`。新增测试若跑真实登录，用独立的固定 `--port` 避免并行冲突，并传 `--no-browser`。
- clippy `-D warnings` 零容忍；edition 2024 语法（let-chains 等）可用。

## Git 工作流（GitHub）

- commit 一律带 agent 身份：`git commit --author='agent <agent@phateffect.com>'`。
- 永不 `--no-verify`；hook 失败修根因重跑。永不 force push。
- 完成编码任务后推送分支并创建 PR：
  `git push -u origin HEAD`
  `gh pr create --title "$TITLE" --body "$BODY"`
- 处理 PR 评论：`gh pr view --comments $PR_NUMBER`，逐条响应人工评论，忽略 bot 评论。

## 修改入口速查

| 想改什么 | 动哪里 |
| --- | --- |
| 新增 storage provider | `provider/` 新模块实现 `SearchProvider`、`config.rs::StorageConfig` 加变体、示例与文档 |
| 查询/清洗语义 | `provider/service.rs`（预置合并、json_fields、ignores） |
| 认证 | `auth.rs`；缓存参数是 `CACHE_IDLE_TIMEOUT` |
| 登出/吊销 | `src/oidc.rs::revoke`（RFC 7009）+ `src/main.rs::logout`，失败只警告不阻塞 |
| API 请求/响应结构 | `search.rs`；注意 CLI 的请求体拼装在 `src/main.rs::search` |
| CLI 参数 | `src/main.rs` 的 usage-rs derive；e2e 断言在 `tests/cli.rs` |
| CLI 登录/续期 | `src/oidc.rs`；凭证文件路径与 `0600` 权限、token 优先级在 `src/main.rs::resolve_token` |
