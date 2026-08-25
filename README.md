# protium-core

`1H` 指氕（protium，氢-1 同位素）。protium-core 是 1H-Agent 的 **UI 无关后端核心库**：会话状态机、模型/工具循环、权限与审批、SQLite 持久化，以及面向所有前端适配器的 v2 通用 UI 协议。本仓库只含核心库与协议绑定，**不含 TUI、HTTP 服务或任何前端源码**--前端（TUI/WebUI/Desktop）作为独立消费端项目，仅通过下述通用接口接入。

> 本项目提取自 1H-Agent 仓库的 `crates/protium-core`（前后端分离：核心独立演进，消费端各自适配）。

## 通用接口（前端接入指南）

所有消费端只允许经以下接口驱动核心，不得触碰 `SessionRuntime`/`AgentRunner`/Provider/Storage/审批 oneshot 等内部实现：

| 能力 | 入口 |
| --- | --- |
| 启动 / 关停 | `service::AppService::start(CoreConfig)` -> `AppHandle`；`AppHandle::shutdown` |
| 状态快照 | `AppHandle::snapshot()` -> `protocol::AppSnapshotV2` |
| 消息分页 | `AppHandle::messages(session_id, before, limit)` -> `protocol::MessagePage`（游标分页） |
| 提交输入 / 命令 | `AppHandle::submit`（返回请求序号）；`/` 前缀文本走 `AppHandle::execute_command` |
| 取消 | `AppHandle::cancel(session_id, request_seq)`（序号不匹配则静默忽略，防陈旧取消） |
| 会话切换 | `AppHandle::activate_session` |
| 审批 | `AppHandle::approve(approval_id, accept)` |
| Provider 切换 | `AppHandle::set_provider(preset, model)` |
| 事件流 | `AppHandle::subscribe_from(cursor)` 原子订阅（replay + live）；或 `replay_after` + `subscribe` |

启动序列（所有消费端必须遵守）：先 `snapshot()`，再 `subscribe_from(snapshot.event_cursor)` 原子订阅并按 cursor 去重；`ResyncRequired`（游标逐出）或消费滞后时重取快照 + 消息页并重新订阅。协议只做加法演进：新变体/字段必须被旧消费端忽略，未知事件静默忽略。

完整契约见 [docs/guides/ui-contract.md](docs/guides/ui-contract.md)；可运行示例见 [examples/minimal.rs](examples/minimal.rs)：

```bash
cargo run --example minimal -- /path/to/a/workspace
```

## TypeScript 绑定

`bindings/` 内的 `.ts` 文件由 `#[ts(export)]` 生成的 `export_bindings_*` 测试在 `cargo test` 时再生成并验证（ts-rs 默认配置、`bigint`），以无漂移为准，供 Web 前端直接消费。`protium-tsgen` 二进制（`with_large_int("number")`）输出仅供参考。

## 消费端更新

TUI 和 WebUI 通过 Git 依赖使用本仓库的 `main` 分支：

```toml
protium-core = { git = "https://github.com/olu-py/1H-Agent-core.git", branch = "main" }
```

核心变更合并后，消费端执行 `cargo update -p protium-core` 获取新提交，再按适配器的测试和协议绑定流程更新。WebUI 将本仓库提交的 `bindings/` 同步到自己的 `web/ts/`；Cargo 不会自动运行 `protium-tsgen`。

## 构建与测试

```bash
cargo build --locked
cargo test --locked          # 单测 + 集成测试 + TS 绑定漂移校验
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo fmt --all -- --check
```

首次构建需联网拉取 crates.io 依赖（冷缓存较慢属正常）。

## 配置与密钥

`Config::load(explicit_path, workspace)` 依次读取显式路径与 `~/.config/1h-agent/config.toml`；全部配置键说明见 [config/config.example.toml](config/config.example.toml)。API Key 只来自环境变量或系统钥匙串，不进入 TOML、SQLite、日志、导出或模型上下文。

## 消费端项目

- 1H-Agent（TUI 消费端）：Ratatui/Crossterm 适配器。
- 1H-Agent-webUI（Web 消费端）：Axum REST/SSE 适配器 + React 前端。

两者均为独立仓库，各自持有 UI 源码与传输层，只依赖本核心的通用接口。

## 维护协议

AI 维护协议见 [AGENTS.md](AGENTS.md)；专题指南见 [docs/guides/](docs/guides/)。

## License

见 [LICENSE](LICENSE) 与 [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md)。
