# Git 提交、PR 合并与本地同步流程

本文规定一次代码交付从本地修改到 GitHub 合并、再到本地默认分支对齐的完整步骤。`1H-Agent-core`、TUI、WebUI 是独立 Git 仓库；每次操作都必须先确认仓库根目录，不能把一个仓库的提交或推送误用于另一个仓库。

## 完成标准

一次交付只有在以下条件全部满足后才算完成：

- 目标仓库和目标分支正确，改动经过检查并已提交。
- 对应远端分支包含该提交，PR 指向预期的目标分支。
- 所有必需 CI 和评审通过，PR 在 GitHub 上明确显示为已合并。
- 本地默认分支已同步到远端默认分支，工作区状态已确认。
- 最终记录仓库、分支、PR、合并结果、本地 SHA 和未运行的检查。

GitHub 插件用于查看仓库、PR、评审和 CI，并执行获准的 GitHub 操作；它不会自动更新本地 checkout。提交、推送、拉取和工作区状态检查仍须在对应的本地仓库完成。

> AI agents: 在 Codex 支持项目 Skill 的会话中，执行 commit/push、PR 创建/合并或合并后同步任务时，可使用项目级 [`git-pr-local-sync`](../.agents/skills/git-pr-local-sync/SKILL.md) Skill，也可在 Codex 中直接调用 `$git-pr-local-sync`。若当前代理未发现或不支持 Skill，则按本文执行。

## 交付步骤

### 1. 确认仓库、分支和工作区

在预期项目目录执行：

```bash
git rev-parse --show-toplevel
git remote -v
git remote show origin
git status --short --branch
git branch --show-current
```

确认仓库 URL、当前分支、默认分支和任务目标一致。若工作区有未提交修改，先辨明哪些是本任务改动、哪些是用户已有内容；不覆盖、不重置、不擅自 stash 用户内容。无法确认归属时暂停并向用户列出处理选项。

### 2. 检查并提交改动

完成实现和适用的验证后，按仓库 `AGENTS.md` 选择相应验证，并检查未暂存与已暂存差异；未运行的检查要在最终报告列出：

```bash
git diff
git diff --check
git status --short
```

只暂存本任务文件，复核暂存区后提交：

```bash
git add <明确的文件路径>
git diff --cached
git diff --cached --check
git commit -m "简明描述本次改动"
git status --short --branch
git rev-parse HEAD
```

不要因方便而盲目使用 `git add -A`；若提交意外包含无关文件或密钥，先停止并修复暂存区，不要继续推送。

### 3. 推送并确认远端分支

首次推送该分支时建立 upstream；已有 upstream 时按仓库约定推送：

```bash
git push -u origin HEAD
```

推送后确认命令成功、分支对应关系正确，并比较本地与 upstream SHA：`git rev-parse HEAD`、`git rev-parse '@{u}'`。若 push 被拒绝，先 `git fetch origin` 并检查本地与远端提交关系；解决后重新检查差异和验证。不得用 `--force` 掩盖分歧；确需改写个人功能分支时，先确认分支未被他人依赖，再使用 `--force-with-lease`。共享分支或情况不明时暂停并提供选项。

### 4. 创建并检查 PR

创建 PR 前确认 head 仓库/分支、base 仓库/默认分支、标题和说明；检查 PR diff 与本地预期改动一致。PR 创建后逐项确认：

- base 与 head 正确，改动范围正确，没有意外文件。
- CI 和 required checks 全部成功，评审要求已满足。
- PR 没有冲突，GitHub 显示可合并；若不可合并，先查清原因。
- 合并策略符合仓库设置；不根据习惯自行假定 merge、squash 或 rebase。

CI 失败、评审有未解决意见、目标分支不确定或 diff 不一致时，不合并。冲突修复后重新验证、推送并检查 PR 更新后的 CI。

### 5. 合并 PR

确认上述检查都通过后，按仓库允许的方式合并。使用 GitHub 插件时先读取最新 PR 状态，再发起合并；权限不足、必需检查未满足或保护规则拒绝时，保留现状并报告具体阻碍，不绕过规则。

合并后重新读取 PR，确认 `merged` 为 true，并记录合并提交 SHA 和目标分支。PR 变为 closed 不等于已合并，必须核实合并状态。

### 6. 同步本地默认分支

在**同一仓库**中，确认没有待保留的工作区改动后执行；将 `main` 替换为仓库实际默认分支：

```bash
git fetch origin
git switch main
git pull --ff-only origin main
git status --short --branch
git rev-parse HEAD
git rev-parse origin/main
```

最后两个 SHA 必须相同，且 `git status` 显示工作区干净、本地分支与 `origin/main` 对齐。若切换或快进失败，先检查本地提交、未提交改动和远端变化；不要使用 `reset --hard` 或强制覆盖来“修同步”。保护现有内容后再决定如何收敛。

Squash 或 rebase 合并可能产生不同于 head 分支的合并 SHA；验收依据是本地默认分支与 `origin/<默认分支>` 对齐，且 PR 改动已在目标分支中，不要求功能分支原 SHA 等于合并 SHA。

只有在本地默认分支同步并验收后才清理本地功能分支。若普通 `git branch -d` 因 squash/rebase 拒绝删除，不使用 `-D` 规避检查；可保留分支，或确认无独有内容后再请用户选择清理方式。远端分支是否自动删除由仓库设置决定。

### 7. core 与消费端跨仓库交付

涉及 core 和 TUI/WebUI 的变更必须按仓库分别完成、分别验证：先合并并同步 core，再在消费端更新 Git 依赖和 `Cargo.lock`，协议变更还要从锁定的 core checkout 同步 bindings/fixtures，最后验证并单独提交消费端 PR。Cargo path patch 仅供本地联调；交付前移除 patch，确认锁文件来源是 Git，并以 `--locked` 验证。完整约束见 [Core 发布与消费端更新指南](guides/release.md)。

## 异常处理与暂停条件

| 情况 | 处理 |
| --- | --- |
| 仓库、base/head 或默认分支不明 | 读取 remote、PR 和仓库默认分支；仍有歧义时暂停并给用户选项。 |
| 工作区有不明改动、同步可能覆盖本地内容 | 不切换、不清理、不重置；先识别并保护改动，无法确认时向用户询问。 |
| push 非快进、PR 冲突或 CI 失败 | fetch 并查明原因；修复后重新验证和更新 PR，不强推、不绕过 required checks。 |
| PR 已关闭但合并状态不明 | 读取 PR 的 merged 状态和 merge SHA；未确认前不报告已完成。 |
| 合并成功但本地未对齐 | fetch、切换默认分支并尝试 `--ff-only`；失败时保留本地提交，查明分歧后再选方案。 |
| 需要强制覆盖、删除独有提交或绕过保护规则 | 停止并提供影响明确的选项，取得用户决定后再继续。 |

## 最终报告

简要列出仓库、功能分支与提交 SHA、PR 链接和合并状态、合并 SHA、本地默认分支与远端 SHA 是否一致、工作区是否干净、验证结果及未执行项。若任一完成条件未满足，明确标记为未完成及具体阻碍。
