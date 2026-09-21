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

## CI 与合并门禁

`.github/workflows/ci.yml` 的 required checks 名称为：

- `Linux quality`
- `Minimum Rust (1.85.0)`
- `Test (macos-latest)`
- `Test (windows-latest)`

core 的 `main` 应要求 PR 通过以上检查并与目标分支同步后再合并。消费端更新应在各自仓库完成完整锁定测试；跨仓库升级 PR 不应直接写入 `main`。

## 自动升级

发布 `v*` tag 时，`release-dispatch.yml` 会将 tag 和 commit SHA 发送给 TUI/WebUI。该 workflow 使用 core 仓库 secret `CORE_UPGRADE_TOKEN`；未配置时不跨仓库写入，消费端的每周 schedule 仍会检测 core `main` 并创建升级 PR。

消费端的 `core-upgrade.yml` 只更新固定 rev、Cargo.lock 和（WebUI）bindings，运行锁定验证后创建 PR，不自动合并。PR 正文必须列出旧 SHA、新 SHA、core commit 链接和验证结果。
