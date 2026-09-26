# Provider 维护指南

## 适用范围

Provider 配置、密钥、请求协议、reasoning、`response_id`、上下文压缩和会话恢复——全部归 core 独占；消费端只经非密钥接口切换与展示。

## 入口

- 配置：`ProviderConfig`（稳定 `id` + 模板 `preset` + 可选 `name`/`enabled_models`）、`ProviderPreset`、`provider_for_id`/`provider_for`、`upsert_provider`、`remove_provider_by_id`（`src/config.rs`）。
- 元数据链：`src/model_meta.rs`（四层解析、models.dev 快照与解析、`GET /models` 解析、防投毒边界）、`OpenAiClient::list_models`（单次尝试、per-request 超时、1 MiB 体积上限）、`storage.rs` 的 `model_metadata` 缓存表、`AppHandle::provider_models(refresh)`（列表条目缺失字段按社区行精确键回填，不覆盖接口已报值）。
- 密钥：`api_key_cached*`、`store_api_key_cached`（core `secrets` facade），仅存在性/解锁入口暴露给消费端。
- 切换/编辑：消费端只经 `AppHandle::set_provider_profile`（provider id/空 id=新建 + 模板 + 可选 name + 模型 + 可选 base_url/kind/显式窗口/enabled_models，档案合并语义；窗口 clamp 到 Config 同界）/`set_provider`/`set_provider_config`/`remove_provider` 提交，不直接改配置；设置视图 `AppHandle::provider_settings()`（active/saved/connected，密钥永不入 DTO，connected 按 provider id 且为缓存级解析）；首页选择 `HomeSelection`/`apply_home_selection` 是 TUI 侧入口。
- 请求/恢复：`replay_safe_items`、请求游标、`src/provider/openai.rs`、`storage.rs` 的 Provider 状态。

## 不变量

- `Config.provider` 是当前连接；`Config.providers` 按稳定 `id` 唯一保存完整档案——内置四家仍各一份，自定义供应商可多份共存（`custom-<32 hex>`，显示名必填且 trim + 大小写不敏感去重，历史无名 custom 允许空名回退 preset 标签）。旧 `[provider]`/无 `id` 的旧档案在 `Config::load` 规范化为 `preset.key_id()` 后无损迁移（单一 custom 保持 `"custom"`），API Key 永不序列化。
- 非密钥配置按默认值 -> TOML -> 环境变量覆盖；模板只用 `ProviderPreset::defaults`，不得复制默认 URL。
- 启动只用 `api_key_cached` 解锁当前 Provider 一次，其他环境变量密钥可无交互预热（`preload_environment_keys_for_providers` 按 preset 家族查环境变量、按 id 入缓存）；不得遍历独立钥匙串条目。钥匙串账户名与进程缓存键均为 provider id：内置 id == preset key（向后兼容），自定义 id 各自独立。显式切换/编辑 Provider 可按需解锁一次，Agent 热路径只用 `api_key_cached_only`；新密钥通过 `store_api_key_cached` 同步钥匙串和内存。显式恢复/激活会话时按目标会话保存的 Provider 用 `api_key_cached` 解锁一次（`build_app`/`activate_session`），恢复后的 runtime 才拥有可用 runner。
- 消费端不读取 API Key 进模型上下文、不直接构造 Provider 请求、不解析私有 JSON/SSE；Provider 事件先规范化为公共 `ModelEvent`，再经 protocol 映射给消费端。
- 首页只复制按 id 去重的非密钥档案；仅 `StartNew` 将所选 Provider/模型/mode 应用到配置与新会话并按需解锁，`Resume` 仍恢复目标会话状态。
- 切换 Provider/模型必须重建 runner 并清理旧 `response_id` 与 usage anchor（`rebuild_runner` 内统一清锚）。增量游标从最新用户消息开始且保留其后 `@` 上下文。
- 容量预算（core 唯一权威）：窗口按元数据链解析——显式 `context_window_tokens` > 运行时发现（`GET {base_url}/models`，models.dev 社区库）> 内置 Provider 感知注册表；未知模型返回 `None`（不设默认窗口）。发现值只在 `[4096, 10_000_000]`（窗口）/`[1024, 131072]`（输出）界内接受，越界拒绝不 clamp；`ProviderConfig.discovered` 为运行时戳（`#[serde(skip)]`，不序列化），由 `stamp_discovered_meta` 从缓存重建并同步 live runner。发现型 `max_output_tokens` 只用于预算预留与展示，绝不注入请求体。拉取全部事件驱动（切换 Provider/模型、未知窗口的首次 submit、设置显式刷新），无轮询、冷启动零网络；`[model_metadata] fetch = false` 全程离线，TTL（1..=168h）只门控刷新尝试，缓存值在替换前一直可用；models.dev 请求不带密钥与用户数据。`max_output_tokens` 既是每请求输出硬上限（Responses 用 `max_output_tokens`、OpenAI chat 用 `max_completion_tokens`、其他 chat 用 `max_tokens`）也是输出预留。`safe_input_capacity = 窗口 − 输出预留 − 系统开销(4096)`；超窗先全轮压缩，失败再 hinted 硬裁并插入本地化提示，绝不静默预裁。token 计量唯一入口 `session::estimate_used_tokens`：最近一次成功全量重放请求的真实输入（与当次本地估算取 max，provider 少报不缩表）锚定前缀，其后追加项按校准系数（bytes/4 按真实用量/估算缩放，clamp 0.5..=2.0，仅全量重放轮采样）估增量；无锚或锚失效（会话被重写）回退整会话校准估算。锚在压缩完成、`/uncompact`、undo/redo（重建 runtime）与 Provider/模型切换时清除；`CompactionCompleted`/`Completed` 立即重计表，压缩后计量随之下调。
- 溢出恢复（agent 层）：provider 返回 400..=413 且消息命中已知上下文溢出措辞（`provider::is_context_overflow`，大小写不敏感白名单，识别不了原样失败绝不猜）时，执行全轮压缩，失败再 hinted 硬裁；仅当请求规模（校准估算）严格下降才重试——清 `response_id`、降级全量重放并复用 `AgentEvent::ProviderRetry { reason: "context overflow", delay_ms: 0 }`（零 wire 变更）。重试次数由 `compaction.max_overflow_retries`（clamp 0..=3，默认 1，0 关闭恢复）约束；无进度、重试耗尽或窗口未知时 `save_partial` 后原始错误权威。
- 压缩检查点和 `/uncompact` 都清理 `previous_response_id`；压缩摘要不得与旧服务端状态混用。
- 服务端状态失效后先清 ID，再用 `replay_safe_items` 重放；不得发送孤立 output 或无结果 call。
- 跨 Provider 子 Agent 按 provider id 解析（`ChildProviderResolver = Fn(&str)`，`agent_spawn` 的 `provider` 也可给已保存自定义供应商的 `name` 或旧 preset 名）；`session_provider_config` 先按 id 查 saved/active，再回退 `ProviderPreset::parse` 兼容旧行；子会话 `provider` 列写 id。
- `enabled_models` 本期只持久化并经 DTO 往返（空 = 不限），不参与运行时过滤，也不改动模型列表拉取路径；"真正使用的模型"勾选留作后续独立改动。
- DeepSeek Responses 不用 previous ID；原生搜索与同名本地 tool 互斥。
- Reasoning 事件按增量语义处理：空 content 不结束思考，done 的完整文本不重复追加；完成项 `summary` 仅在该流未收到任何思考增量时兜底（流级状态判定）。Qwen 3.7/3.8 字段按各协议隔离；custom 端点两种协议统一 `CompatibleAuto`，兼容全部已知思考增量事件形态（`reasoning_summary_text`/`reasoning_text`/`reasoning_content`/`reasoning` 的 `.delta`）。
- 诊断输出始终脱敏；HTTP 层指数退避重试仅在"未发出任何事件"的失败上生效（连接/发送阶段错误与 408/429/500/502/503/504）；流中断不重试，由 agent 层空输出重放兜底；`Retry-After` 优先并被 clamp 到 `retry_max_backoff_ms`。重试上限与退避参数来自 `ProviderConfig`（0 关闭）并 clamp。

## 诊断

| 症状 | 检查顺序 |
| --- | --- |
| orphan tool output 400 | 游标 -> call/output ID -> response ID -> replay 过滤 |
| 切换后配置回退 | profile -> active 副本 -> session provider/model -> runner rebuild |
| 重复钥匙串弹窗 | 热路径 key 查询 -> cache-only -> 缓存错误是否被错误重试 |
| 请求/SSE 400 | `ProviderKind` -> body/tool/thinking 字段 -> SSE 终态 |
| 请求失败但无重试 | `retry_max_attempts`/clamp -> 错误分类（`retry_delay`）-> 是否已发事件 |
| 上下文溢出未恢复 | `max_overflow_retries`（0 关闭）-> 错误措辞匹配（`is_context_overflow`）-> 窗口是否可解析 -> 进度门槛（估算须严格下降） |

## 验证

- 迭代过滤器：`config::tests`、`settings::tests`、`secrets::tests`、`provider::openai::tests`、`provider::tests`（重试决策）、`model_meta::tests`（解析链、字段形态、边界拒绝）。
- Agent 状态过滤器：`incremental_cursor_keeps_latest_user_message_and_following_context`、`stateless_replay_keeps_only_complete_ordered_tool_pairs`、`provider_retry_event_reaches_the_ui_channel`。
- 完成阶段按根文档运行一次 lib 测试；涉及存储恢复时升级到完整测试。
- 重试测试用 `OpenAiClient::scripted_with_failures`/`scripted_steps`（`Fail`/`Events`/`EventsThenFail`/`Models`）模拟"发出事件后再失败"的流中断与元数据结果，验证不重试防 delta 重放；集成测用 1ms 退避避免 flaky。步骤按请求顺序严格消费且 `scripted_with_failures` 把失败全部前置，"失败→成功→失败"的交错序列须用纯失败步骤组合表达（溢出恢复耗尽测试即以 500 步令压缩失败、由 trim 路径产生进展）。
- 新增 `ModelEvent` 变体需一次接通：`StreamCollector::on_event` 的 `other => Some(other)` 自动透传 → agent 主/子 forward 闭包显式分支（`Send`/`SendMany`（一事件展开为有序序列，如 `ReasoningCompleted`+`TextDelta`）/`SendIgnore`）→ 确认 `should_coalesce_stream_redraw` 是否需合并低频事件；跨层接线链见 UI Contract 专题。
