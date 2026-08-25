# protium-core AI 维护协议

> 先读本文件，再按任务路由只读一个相关专题及目标源码；跨领域任务才组合读取，禁止为背景扫描整个仓库。

## 稳定上下文

```text
project: protium-core（1H = 氕/protium；1H-Agent 的 UI 无关后端核心库）
goal: 极致轻量、高性能、权限感知的 Agent 核心；核心独立演进，TUI/WebUI/Desktop 为外部消费端项目
runtime: Rust/Tokio 单库（SQLite/WAL）；消费端只经通用接口连接核心
authority: 源码 > config/config.example.toml > 本文件 > 专题指南
scope: 核心状态机、通用 UI 协议、模型流、受控工具、多会话、AI 集群
excluded: TUI/WebUI/Desktop 源码、HTTP/SSE 服务器、内置浏览器、远程 MCP、动态插件、图片语音能力
```

核心独占 SessionRuntime/AgentRunner/Provider 与密钥/ToolRegistry 与 Security/Storage(SQLite)/审批 oneshot，以及命令串行队列与取消、关停逻辑；消费端不得触碰以上任何一项。不引入 Node.js、Python、Chromium、动态插件 ABI 或后台轮询。所有路径、网络、工具、进程、缓存、channel 和输出必须有边界、取消与释放路径。

## 一分钟工作流

1. 先运行 `git status --short --branch`，识别并保护用户已有改动。
2. 用 `rg` 定位定义、直接调用者、事件变体和相邻测试；只读任务命中的专题。
3. 核心状态机在 `src/service.rs`（`AppService`/`Engine`/`AppHandle`），单会话在 `src/session.rs`（`SessionRuntime`），模型/工具循环在 `src/agent.rs`（`AgentRunner`）；消费端契约在 `src/protocol.rs`/`src/bridge.rs`。
4. 修改事件、配置或持久化类型时，覆盖所有构造点、match、序列化、恢复和测试。
5. 先跑最小目标测试；跨模块行为才升级到完整 Clippy 和测试。
6. 需要下游适配时先完成并 push core，再到各消费端定向更新；禁止修改 Cargo Git checkout。

## 任务路由

| 领域 | 首读入口 | 专题 |
| --- | --- | --- |
| 通用 UI 契约、事件游标/回放、resync | `src/protocol.rs`、`src/bridge.rs`、`src/service.rs` | [UI Contract](docs/guides/ui-contract.md) |
| 启动、全局状态、会话路由 | `src/service.rs`、`src/app.rs` | [Runtime](docs/guides/runtime.md) |
| Provider、模型、密钥、协议、压缩恢复 | `src/config.rs`、`src/agent.rs`、`src/provider/openai.rs` | [Provider](docs/guides/provider.md) |
| 子 Agent、审批、取消、集群停滞 | `src/agent.rs`、`src/service.rs` | [Cluster](docs/guides/cluster.md) |
| 工具、路径、SSRF、外部进程 | `src/tools/`、`src/security.rs` | [Tools](docs/guides/tools.md) |
| 会话、分支、迁移、持久化 | `src/storage.rs`、`src/session.rs` | [Storage](docs/guides/storage.md) |
| 配置上限、容量归一化、新增配置键 | `src/config.rs` 的 `Config::load` clamp 区、`config/config.example.toml` | Provider 专题（容量预算）；同步 `defaults_are_bounded` 类测试 |
| 版本、bindings/conformance 交付、消费端更新 | `Cargo.toml`、`Cargo.lock`、`bindings/`、`conformance/` | [Release](docs/guides/release.md) |

指南与源码不一致时以源码为准，并在同一改动中更新该指南；一个事实只归属根文档或一个专题。

## 消费端接入约束（摘要）

消费端只允许 `AppService::start(CoreConfig)`、`AppHandle` 的 snapshot/messages/submit/execute_command/approve/cancel/activate_session/set_provider/subscribe/shutdown 接口，以及 `protocol.rs` DTO 与 `bridge.rs` 原子订阅（`subscribe_from`/`ResyncRequired`）。启动先取 snapshot，再 `subscribe_from(event_cursor)` 原子订阅并按 cursor 去重；`submit` 返回请求序号、`cancel` 携带之防陈旧取消；协议只做加法演进，未知事件静默忽略。完整契约见 UI Contract 专题，接口示例见 `examples/minimal.rs`。

## 实施与验证

| 改动 | 最小验证 |
| --- | --- |
| 文档 | `bash scripts/check-agent-docs.sh`、`git diff --check` |
| 迭代中 | `cargo test --lib --all-features --locked <filter>` |
| 局部 Rust 完成 | `cargo fmt --all -- --check`、`cargo test --lib --all-features --locked` |
| 工具/存储/安全/进程或跨模块 | `cargo clippy --all-targets --all-features --locked -- -D warnings`、`cargo test --all-features --locked` |
| 发布给消费端 | 完整验证后先提交并 push core；再由 TUI/WebUI 各自更新锁文件与适配 |

保持改动聚焦，复用现有 helper，不清理无法证明无用的文件。事件/协议/持久化类型改动必须覆盖所有构造点、match、序列化与恢复测试。本仓库自 1H-Agent 提取（消费端项目各自持有前端源码）；未运行的检查必须在最终回复说明。
