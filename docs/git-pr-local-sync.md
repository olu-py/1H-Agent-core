# Git 提交、PR 合并与本地同步流程

本文规定一次代码交付从本地修改到 GitHub 合并、再到本地默认分支对齐的**策略与完成标准**。`1H-Agent-core`、TUI、WebUI 是独立 Git 仓库；每次操作都必须先确认仓库根目录，不能把一个仓库的提交或推送误用于另一个仓库。

逐步命令、常见场景与停止条件见项目 Skill [`git-pr-local-sync`](../.agents/skills/git-pr-local-sync/SKILL.md)（也可调用 `$git-pr-local-sync`）；本仓库按该 Skill 执行，本文只补 core 特有的约束。GitHub 插件用于读取仓库、PR、评审和 CI 并执行获准的 GitHub 操作，它不会自动更新本地 checkout；提交、推送、拉取与工作区检查仍须在本地仓库完成。

## 完成标准

- 目标仓库和目标分支正确，改动经过检查并已提交。
- 对应远端分支包含该提交，PR 指向预期的目标分支。
- 所需 CI 与评审通过，PR 在 GitHub 上明确显示为已合并（closed 不等于 merged）。
- 本地默认分支已同步到远端默认分支，工作区状态已确认。
- 最终记录仓库、分支、PR、合并结果、本地与远端 SHA，以及未运行的检查。

## core 特有约束

- 涉及 core 和 TUI/WebUI 的变更必须按仓库分别完成、分别验证：先合并并同步 core，再在消费端更新 Git 依赖和 `Cargo.lock`；协议变更还要从锁定的 core checkout 同步 bindings/fixtures。完整约束见 [Core 发布与消费端更新指南](guides/release.md)。
- Cargo path patch 仅供本地联调；交付前移除 patch，确认锁文件来源是 Git，并以 `--locked` 验证。
- squash/rebase 合并会产生不同于 head 分支的合并 SHA：验收依据是本地默认分支与 `origin/<默认分支>` 对齐、PR 改动已在目标分支中，不要求功能分支原 SHA 相等。普通 `git branch -d` 因 squash/rebase 拒绝删除时不使用 `-D` 规避检查；远端分支是否自动删除由仓库设置决定。
- 同步失败、PR 冲突或工作区有不明改动时先查明原因并保护内容，不使用 `reset --hard`、`branch -D` 或 force push 作为同步捷径。

## 直推 main 的例外

默认仍走功能分支 + PR。仅当用户在当前任务中明确要求直推、且 diff 的每个文件都是 `*.md` 时，才允许直接 push `main`（`.github/workflows/main-guard.yml` 只放行 `*.md`；`src/`、`crates/`、`web/src/`、`Cargo.toml`/`Cargo.lock`、`.github/workflows/` 属保护路径，即使只改一行也要走 PR）：

```bash
pwsh -File scripts/push.ps1 -Branch main -AllowMain
```

推送前先 `git fetch` 并要求本地 `main` 等于 `origin/main`；推送后必须确认新 head 的 CI 结论，失败时用新的 revert 提交回滚（绝不 force），并在报告中给出旧 SHA、新 SHA 与 CI 结果。本地可用 `pwsh -File scripts/install-hooks.ps1` 启用 `.githooks/pre-push` 兜底。

## 异常处理与暂停条件

| 情况 | 处理 |
| --- | --- |
| 仓库、base/head 或默认分支不明 | 读取 remote、PR 和仓库默认分支；仍有歧义时暂停并给用户选项。 |
| 工作区有不明改动、同步可能覆盖本地内容 | 不切换、不清理、不重置；先识别并保护改动，无法确认时向用户询问。 |
| push 非快进、PR 冲突或 CI 失败 | fetch 并查明原因；修复后重新验证和更新 PR，不强推、不绕过 required checks。 |
| 直推 main 被 hook 或 CI 拒绝 | 说明 diff 里有非 `*.md` 文件或命中保护路径；改为功能分支 + PR，或先 revert 再交付。 |
| 需要强制覆盖、删除独有提交或绕过保护规则 | 停止并提供影响明确的选项，取得用户决定后再继续。 |

## 最终报告

简要列出仓库、功能分支与提交 SHA、PR 链接和合并状态、合并 SHA、本地默认分支与远端 SHA 是否一致、工作区是否干净、验证结果及未执行项。若任一完成条件未满足，明确标记为未完成及具体阻碍。