# protium-core

`1H` 指氕（protium，氢-1 同位素）。protium-core 是 1H-Agent 的 UI 无关后端核心库：会话状态机、模型/工具循环、权限与审批、SQLite 持久化，以及面向所有前端适配器的 v2 通用 UI 协议。

本仓库只提供 Rust 库、协议夹具和 TypeScript bindings，不提供可供终端用户直接使用的 TUI 或 WebUI。普通用户应选择一个消费端：

- [1H-Agent](https://github.com/olu-py/1H-Agent)：Ratatui/Crossterm TUI。
- [1H-Agent-webUI](https://github.com/olu-py/1H-Agent-webUI)：Axum REST/SSE + React WebUI。

两个消费端都是独立 Git 仓库，各自拥有源码、`Cargo.lock`、版本、Release 和提交历史；本仓库不使用 submodule，也不与消费端共享 push 范围。

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
| 审批 | `AppHandle::approve(approval_id, accept, allow_session)`；会话放行仅驻留内存 |
| Provider 切换 | `AppHandle::set_provider(preset, model)` |
| 事件流 | `AppHandle::subscribe_from(cursor)` 原子订阅（replay + live）；或 `replay_after` + `subscribe` |

启动序列（所有消费端必须遵守）：先 `snapshot()`，再 `subscribe_from(snapshot.event_cursor)` 原子订阅并按 cursor 去重；`ResyncRequired`（游标逐出）或消费滞后时重取快照 + 消息页并重新订阅。协议只做加法演进：新变体/字段必须被旧消费端忽略，未知事件静默忽略。

完整契约见 [docs/guides/ui-contract.md](docs/guides/ui-contract.md)；可运行示例见 [examples/minimal.rs](examples/minimal.rs)：

```bash
cargo run --example minimal -- /path/to/a/workspace
```

## TypeScript 绑定

`bindings/` 内的 `.ts` 文件由 `#[ts(export)]` 生成的 `export_bindings_*` 测试在 `cargo test` 时再生成并验证（ts-rs 默认配置、`bigint`），以无漂移为准，供 Web 前端直接消费。`protium-tsgen` 二进制（`with_large_int("number")`）输出仅供参考。

## 作为 Git 依赖使用

TUI 和 WebUI 通过 Git 依赖使用本仓库的 `main` 分支：

```toml
protium-core = { git = "https://github.com/olu-py/1H-Agent-core.git", branch = "main" }
```

消费端提交自己的 `Cargo.lock`，其中锁定本仓库的具体 commit。普通用户执行 `cargo build --locked` 时只会获取这个锁定版本，不会因为 `main` 前进而自动改变 core。维护者只有在需要接入新版 core 时才执行：

```bash
cargo update -p protium-core
```

更新后的消费端必须提交自己的 `Cargo.lock` 和适配改动。普通 `cargo update` 还会更新其他依赖，因此 core 维护应使用上面的定向命令。

## 跨仓库维护流程

1. 在本仓库修改 core；协议变更同时更新 `conformance/` 和 `bindings/`。
2. 完成本仓库验证，提交并 push 到 `main`。
3. 在 TUI 仓库执行 `cargo update -p protium-core`，完成适配、conformance 测试并独立提交。
4. 在 WebUI 仓库执行 `cargo update -p protium-core` 和 `bash scripts/core-bindings.sh sync`，完成 Rust/TypeScript 适配并独立提交。

不得直接修改 Cargo 缓存中的 Git checkout，也不得把 core 源码复制回消费端。需要并行开发时，应把三个仓库分别 clone 到互不嵌套的目录。

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

## 维护协议

AI 维护协议见 [AGENTS.md](AGENTS.md)；协议适配见 [UI Contract](docs/guides/ui-contract.md)，跨仓库发布顺序见 [Release](docs/guides/release.md)。

## License

见 [LICENSE](LICENSE) 与 [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md)。
