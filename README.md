# protium-core

`1H` 指氕（protium，氢-1 同位素）。protium-core 是 1H-Agent 的 UI 无关后端核心库：会话状态机、模型/工具循环、权限与审批、SQLite 持久化，以及供所有前端使用的 v2 UI 协议。

本仓库只发布 Rust 库、协议 TypeScript bindings 和一致性夹具，不含可直接给终端用户运行的程序。

## 安装消费端

| 消费端 | 形态 | 安装 |
| --- | --- | --- |
| [1H-Agent](https://github.com/olu-py/1H-Agent) | Ratatui/Crossterm 终端界面 | `cargo install --git https://github.com/olu-py/1H-Agent.git --locked --bin 1h-agent` |
| [1H-Agent-webUI](https://github.com/olu-py/1H-Agent-webUI) | 单二进制内嵌 HTTP/SSE 与 React 页面 | `cargo install --git https://github.com/olu-py/1H-Agent-webUI.git --locked --bin 1h-agent-web` |

两仓各自拥有源码、`Cargo.lock`、版本、Release 和提交历史；本仓库不使用 submodule，也不与它们共享 push 范围。安装后按各仓 README 配置 Provider 与 API Key。

## 作为库接入

```toml
protium-core = { git = "https://github.com/olu-py/1H-Agent-core.git", branch = "main" }
```

启动骨架（可运行完整版见 [`examples/minimal.rs`](examples/minimal.rs)，用 `cargo run --example minimal -- /path/to/workspace` 运行）：

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

`AppService::start` 会以独占锁锁定工作区：第二个进程打开同一工作区会立即失败。

接口语义、事件顺序与游标规则以 [Core UI Contract](../.agents/guides/core/ui-contract.md) 为准；这里只列启动五步：`AppService::start` → `snapshot()` → `subscribe_from(cursor)` → `submit(session_id, text)` → `shutdown()`。

## 版本与兼容性

| 项 | 当前值 |
| --- | --- |
| crate 版本 | `0.5.0` |
| UI 协议版本 | `protocol::PROTOCOL_VERSION = 2` |
| Edition / MSRV | 2024 / Rust 1.85 |
| 平台 | Linux、macOS、Windows |

协议只做加法演进：新增事件变体或字段必须能被旧消费端忽略，未知事件静默忽略；不重命名、不重排、不复用旧 tag。

## 能力

会话树与 fork/软删/undo（含文件快照回滚）；流式思考与工具调用；上下文压缩与溢出恢复；四种模式 `build`/`plan`/`explore`/`cluster`；权限审批、SSRF 校验与只读拦截；SQLite/WAL 持久化，消息、工具调用、Provider 状态与中断回答全部落盘；上下文窗口按「显式配置 > 运行时发现 > models.dev 快照 > 内置注册表」四层解析；OpenAI Responses/Chat 兼容、stdio MCP 子集、自定义斜杠命令与 Agent 模板。

安全边界：文件工具限制在 `CoreConfig::workspace` 内，拒绝 `..`、绝对路径与符号链接逃逸；写入、删除、命令和浏览器交互按策略审批，`permissions.tools` 的 `deny` 优先于本会话放行。API Key 只来自环境变量或系统钥匙串，不写入 TOML、SQLite、日志、导出或模型上下文。

## 配置

`Config::load(explicit_path, workspace)` 依次读取显式路径与平台配置目录下的 `1h-agent/config.toml`；全部键与默认值见 [`config/config.example.toml`](config/config.example.toml)。常用环境变量：

| 变量 | 作用 |
| --- | --- |
| `AGENT_API_BASE` / `AGENT_MODEL` / `AGENT_PROVIDER` | 覆盖当前 Provider 的连接、模型与协议（`chat` / `responses`） |
| `AGENT_DATA_DIR` | 会话数据库与工作区锁目录（默认平台数据目录下的 `1h-agent`） |
| `OPENAI_API_KEY` / `DEEPSEEK_API_KEY` / `DASHSCOPE_API_KEY` / `ARK_API_KEY` / `AGENT_API_KEY` | 各 Provider 的 API Key（环境变量优先于系统钥匙串） |

## 协议产物

- [`bindings/`](bindings)：`.ts` 由 `#[ts(export)]` 在 `cargo test` 时写出并校验无漂移（ts-rs 默认配置、`bigint`），供 Web 前端直接消费。
- [`conformance/`](conformance)：19 个可重放场景，由 `src/conformance.rs` 在 `test-util` feature 下导出并做无漂移校验；消费端在 dev-dependencies 启用同一 feature 即可重放。
- `protium-tsgen`（`with_large_int("number")`）输出仅供参考，不参与无漂移校验。

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

该覆盖不修改受跟踪的 `Cargo.toml`，但 Cargo 可能临时改写消费端 `Cargo.lock`；本地 patch 与临时锁文件不得提交。

## 文档

| 文档 | 内容 |
| --- | --- |
| [AGENTS.md](AGENTS.md) | Core 模块维护入口 |
| [本地共享 Core 指南](../.agents/guides/core/) | Runtime、Provider、Cluster、Tools、Storage、UI Contract 与 Release |
| [Git 工作流 Skill](../.agents/skills/git-pr-local-sync/SKILL.md) | commit、push、PR 检查/合并与本地同步 |

共享指南保存在同级 `1HAgent/` 工作区，不随此仓库单独 clone 或 push。

## License

见 [LICENSE](LICENSE) 与 [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md)。
