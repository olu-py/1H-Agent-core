# protium-core

`1H` 指氕（protium，氢-1 同位素）。protium-core 是 1H-Agent 的 UI 无关后端核心库：会话状态机、模型/工具循环、权限与审批、SQLite 持久化，以及供所有前端适配器使用的 v2 UI 协议。

本仓库只发布 Rust 库、协议 TypeScript bindings 和一致性夹具，**不含可直接给终端用户运行的程序**。想直接用 Agent，请安装下面的消费端；想把核心接进自己的前端，请从「作为库接入」开始。

## 想直接用？安装消费端

| 消费端 | 形态 | 安装 |
| --- | --- | --- |
| [1H-Agent](https://github.com/olu-py/1H-Agent) | Ratatui/Crossterm 终端界面 | `cargo install --git https://github.com/olu-py/1H-Agent.git --locked --bin 1h-agent` |
| [1H-Agent-webUI](https://github.com/olu-py/1H-Agent-webUI) | 单二进制内嵌 HTTP/SSE 与 React 页面 | `cargo install --git https://github.com/olu-py/1H-Agent-webUI.git --locked --bin 1h-agent-web` |

两个消费端都是独立仓库，各自拥有源码、`Cargo.lock`、版本、Release 和提交历史；本仓库不使用 submodule，也不与它们共享 push 范围。安装完成后按各仓库 README 配置 Provider 与 API Key 即可开始对话。

## 版本与兼容性

| 项 | 当前值 |
| --- | --- |
| crate 版本 | `0.5.0` |
| UI 协议版本 | `protocol::PROTOCOL_VERSION = 2` |
| Edition / MSRV | 2024 / Rust 1.85 |
| 平台 | Linux、macOS、Windows |

协议只做加法演进：新增事件变体或字段必须能被旧消费端忽略，未知事件静默忽略；不重命名、不重排、不复用旧 tag。

## 能力总览

- **会话状态机**：多会话与树形子会话、切换、fork、软删、undo/redo（含文件快照回滚）；所有变更经单一命令队列串行进入核心状态机。
- **模型与工具循环**：流式思考与正文字段、工具调用参数流式进度、上下文压缩与溢出恢复；Provider 事件先归一化为公共事件再映射进协议。
- **四种模式**：`build`、`plan`、`explore`、`cluster`。集群模式下主 Agent 通过 `agent_spawn` 调度有界子 Agent，子会话以树形展示。
- **权限与审批**：工作区路径限定、SSRF 校验、默认 Deny + 变更操作需审批 + 只读模式拦截 + `permissions.tools` 覆盖；审批按 `approval_id` 决策，可本会话放行。
- **持久化**：SQLite/WAL 单连接；消息、工具调用、Provider 状态、文件快照与中断的未完成回答全部落盘。
- **上下文计量**：模型窗口按「显式配置 > 运行时发现（`GET /models`）> models.dev 社区快照 > 内置注册表」四层解析，核心是容量的唯一权威。
- **扩展能力**：OpenAI Responses 与 Chat Completions 兼容协议、本地 stdio MCP 子集、可选外部浏览器桥、自定义斜杠命令与 Agent 模板。
- **密钥**：API Key 只来自环境变量或系统钥匙串，不写入 TOML、SQLite、日志、导出或模型上下文。

## 作为库接入

在消费端的 `Cargo.toml` 中声明 Git 依赖：

```toml
protium-core = { git = "https://github.com/olu-py/1H-Agent-core.git", branch = "main" }
```

下面是接入骨架（可运行完整版本见 [`examples/minimal.rs`](examples/minimal.rs)，用 `cargo run --example minimal -- /path/to/workspace` 运行）：

```rust
use std::{path::PathBuf, time::Duration};
use protium_core::{
    config::Config,
    protocol::DEFAULT_PAGE_SIZE,
    service::{AppService, CoreConfig},
};

let workspace = PathBuf::from("/path/to/workspace").canonicalize()?;
let mut config = Config::load(None, &workspace)?; // 显式路径优先，其次平台配置目录
config.data_dir = workspace.join(".protium-data"); // 换成你自己的数据目录

let handle = AppService::start(CoreConfig {
    workspace,
    data_dir: config.data_dir.clone(),
    event_capacity: config.server.event_buffer,
    event_max_bytes: config.server.event_max_bytes,
    approval_timeout: Duration::from_secs(config.server.approval_timeout_seconds),
    message_page_size: DEFAULT_PAGE_SIZE,
    config,
})
.await?;

// 启动序列对所有消费端都是强制的：先取快照，再从快照游标原子订阅。
let snapshot = handle.snapshot().await?;
let subscription = handle
    .subscribe_from(snapshot.event_cursor)
    .map_err(|_| anyhow::anyhow!("event cursor evicted: resync required"))?;

// 之后的典型操作。
handle.submit(Some(session_id), "hello").await?; // 返回 request_seq，供取消使用
handle.approve(&approval_id, true, false).await?; // accept / allow_session
handle.shutdown().await?;
```

`AppService::start` 会用独占锁锁定工作区：第二个进程打开同一工作区会立即失败。

## 接口速查

消费端只经 `AppHandle` 驱动核心，不得触碰 `SessionRuntime`、`AgentRunner`、`Storage`、Provider 或审批 oneshot。

| 分类 | 入口 |
| --- | --- |
| 启动 / 关停 | `AppService::start(CoreConfig)` -> `AppHandle`；`AppHandle::shutdown` |
| 状态快照 | `AppHandle::snapshot()` -> `AppSnapshotV2` |
| 消息分页 | `AppHandle::messages(session_id, before, limit)` -> `MessagePage`（游标分页，limit 归一化到 20..=200） |
| 提交 / 命令 | `AppHandle::submit(session_id, text)`（返回 `request_seq`）；`AppHandle::execute_command` |
| 取消 | `AppHandle::cancel(session_id, request_seq)`（序号不匹配则静默忽略，防陈旧取消误杀新请求） |
| 会话切换 | `AppHandle::activate_session(session_id)` |
| 审批 | `AppHandle::approve(approval_id, accept, allow_session)`；本会话放行只驻留内存 |
| Provider 切换 | `set_provider` / `set_provider_config` / `set_provider_profile` / `remove_provider` |
| Provider 视图 | `provider_settings()` -> `ProviderSettingsDto`；`provider_models(refresh)` -> `ProviderModelsDto`（均为非密钥字段） |
| 密钥 | `secrets::store_api_key_cached` / `secrets::api_key_cached`（只暴露存在性与解锁入口） |
| 事件流 | `AppHandle::subscribe_from(cursor)` 原子订阅（replay + live）；或 `replay_after(cursor)` + `subscribe()` |
| 游标与容量 | `AppHandle::current_cursor()` / `event_capacity()` / `event_max_bytes()` |

### 事件流时序

1. `snapshot()` 取当前状态，记下 `event_cursor`。
2. `subscribe_from(event_cursor)` 原子返回 replay + live；replay 与 live 的重叠按 cursor 去重（跳过 `<=` 已处理游标的 live 事件）。
3. 消费滞后或游标被逐出时核心发送 `ResyncRequired`：重取快照与消息页，再从新游标重新订阅，不要猜缺失状态。
4. `Approval` 到达必须立即展示；`ApprovalResolved` 到达则关闭匹配项，并从快照收敛全局下一个审批。

完整契约、事件顺序（`ReasoningDelta* -> ReasoningCompleted -> TextDelta* -> ToolCallStreaming* -> Approval/ToolStarted`）与诊断表见 [`docs/guides/ui-contract.md`](docs/guides/ui-contract.md)。

## 协议产物

- [`bindings/`](bindings) 中的 `.ts` 由 `#[ts(export)]` 生成的 `export_bindings_*` 测试在 `cargo test` 时写出并校验，以无漂移为准（ts-rs 默认配置、`bigint`），供 Web 前端直接消费。
- [`conformance/`](conformance) 是 19 个可重放场景夹具，由 `src/conformance.rs` 在 `test-util` feature 下导出并做无漂移校验；消费端在 dev-dependencies 启用同一 feature 即可重放同一语料。
- `protium-tsgen` 二进制（`with_large_int("number")`）输出仅供参考，不参与无漂移校验。

## 配置与密钥

`Config::load(explicit_path, workspace)` 依次读取显式路径与平台配置目录下的 `1h-agent/config.toml`；全部配置键与默认值见 [`config/config.example.toml`](config/config.example.toml)。常用环境变量：

| 变量 | 作用 |
| --- | --- |
| `AGENT_API_BASE` / `AGENT_MODEL` / `AGENT_PROVIDER` | 覆盖当前 Provider 的连接、模型与协议（`chat` / `responses`） |
| `AGENT_DATA_DIR` | 会话数据库与工作区锁目录（默认平台数据目录下的 `1h-agent`） |
| `OPENAI_API_KEY` / `DEEPSEEK_API_KEY` / `DASHSCOPE_API_KEY` / `ARK_API_KEY` / `AGENT_API_KEY` | 各 Provider 的 API Key（环境变量优先于系统钥匙串） |

配置节涵盖 `[provider]`、`[[providers]]`、`[runtime]`、`[compaction]`、`[model_metadata]`、`[cluster]`、`[security]`、`[permissions.tools]`、`[browser]`、`[[commands]]`、`[[mcp_servers]]`、`[[agents]]`；`[ui]` 与 `[server]` 由消费端使用，核心忽略。所有数值上限都会在加载时归一化到安全区间。API Key 只来自环境变量或系统钥匙串，不进入 TOML、SQLite、日志、导出或模型上下文。

## 安全边界

- 文件工具只能访问 `CoreConfig::workspace` 内的路径，拒绝 `..`、绝对路径逃逸与符号链接逃逸。
- 写入、删除、命令、变更型 Git 操作与浏览器交互按策略要求审批；`permissions.tools` 可设 `allow` / `ask` / `deny`，`deny` 始终优先于本会话放行。
- 外部进程工具带超时、输出截断、取消与进程树清理；网络工具校验 HTTP/HTTPS 与公网地址（`security.allow_private_networks` 可放行私网）。
- Provider 的私有 JSON/SSE 只在下游归一化为公共事件，消费端拿不到原始模型协议载荷。

## 构建与验证

```bash
cargo build --locked
cargo test --locked          # 单测 + 集成测试 + TS bindings 与 conformance 漂移校验
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo fmt --all -- --check
```

首次构建需联网拉取 crates.io 依赖。

### 本地联调（可选）

core 与消费端需要边改边测时，把两者放在同一父目录，用命令行临时覆盖 Git 依赖：

```bash
cargo --config \
  'patch."https://github.com/olu-py/1H-Agent-core.git".protium-core.path="../1H-Agent-core"' \
  test --all-features --locked
```

该覆盖不修改受跟踪的 `Cargo.toml`，但 Cargo 可能临时改写消费端 `Cargo.lock`；本地 patch 与临时锁文件不得提交。完整的跨仓库交付顺序见下表。

## 文档导航

| 文档 | 内容 |
| --- | --- |
| [AGENTS.md](AGENTS.md) | AI/维护者入口：架构、源码路由、安全边界与分级验证 |
| [UI Contract](docs/guides/ui-contract.md) | 通用 UI 契约、事件游标/回放/resync、协议演进规则 |
| [Runtime](docs/guides/runtime.md) | 生命周期、后台容量、会话切换、取消与审批拒绝 |
| [Provider](docs/guides/provider.md) | Provider 配置、密钥、请求协议、压缩与恢复 |
| [Cluster](docs/guides/cluster.md) | AI 集群调度、审批 owner、预算与子会话 |
| [Tools](docs/guides/tools.md) | 内置工具、权限分类、路径安全与 SSRF |
| [Storage](docs/guides/storage.md) | schema/迁移、会话树、undo/redo 与快照 |
| [Release](docs/guides/release.md) | 版本、Git 交付与消费端更新顺序 |

## License

见 [LICENSE](LICENSE) 与 [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md)。