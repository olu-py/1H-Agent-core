# Core 发布与消费端更新指南

## 适用范围

core 版本、Git `main` 交付、bindings/conformance 产物，以及 TUI/WebUI 消费端更新顺序。

## 入口

- `Cargo.toml`/`Cargo.lock`：core 自身版本与依赖；`bindings/`、`conformance/`：协议交付物。
- TUI/WebUI 的 `Cargo.toml`/`Cargo.lock`：Git 依赖声明与实际锁定 commit。

## 不变量

- core、TUI、WebUI 是三个独立仓库；版本号、tag、提交和 push 互不隐含同步。
- 联调可用 Cargo `--config` 本地 path patch，但它不是交付来源；patch 与临时 path 锁文件不得提交。
- 先完成并 push core，再由消费端执行 `cargo update -p protium-core`；普通用户只用消费端已提交的锁文件。
- 协议变更必须先在 core 提交 bindings/conformance；WebUI 从锁定 checkout 同步 bindings，TUI 通过 `test-util` 重放夹具。
- 不修改 Cargo 缓存 checkout，不复制 core 源码，不以 submodule 连接仓库。

## 诊断

| 症状 | 检查顺序 |
| --- | --- |
| 消费端仍使用旧 core | core 远端 SHA -> 消费端 `Cargo.lock` source SHA -> 是否执行定向 update |
| WebUI 类型漂移 | 锁定 core commit -> core `bindings/` -> `core-bindings.sh sync/check` |
| 消费端意外锁到 path | 移除本地 patch -> 定向 update -> metadata source Git -> `--locked` 复测 |
| 本仓库 push 混入前端 | 当前仓库根目录 -> `git status` -> 是否把仓库互相嵌套 |

## 验证

```bash
cargo fmt --all -- --check
cargo test --all-features --locked
cargo clippy --all-targets --all-features --locked -- -D warnings
bash scripts/check-agent-docs.sh
git diff --check
```

push core 后分别在 TUI/WebUI 更新和验证；消费端失败时修消费端适配，不把状态机或协议逻辑复制过去。

## CI 与自动升级

- core `main` 的 required checks：`Linux quality`、`Minimum Rust (1.85.0)`、`Test (macos-latest)`、`Test (windows-latest)`；跨仓库升级 PR 不直接写 `main`。
- required-check 名保持稳定；runner 固定 Ubuntu 24.04、macOS 26、Windows 2025（发布 ARM64/Intel 各用原生 macOS runner）；Action 固定完整 SHA并由每周 Dependabot 分组更新，checkout 不保留凭据，缓存仅允许 `main` CI 写入。
- 发布 `v*` tag 后，`release-dispatch.yml` 使用 `CORE_UPGRADE_TOKEN` 通知消费端；未配置时由消费端每周 schedule 检测 core `main`。
- 消费端 workflow 只更新固定 rev、lock 和 WebUI bindings，完整验证后创建包含新旧 SHA 的 PR，不自动合并。

## 性能基线

`cargo bench --bench maintainability` 覆盖 EventBridge 1k/10k 发布、回放、逐出及 Storage 10k message 追加、分页、恢复；先非阻塞记录，固定机器连续三次稳定后再设 20% 退化预警。
