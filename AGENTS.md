# AGENTS.md

面向在此仓库工作的 coding agent。用户文档见 [README.md](README.md)，CLI 架构见 [docs/design.md](docs/design.md)，外部服务端的历史行为契约见 [docs/backend-migration.md](docs/backend-migration.md)。

## 项目是什么

chuanshanjia 是面向 Agent 的日志查询 CLI，当前仓库是单一 Rust package，只构建 `chuanshanjia` 二进制。CLI 通过 `base_url` 调用其他项目实现的 HTTP 服务；本仓库不再包含 `chuanshanjia-server`、storage provider 或服务端配置。

## 常用命令

```sh
mise run fmt-check      # cargo fmt --all --check
mise run test           # cargo test
mise run lint           # cargo clippy --all-targets -- -D warnings
mise run build          # cargo build
mise exec -- cargo build --release             # release 构建
cargo test <filter>                            # 过滤单测
cargo test --test cli                          # 只跑 CLI 端到端
```

CI（`.github/workflows/ci.yml`）在 macOS/Ubuntu/Windows 三平台执行全部四项；Windows 也必须通过，注意路径拼接、`EDITOR`、文件权限等跨平台写法。CI 和 release 只构建 CLI 产物。

## 仓库结构

```
src/main.rs                  # CLI：参数解析、时间归一化、HTTP 调用、config/login/logout
src/oidc.rs                  # OIDC 发现、授权码+PKCE、loopback 回调、token 刷新/存储/吊销
tests/cli.rs                 # CLI 端到端（assert_cmd + mockito）
docs/design.md               # 当前纯 CLI 设计文档
docs/backend-migration.md    # 原服务端行为契约与 Python 重实现说明
.github/workflows/ci.yml     # 三平台检查及 CLI 构建
.github/workflows/release.yml # 三平台 CLI release
```

## 硬性约定

- **行为变更必须同步文档**：改动 CLI 参数、查询语义、配置、认证、输出或错误时，更新 README.md、docs/design.md 和对应测试。
- **本仓库保持纯 CLI**：不要重新引入服务端 crate、云服务 provider 或本地 server 配置。外部服务由其他项目实现。
- **外部契约变更要有依据**：CLI 依赖的 HTTP/OIDC 接口变化必须同步 `docs/backend-migration.md`，不要擅自改变既有请求、响应或认证语义。
- **时间语义稳定**：对外统一 epoch seconds、区间 `[start, end)`；CLI 的 `YYYY-MM-DD` 固定按 UTC+8 解释，不要改成系统时区；仅传 `--start` 时，`--end` 取下一个 UTC+8 日界。
- **测试隔离**：外部 HTTP 服务与 IdP 用 mockito，外部命令和 HOME 用临时目录；不访问真实云服务或 IdP，不读取真实用户配置。新增行为先补测试。
- **CLI 不保存 client_secret**：登录必须是公开客户端 + PKCE（S256）+ loopback 回调，`state` 必须校验，不要把 secret 打进二进制。
- **OIDC 能力按实际 IdP 验证**：改动认证前通过 discovery 和测试确认 JWT access token、RFC 8707 resource、loopback redirect、offline access/refresh token 等支持；示例域名只是占位符。
- **scope 策略**：CLI 默认只请求 `openid offline_access`，不请求业务 scope；业务 scope 由 IdP 按 `resource` 授予。不要用资源元数据的 `scopes_supported` 做登录前置条件。
- **resource 配置不可省略**：CLI 的 `resource`、外部服务接受的 audience、IdP 资源登记必须一致。服务端验签与授权规则见迁移契约，本仓库不放宽这些规则。
- **凭证文件 `credentials.toml` 是程序产物**：Unix 下必须 `0600`，不要合进 `config.toml`。真实登录测试使用 `--no-browser` 和随机 loopback 端口，避免并行冲突。
- **本地敏感文件**：`server.toml` 虽已不再使用但仍可能在用户目录中且已 gitignore，不要读取、提交、删除或写入日志。
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
| CLI 参数与查询请求 | `src/main.rs` 的 usage-rs derive 与 `search`；e2e 断言在 `tests/cli.rs` |
| 时间、URL、配置、输出 | `src/main.rs` 对应函数与模块内测试 |
| CLI 登录/续期 | `src/oidc.rs`；凭证路径与 `0600` 权限、token 优先级在 `src/main.rs::resolve_token` |
| 登出/吊销 | `src/oidc.rs::revoke` + `src/main.rs::logout`，失败只警告不阻塞 |
| 外部服务端兼容契约 | `docs/backend-migration.md`；实际 Python 实现在其他项目 |
| 构建与发布 | 只允许 CLI：根 Cargo 配置、mise、`.github/workflows/` |
